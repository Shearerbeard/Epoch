//! In-memory [`EventStreams`] backend on the unified 1-based sequence
//! semantics (ADR 0003) and the shared-store `Clone` contract
//! (ADR 0005): clones share one store, so concurrent writers contend on
//! the same version check instead of diverging into private copies.

use std::fmt::Debug;
use std::sync::{Arc, Mutex};

use thiserror::Error;

use crate::decider::Event;

use super::{
    AppendError, CategoryEvent, EventBatch, EventStreams, ExpectedVersion, LoadError, StreamId,
    StreamSequence, StreamSlice, StreamState, StreamVersion, VersionConflict,
};

/// One category of streams held in memory: a single oldest-first log of
/// `(stream key, event)` pairs behind one lock, so a stream's version
/// IS its event count and the append version check is atomic with the
/// write.
#[derive(Debug)]
pub struct InMemoryEventStreams<E> {
    log: Arc<Mutex<Vec<(String, E)>>>,
}

impl<E> InMemoryEventStreams<E> {
    pub fn new() -> Self {
        Self {
            log: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

/// Shares the store (ADR 0005); never snapshots it. Hand-written so the
/// derive's spurious `E: Clone` bound is not required of callers.
impl<E> Clone for InMemoryEventStreams<E> {
    fn clone(&self) -> Self {
        Self {
            log: Arc::clone(&self.log),
        }
    }
}

/// Hand-written for the same reason as `Clone`: no `E: Default` bound.
impl<E> Default for InMemoryEventStreams<E> {
    fn default() -> Self {
        Self::new()
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

impl<E> EventStreams<E> for InMemoryEventStreams<E>
where
    E: Event + Clone + Send + Sync + Debug,
{
    type Id = String;
    type Error = InMemoryError;

    async fn load_stream(&self, id: &Self::Id) -> Result<StreamState<E>, Self::Error> {
        let log = self.log.lock().expect("event store lock poisoned");
        let key = id.stream_key();
        let events: Vec<E> = log
            .iter()
            .filter(|(stored_key, _)| *stored_key == key)
            .map(|(_, event)| event.clone())
            .collect();

        if events.is_empty() {
            Ok(StreamState::Missing)
        } else {
            // `EventBatch::new` is an E1 hole outside E2's fill bound;
            // crate-internal construction is valid here because the
            // nonempty invariant was just checked.
            Ok(StreamState::Present(EventBatch(events)))
        }
    }

    async fn load_stream_from(
        &self,
        id: &Self::Id,
        from: Option<StreamSequence>,
    ) -> Result<StreamSlice<E>, Self::Error> {
        let log = self.log.lock().expect("event store lock poisoned");
        let key = id.stream_key();
        let history: Vec<E> = log
            .iter()
            .filter(|(stored_key, _)| *stored_key == key)
            .map(|(_, event)| event.clone())
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

        // `StreamSlice::new` is an E1 hole outside E2's fill bound; the
        // pair is consistent by construction (events only come from a
        // nonempty history, whose observation is `Exact`).
        Ok(StreamSlice { events, at })
    }

    async fn load_category(
        &self,
    ) -> Result<
        Vec<CategoryEvent<Self::Id, E>>,
        LoadError<Self::Error, <Self::Id as StreamId>::ParseError>,
    > {
        let log = self.log.lock().expect("event store lock poisoned");
        log.iter()
            .map(|(stored_key, event)| {
                let id = Self::Id::parse_key(stored_key).map_err(LoadError::InvalidKey)?;
                Ok(CategoryEvent {
                    id,
                    event: event.clone(),
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
        let mut log = self.log.lock().expect("event store lock poisoned");
        let key = stream.stream_key();
        let count = log
            .iter()
            .filter(|(stored_key, _)| *stored_key == key)
            .count() as u64;

        let satisfied = match expected {
            ExpectedVersion::Any => true,
            ExpectedVersion::NoStream => count == 0,
            ExpectedVersion::StreamExists => count > 0,
            ExpectedVersion::Exact(sequence) => sequence.get() == count,
        };
        if !satisfied {
            // `VersionConflict::new` is an E1 hole outside E2's fill
            // bound; the pair is a genuine conflict because it is only
            // built when the check just failed.
            return Err(AppendError::Conflict(VersionConflict {
                expected,
                actual: version_of(count),
            }));
        }

        log.extend(events.0.iter().map(|event| (key.clone(), event.clone())));
        let position = count + events.0.len() as u64;
        Ok(StreamSequence::new(position).expect("a batch holds at least one event"))
    }
}
