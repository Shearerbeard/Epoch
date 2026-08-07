//! Redesigned core stream surface (ADRs 0001-0005).
//!
//! Layer-1 typed-holes skeleton: the full type surface with `todo!()`
//! bodies, pending the E1 design panel. No behavior lands here until
//! the panel passes.

use std::fmt::Debug;
use std::num::NonZeroU64;

use thiserror::Error;

use crate::decider::Event;

mod batch;
#[cfg(feature = "in_memory")]
pub mod in_memory;
#[cfg(feature = "postgres")]
pub mod postgres;
pub mod spec;

pub use batch::{
    AtomicStreams, Batch, BatchBuilder, BatchConflict, BatchConstraint, BatchWrite,
    ConstraintViolation, DuplicateWrite, StreamConstraint, StreamRef, TransactError,
    WritelessBatch,
};

/// Two-way typed contract between a consumer's stream id types and the
/// stored stream key (ADR 0004). The repository owns namespacing; an id
/// renders only its own identity. Round-tripping (`parse_key` accepts
/// what `stream_key` rendered) is a behavioral law on implementations,
/// pinned by the universal spec suite rather than the types.
pub trait StreamId: Sized + Send + Sync {
    /// Error for keys that do not round-trip into this id type.
    type ParseError: std::error::Error + Send + Sync + 'static;

    /// Render this id to its storage-key form.
    fn stream_key(&self) -> String;

    /// Parse a storage key back into a typed id, so category reads can
    /// return typed ids.
    fn parse_key(key: &str) -> Result<Self, Self::ParseError>;
}

/// `String` keeps working for tests and simple consumers (ADR 0004).
impl StreamId for String {
    type ParseError = std::convert::Infallible;

    fn stream_key(&self) -> String {
        self.clone()
    }

    fn parse_key(key: &str) -> Result<Self, Self::ParseError> {
        Ok(key.to_owned())
    }
}

/// A stream position under the 1-based sequence semantics every backend
/// shares (ADR 0003): a stream's version after N events is sequence N.
/// Zero is unrepresentable, so the 0-based empty/one-event ambiguity
/// that let two racers both win at `Exact(0)` cannot be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamSequence(NonZeroU64);

impl StreamSequence {
    /// Parse a raw backend position; zero is not a position.
    pub fn new(raw: u64) -> Result<Self, ZeroSequence> {
        NonZeroU64::new(raw).map(Self).ok_or(ZeroSequence)
    }

    /// The raw 1-based sequence number.
    pub fn get(self) -> u64 {
        self.0.get()
    }
}

/// Zero arrived where a 1-based stream position was required.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("zero is not a 1-based stream position")]
pub struct ZeroSequence;

/// What a writer asserts about a stream's state on append (ADR 0003).
/// Write-side vocabulary only: a load can never report `Any` or
/// `StreamExists`, so those states are unrepresentable in
/// [`StreamVersion`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectedVersion {
    /// No assertion; append regardless of stream state.
    Any,
    /// Assert the stream does not exist yet.
    NoStream,
    /// Assert the stream exists, at any version.
    StreamExists,
    /// Assert the stream is exactly at this position.
    Exact(StreamSequence),
}

/// What the store reports about a stream's position (ADR 0003).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamVersion {
    /// The stream has no events.
    NoStream,
    /// The stream's last event sits at this position.
    Exact(StreamSequence),
}

/// An observed position is usable as the expectation for the next
/// append: `NoStream` maps to `NoStream`, `Exact` to `Exact`.
impl From<StreamVersion> for ExpectedVersion {
    fn from(observed: StreamVersion) -> Self {
        match observed {
            StreamVersion::NoStream => ExpectedVersion::NoStream,
            StreamVersion::Exact(s) => ExpectedVersion::Exact(s),
        }
    }
}

/// One or more events to append. An empty append is meaningless, so it
/// is rejected at construction rather than reaching a backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventBatch<E>(Vec<E>);

impl<E> EventBatch<E> {
    /// Reject an empty batch at the boundary.
    pub fn new(events: Vec<E>) -> Result<Self, EmptyBatch> {
        if events.is_empty() {
            Err(EmptyBatch)
        } else {
            Ok(Self(events))
        }
    }

    /// The batched events, oldest first.
    pub fn as_slice(&self) -> &[E] {
        &self.0
    }

    /// Consume the batch.
    pub fn into_vec(self) -> Vec<E> {
        self.0
    }
}

/// An append was requested with no events in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("an event batch must contain at least one event")]
pub struct EmptyBatch;

/// A full stream load: either the stream does not exist, or it has at
/// least one event. Under ADR 0003's semantics a full load's version
/// IS its event count, so `Present` carries no separate position: the
/// one-event-at-sequence-99 state is structurally unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamState<E> {
    /// The stream has no events; loads answer this, never an error
    /// (ADR 0003).
    Missing,
    /// The stream exists: its full history, oldest first.
    Present(EventBatch<E>),
}

impl<E> StreamState<E> {
    /// The stream's position: `NoStream` when missing, otherwise the
    /// 1-based count of its history.
    pub fn version(&self) -> StreamVersion {
        match self {
            StreamState::Missing => StreamVersion::NoStream,
            StreamState::Present(batch) => {
                // EventBatch::new rejects empty batches, so a present
                // stream always holds at least one event and its count
                // is a valid 1-based sequence; the Err arm is unreachable.
                match StreamSequence::new(batch.0.len() as u64) {
                    Ok(seq) => StreamVersion::Exact(seq),
                    Err(ZeroSequence) => StreamVersion::NoStream,
                }
            }
        }
    }
}

