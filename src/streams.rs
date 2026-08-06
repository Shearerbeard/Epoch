//! Redesigned core stream surface (ADRs 0001-0005).
//!
//! Layer-1 typed-holes skeleton: the full type surface with `todo!()`
//! bodies, pending the E1 design panel. No behavior lands here until
//! the panel passes.

use std::convert::Infallible;
use std::fmt::Debug;

use thiserror::Error;

use crate::decider::Event;

/// Two-way typed contract between a consumer's stream id types and the
/// stored stream key (ADR 0004). The repository owns namespacing; an id
/// renders only its own identity.
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
    type ParseError = Infallible;

    fn stream_key(&self) -> String {
        todo!()
    }

    #[expect(unused_variables, reason = "todo!() body; filled by E1 post-panel")]
    fn parse_key(key: &str) -> Result<Self, Self::ParseError> {
        todo!()
    }
}

/// What a writer asserts about a stream's state on append (ADR 0003).
/// Write-side vocabulary only: a load can never report `Any` or
/// `StreamExists`, so those states are unrepresentable in
/// [`StreamVersion`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectedVersion<V> {
    /// No assertion; append regardless of stream state.
    Any,
    /// Assert the stream does not exist yet.
    NoStream,
    /// Assert the stream exists, at any version.
    StreamExists,
    /// Assert the stream is exactly at this version.
    Exact(V),
}

/// What the store reports about a stream's position (ADR 0003).
/// 1-based sequence semantics in every backend: an empty stream is
/// `NoStream`, and a stream's version after N events is sequence N.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StreamVersion<V> {
    /// The stream has no events.
    NoStream,
    /// The stream's last event sits at this 1-based sequence.
    Exact(V),
}

/// An observed position is usable as the expectation for the next
/// append: `NoStream` maps to `NoStream`, `Exact` to `Exact`.
impl<V> From<StreamVersion<V>> for ExpectedVersion<V> {
    #[expect(unused_variables, reason = "todo!() body; filled by E1 post-panel")]
    fn from(observed: StreamVersion<V>) -> Self {
        todo!()
    }
}

/// The single error surface for append outcomes (ADR 0003).
#[derive(Debug, Error)]
pub enum AppendError<V, E>
where
    V: Debug,
    E: std::error::Error,
{
    /// The optimistic version check failed: the writer's assertion did
    /// not hold against the stream's observed position.
    #[error("version conflict: expected {expected:?}, stream at {actual:?}")]
    VersionConflict {
        expected: ExpectedVersion<V>,
        actual: StreamVersion<V>,
    },
    /// The backend failed before the version check could decide.
    #[error(transparent)]
    Backend(#[from] E),
}

/// Versioned event streams over a typed id (ADRs 0002, 0003, 0004,
/// 0005). Native async-fn-in-trait; the attribute supplies the `Send`
/// bound generic consumers need to spawn futures. `append` takes
/// `&self`: the version check, not the receiver, is the concurrency
/// contract.
#[trait_variant::make(Send)]
pub trait EventStreams<E>
where
    E: Event + Send + Sync + Debug,
{
    /// Typed stream id; `String` works via its blanket [`StreamId`]
    /// impl.
    type Id: StreamId;
    /// Backend position type carried by [`StreamVersion`] and
    /// [`ExpectedVersion`].
    type Version: Send + Sync + Eq + Ord + Debug;
    /// Backend failure type wrapped by [`AppendError::Backend`].
    type Error: std::error::Error + Send + Sync;

    /// Load a stream's events and its observed position. `None` loads
    /// the repository's whole category. A missing stream answers
    /// `NoStream`, never an error.
    async fn load(
        &self,
        id: Option<&Self::Id>,
    ) -> Result<(Vec<E>, StreamVersion<Self::Version>), Self::Error>;

    /// Load events at positions after `from`, with the stream's
    /// observed position.
    async fn load_from_version(
        &self,
        from: &StreamVersion<Self::Version>,
        id: Option<&Self::Id>,
    ) -> Result<(Vec<E>, StreamVersion<Self::Version>), Self::Error>;

    /// Append `events` if `expected` holds, returning the appended
    /// events and the stream's new position.
    async fn append(
        &self,
        expected: ExpectedVersion<Self::Version>,
        stream: &Self::Id,
        events: &[E],
    ) -> Result<(Vec<E>, StreamVersion<Self::Version>), AppendError<Self::Version, Self::Error>>;
}
