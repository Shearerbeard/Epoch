//! In-memory [`EventStreams`] backend on the unified 1-based sequence
//! semantics (ADR 0003) and the shared-store `Clone` contract
//! (ADR 0005): clones share one store, so concurrent writers contend on
//! the same version check instead of diverging into private copies.
//!
//! Every store is a category view onto an [`InMemoryDatabase`] root: one
//! oldest-first log behind one mutex, holding every category the root
//! knows. That is the in-memory equivalent of the postgres pool
//! (ADR 0006's ownership seam) - an atomic batch spans categories whose
//! event types differ, so the log stores payloads erased behind `Any`
//! and each category's event type is claimed the first time the
//! category is opened or written. Because a store and a batch commit
//! through the same mutex, an append and a batch on one stream
//! serialize against each other exactly as they do on postgres.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::fmt::{self, Debug};
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use thiserror::Error;

use crate::decider::Event;

use super::{
    AppendError, CategoryEvent, EventBatch, EventMetadata, EventStreams, ExpectedVersion,
    LoadError, StreamId, StreamSequence, StreamSlice, StreamState, StreamVersion, VersionConflict,
};

mod batch;
mod feed;

pub use batch::{InMemoryBatch, InMemoryBatchBuilder, MemoryEvent};
pub use feed::InMemoryEventFeed;

/// The category a standalone [`InMemoryEventStreams`] opens on its own
/// private root.
const STANDALONE_CATEGORY: &str = "in_memory";

/// A payload erased for the shared root, so one log can hold categories
/// whose event types differ.
pub(crate) type ErasedEvent = Arc<dyn Any + Send + Sync>;

/// One stored event and the stream it belongs to.
struct StoredEvent {
    category: String,
    key: String,
    payload: ErasedEvent,
    metadata: EventMetadata,
}

/// The shared store root: one oldest-first log, the event type each
/// category is claimed with, and each feed group's progress.
#[derive(Default)]
pub(crate) struct Root {
    log: Vec<StoredEvent>,
    types: HashMap<String, TypeId>,
    cursors: HashMap<(String, String), feed::GroupProgress>,
}

impl Root {
    /// A stream's head under ADR 0003's semantics: the count of its
    /// stored events, zero for a stream that does not exist.
    pub(crate) fn head(&self, category: &str, key: &str) -> StreamVersion {
        let count = self
            .log
            .iter()
            .filter(|stored| stored.category == category && stored.key == key)
            .count() as u64;
        version_of(count)
    }

    /// Whether this category can hold `event_type`, without claiming it.
    pub(crate) fn admits(
        &self,
        category: &str,
        event_type: TypeId,
    ) -> Result<(), CategoryTypeMismatch> {
        match self.types.get(category) {
            Some(claimed) if *claimed != event_type => Err(CategoryTypeMismatch {
                category: category.to_owned(),
            }),
            Some(_) | None => Ok(()),
        }
    }

    /// Fix this category's event type, or confirm the one it has.
    pub(crate) fn claim(
        &mut self,
        category: &str,
        event_type: TypeId,
    ) -> Result<(), CategoryTypeMismatch> {
        self.admits(category, event_type)?;
        self.types.insert(category.to_owned(), event_type);
        Ok(())
    }

    /// Store one event at the tail of the log. The caller has already
    /// claimed the category and checked the stream's head.
    pub(crate) fn append_erased(
        &mut self,
        category: &str,
        key: &str,
        payload: ErasedEvent,
        metadata: EventMetadata,
    ) {
        self.log.push(StoredEvent {
            category: category.to_owned(),
            key: key.to_owned(),
            payload,
            metadata,
        });
    }

    /// One feed group's progress on one category; absent means
    /// nothing acknowledged, nothing delivered.
    pub(crate) fn feed_progress(&self, category: &str, group: &str) -> feed::GroupProgress {
        self.cursors
            .get(&(category.to_owned(), group.to_owned()))
            .copied()
            .unwrap_or_default()
    }

    /// Record that a poll delivered through `position`. The watermark
    /// only moves forward.
    pub(crate) fn record_delivery(&mut self, category: &str, group: &str, position: u64) {
        let progress = self
            .cursors
            .entry((category.to_owned(), group.to_owned()))
            .or_default();
        progress.delivered_to = progress.delivered_to.max(position);
    }

    /// Advance a group's cursor. Callers validate the move first: the
    /// position exceeds the current cursor and lies within delivery.
    pub(crate) fn advance_cursor(&mut self, category: &str, group: &str, to: u64) {
        self.cursors
            .entry((category.to_owned(), group.to_owned()))
            .or_default()
            .cursor = to;
    }
}