/// An incremental read: the events at or after the requested position
/// (inclusive, matching the existing PostgreSQL and ESDB contract) and
/// the stream's observed position. Empty `events` with an `Exact`
/// position is valid (a cursor past the tail reads nothing); nonempty
/// events on a `NoStream` observation is rejected at construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamSlice<E> {
    events: Vec<E>,
    at: StreamVersion,
}

impl<E> StreamSlice<E> {
    /// Assemble a slice; rejects events claimed from a stream the
    /// observation says does not exist.
    pub fn new(events: Vec<E>, at: StreamVersion) -> Result<Self, MisshapenSlice> {
        if matches!(at, StreamVersion::NoStream) && !events.is_empty() {
            Err(MisshapenSlice)
        } else {
            Ok(Self { events, at })
        }
    }

    /// The events in the requested range, oldest first.
    pub fn events(&self) -> &[E] {
        &self.events
    }

    /// The stream position observed by the read.
    pub fn at(&self) -> StreamVersion {
        self.at
    }

    /// Consume the slice.
    pub fn into_parts(self) -> (Vec<E>, StreamVersion) {
        (self.events, self.at)
    }
}

/// Events were claimed from a stream whose observation says it does
/// not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("a slice cannot carry events from a stream observed as NoStream")]
pub struct MisshapenSlice;

/// One event of a category read, carrying the typed id of the stream
/// it belongs to (ADR 0004: category reads return typed ids). The
/// bounds are on the type so an unaddressable record cannot be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CategoryEvent<Id, E>
where
    Id: StreamId,
    E: Event,
{
    pub id: Id,
    pub event: E,
}

/// A failed optimistic version check: the writer's assertion against
/// the stream's observed position, in the shapes that can actually
/// conflict. Construction validates the pair, so impossible conflicts
/// (`Any` as the expectation, or an expectation the observation
/// satisfies) are rejected rather than represented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionConflict {
    expected: ExpectedVersion,
    actual: StreamVersion,
}

impl VersionConflict {
    /// Build a conflict from a failed check; rejects pairs that are
    /// not conflicts.
    pub fn new(expected: ExpectedVersion, actual: StreamVersion) -> Result<Self, NotAConflict> {
        let satisfied = match (expected, actual) {
            (ExpectedVersion::Any, _) => true,
            (ExpectedVersion::NoStream, StreamVersion::NoStream) => true,
            (ExpectedVersion::StreamExists, StreamVersion::Exact(_)) => true,
            (ExpectedVersion::Exact(a), StreamVersion::Exact(b)) => a == b,
            _ => false,
        };
        if satisfied {
            Err(NotAConflict)
        } else {
            Ok(Self { expected, actual })
        }
    }

    /// The writer's failed assertion.
    pub fn expected(self) -> ExpectedVersion {
        self.expected
    }

    /// The stream position the store observed.
    pub fn actual(self) -> StreamVersion {
        self.actual
    }
}

/// The expectation/observation pair satisfies the check; there is no
/// conflict to represent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("the expectation is satisfied by the observed position; not a conflict")]
pub struct NotAConflict;

/// The single error surface for append outcomes (ADR 0003).
#[derive(Debug, Error)]
pub enum AppendError<E>
where
    E: std::error::Error,
{
    /// The optimistic version check failed.
    #[error("version conflict: {0:?}")]
    Conflict(VersionConflict),
    /// The backend failed before the version check could decide.
    #[error(transparent)]
    Backend(#[from] E),
}

/// Failure surface for category reads, which parse stored keys back
/// into typed ids and so can fail two distinct ways a caller handles
/// differently: a backend fault (retryable) or a stored key that does
/// not round-trip (data/config defect).
#[derive(Debug, Error)]
pub enum LoadError<B, P>
where
    B: std::error::Error,
    P: std::error::Error,
{
    /// The backend failed.
    #[error(transparent)]
    Backend(B),
    /// A stored key did not parse into the typed id.
    #[error("stored stream key did not parse into the typed id: {0}")]
    InvalidKey(P),
}

/// Versioned event streams over a typed id (ADRs 0002, 0003, 0004,
/// 0005). Native async-fn-in-trait; the attribute rewrites the trait
/// so its futures carry the `Send` bound generic consumers need to
/// spawn them (no second trait is emitted). `append` takes `&self`:
/// the version check, not the receiver, is the concurrency contract.
#[trait_variant::make(Send)]
pub trait EventStreams<E>
where
    E: Event + Send + Sync + Debug,
{
    /// Typed stream id; `String` works via its concrete [`StreamId`]
    /// impl.
    type Id: StreamId;
    /// Backend failure type wrapped by [`AppendError::Backend`] and
    /// [`LoadError::Backend`].
    type Error: std::error::Error + Send + Sync;

    /// Load one stream in full. A missing stream is
    /// [`StreamState::Missing`], never an error.
    async fn load_stream(&self, id: &Self::Id) -> Result<StreamState<E>, Self::Error>;

    /// Load one stream's events at or after `from` (inclusive), with
    /// the stream's observed position. `None` reads from the start.
    async fn load_stream_from(
        &self,
        id: &Self::Id,
        from: Option<StreamSequence>,
    ) -> Result<StreamSlice<E>, Self::Error>;

    /// Load every event in this repository's category, oldest first,
    /// each carrying its typed stream id.
    async fn load_category(
        &self,
    ) -> Result<
        Vec<CategoryEvent<Self::Id, E>>,
        LoadError<Self::Error, <Self::Id as StreamId>::ParseError>,
    >;

    /// Append the batch if `expected` holds, returning the stream's
    /// new position.
    async fn append(
        &self,
        expected: ExpectedVersion,
        stream: &Self::Id,
        events: &EventBatch<E>,
    ) -> Result<StreamSequence, AppendError<Self::Error>>;
}