impl Debug for Root {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Root")
            .field("events", &self.log.len())
            .field("categories", &self.types.len())
            .finish()
    }
}

/// A category was opened or written with an event type other than the
/// one it already holds.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("category {category} already holds events of a different type")]
pub struct CategoryTypeMismatch {
    /// The category whose claimed event type was contradicted.
    pub category: String,
}

/// The in-memory database handle: the store root every category view
/// and every batch commits through (ADR 0006's ownership seam). Clones
/// share the root, exactly as ADR 0005 requires of the stores.
#[derive(Debug, Clone, Default)]
pub struct InMemoryDatabase {
    root: Arc<Mutex<Root>>,
}

impl InMemoryDatabase {
    pub fn new() -> Self {
        Self::default()
    }

    /// A category view over this root, fixing the category's event type
    /// if nothing has claimed it yet.
    pub fn category<E>(&self, name: &str) -> Result<InMemoryEventStreams<E>, CategoryTypeMismatch>
    where
        E: Send + Sync + 'static,
    {
        self.root
            .lock()
            .expect("event store lock poisoned")
            .claim(name, TypeId::of::<E>())?;
        Ok(InMemoryEventStreams {
            root: Arc::clone(&self.root),
            category: name.to_owned(),
            _marker: PhantomData,
        })
    }

    /// A builder for a batch this handle can commit.
    pub fn batch(&self) -> InMemoryBatchBuilder {
        InMemoryBatchBuilder::new()
    }
}

/// One category of streams held in memory, addressed by a stream key.
/// A stream's version IS its event count, and the append version check
/// is atomic with the write because both happen under the root's lock.
pub struct InMemoryEventStreams<E> {
    root: Arc<Mutex<Root>>,
    category: String,
    _marker: PhantomData<fn() -> E>,
}

impl<E> InMemoryEventStreams<E>
where
    E: Send + Sync + 'static,
{
    /// A store on its own private root. Use
    /// [`InMemoryDatabase::category`] instead when the store has to
    /// share a root with other categories, which an atomic batch
    /// requires.
    pub fn new() -> Self {
        InMemoryDatabase::new()
            .category(STANDALONE_CATEGORY)
            .expect("a fresh root has no claimed category to contradict")
    }
}

/// Shares the store (ADR 0005); never snapshots it. Hand-written so the
/// derive's spurious `E: Clone` bound is not required of callers.
impl<E> Clone for InMemoryEventStreams<E> {
    fn clone(&self) -> Self {
        Self {
            root: Arc::clone(&self.root),
            category: self.category.clone(),
            _marker: PhantomData,
        }
    }
}

/// Hand-written for the same reason as `Clone`: no `E: Default` bound.
impl<E> Default for InMemoryEventStreams<E>
where
    E: Send + Sync + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

/// Hand-written because the erased payloads in the root are not `Debug`.
impl<E> Debug for InMemoryEventStreams<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InMemoryEventStreams")
            .field("category", &self.category)
            .finish_non_exhaustive()
    }
}

/// The in-memory store has no backend failure mode, so its error type
/// is uninhabited: [`AppendError::Backend`] and [`LoadError::Backend`]
/// are unrepresentable for this implementation.
#[derive(Debug, Error)]
pub enum InMemoryError {}

/// A count of stored events as the observed position: zero events is
/// `NoStream`, N events is sequence N.
fn version_of(count: u64) -> StreamVersion {
    match StreamSequence::new(count) {
        Ok(sequence) => StreamVersion::Exact(sequence),
        Err(super::ZeroSequence) => StreamVersion::NoStream,
    }
}

/// The category's claimed event type makes this a corruption assertion
/// rather than a runtime branch: nothing else can have been stored.
fn stored_event<E>(payload: &ErasedEvent) -> E
where
    E: Clone + 'static,
{
    payload
        .downcast_ref::<E>()
        .expect("a category holds exactly the event type it is claimed with")
        .clone()
}

impl<E> EventStreams<E> for InMemoryEventStreams<E>
where
    E: Event + Clone + Send + Sync + Debug + 'static,
{
    type Id = String;
    type Error = InMemoryError;

    async fn load_stream(&self, id: &Self::Id) -> Result<StreamState<E>, Self::Error> {
        let root = self.root.lock().expect("event store lock poisoned");
        let key = id.stream_key();
        let events: Vec<E> = root
            .log
            .iter()
            .filter(|stored| stored.category == self.category && stored.key == key)
            .map(|stored| stored_event(&stored.payload))
            .collect();

        if events.is_empty() {
            Ok(StreamState::Missing)
        } else {
            // The nonempty invariant was just checked above, so the
            // empty-batch error is unreachable.
            Ok(StreamState::Present(
                EventBatch::new(events).expect("events were checked nonempty"),
            ))
        }
    }

    async fn load_stream_from(
        &self,
        id: &Self::Id,
        from: Option<StreamSequence>,
    ) -> Result<StreamSlice<E>, Self::Error> {
        let root = self.root.lock().expect("event store lock poisoned");
        let key = id.stream_key();
        let history: Vec<E> = root
            .log
            .iter()
            .filter(|stored| stored.category == self.category && stored.key == key)
            .map(|stored| stored_event(&stored.payload))
            .collect();

        let at = version_of(history.len() as u64);
        // `from` is 1-based and inclusive; a cursor past the tail reads
        // nothing.
        let start = from.map_or(0, |sequence| (sequence.get() - 1) as usize);
        let events = if start >= history.len() {
            Vec::new()
        } else {
            history[start..].to_vec()
        };

        // The pair is consistent by construction: nonempty events only
        // come from a nonempty history, whose observation is `Exact`,
        // so the misshapen-slice error is unreachable.
        Ok(StreamSlice::new(events, at).expect("nonempty events imply an Exact observation"))
    }

    async fn load_category(
        &self,
    ) -> Result<
        Vec<CategoryEvent<Self::Id, E>>,
        LoadError<Self::Error, <Self::Id as StreamId>::ParseError>,
    > {
        let root = self.root.lock().expect("event store lock poisoned");
        root.log
            .iter()
            .filter(|stored| stored.category == self.category)
            .map(|stored| {
                let id = Self::Id::parse_key(&stored.key).map_err(LoadError::InvalidKey)?;
                Ok(CategoryEvent {
                    id,
                    event: stored_event(&stored.payload),
                })
            })
            .collect()
    }

    async fn append(
        &self,
        expected: ExpectedVersion,
        stream: &Self::Id,
        events: &EventBatch<E>,
    ) -> Result<StreamSequence, AppendError<Self::Error>> {
        let mut root = self.root.lock().expect("event store lock poisoned");
        let key = stream.stream_key();
        let observed = root.head(&self.category, &key);

        if !super::batch::expectation_satisfied(expected, observed) {
            // The pair is a genuine conflict because it is only built
            // when the check just failed, so the not-a-conflict error
            // is unreachable.
            return Err(AppendError::Conflict(
                VersionConflict::new(expected, observed)
                    .expect("the expectation just failed against the observation"),
            ));
        }

        let count = match observed {
            StreamVersion::NoStream => 0,
            StreamVersion::Exact(sequence) => sequence.get(),
        };
        for record in events.records() {
            root.append_erased(
                &self.category,
                &key,
                Arc::new(record.event().clone()),
                record.metadata().clone(),
            );
        }
        let position = count + events.records().len() as u64;
        Ok(StreamSequence::new(position).expect("a batch holds at least one event"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streams::RecordedEvent;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Noted;

    impl Event for Noted {
        type EntityId = ();

        fn event_type(&self) -> String {
            "Noted".to_owned()
        }

        fn get_id(&self) -> Self::EntityId {}
    }

    /// The envelope written at append is the envelope stored: bare
    /// events store the empty map, keyed events store their keys.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_envelope_written_is_the_envelope_stored() {
        let db = InMemoryDatabase::new();
        let store = db.category::<Noted>("envelope").expect("fresh category");

        let mut envelope = EventMetadata::new();
        envelope.insert("intent", "saga-1/order-5/2");
        let batch = EventBatch::from_records(vec![
            RecordedEvent::new(Noted),
            RecordedEvent::keyed(Noted, envelope),
        ])
        .expect("two events are nonempty");
        store
            .append(ExpectedVersion::NoStream, &"s".to_owned(), &batch)
            .await
            .expect("append succeeds");

        let root = db.root.lock().expect("event store lock poisoned");
        let stored: Vec<&StoredEvent> = root
            .log
            .iter()
            .filter(|stored| stored.category == "envelope")
            .collect();
        assert_eq!(stored.len(), 2);
        assert!(stored[0].metadata.is_empty(), "a bare event stores no keys");
        assert_eq!(
            stored[1].metadata.get("intent"),
            Some("saga-1/order-5/2"),
            "a keyed event stores its envelope"
        );
    }
}
