//! Atomic multi-stream batches (ADR 0006): the capability trait, the
//! batch a backend commits, and the builder that assembles one.
//!
//! A batch is data. A write names a category, a typed stream id and its
//! events; nothing here carries a pool, a store or a connection, so the
//! only database a [`AtomicStreams::transact`] can reach is the handle's
//! own. The wire form `W` is the backend's, fixed by the builder each
//! handle hands out, which is what makes a batch backend-scoped without
//! branding the per-category stores.
//!
//! Two invariants are structural rather than checked at the backend: a
//! batch carries at least one write (a constraints-only batch is an
//! atomic read, and [`Batch`] has no other constructor than
//! [`BatchBuilder::build`]), and it carries at most one write per stream
//! (a second push for the same stream is [`DuplicateWrite`]), which is
//! what makes the pre-batch head the only head a write can be checked
//! against.

use std::fmt::{self, Display};
use std::time::Duration;

use thiserror::Error;

use super::{ExpectedVersion, StreamId, StreamSequence, StreamVersion, VersionConflict};

/// The stream a write or a constraint addresses: the owning category
/// plus the key the typed id rendered. The repository owns namespacing
/// (ADR 0004), so the pair is the whole address.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StreamRef {
    category: String,
    key: String,
}

impl StreamRef {
    /// Address a stream by its category and typed id.
    pub fn new<Id>(category: &str, id: &Id) -> Self
    where
        Id: StreamId,
    {
        Self {
            category: category.to_owned(),
            key: id.stream_key(),
        }
    }

    /// The owning category.
    pub fn category(&self) -> &str {
        &self.category
    }

    /// The stored key the typed id rendered.
    pub fn key(&self) -> &str {
        &self.key
    }
}

impl Display for StreamRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.category, self.key)
    }
}

/// What a batch asserts about a stream it need not write (ADR 0006).
/// Deliberately narrower than [`ExpectedVersion`]: a constraint cannot
/// restate `Any`, because an assertion that asserts nothing is not one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchConstraint {
    /// The stream has at least one event.
    StreamExists,
    /// The stream has no events.
    StreamDoesNotExist,
    /// The stream's head is exactly this position.
    StreamAt(StreamSequence),
}

impl BatchConstraint {
    /// Whether the constraint holds against an observed head.
    pub fn satisfied_by(self, observed: StreamVersion) -> bool {
        match self {
            Self::StreamExists => matches!(observed, StreamVersion::Exact(_)),
            Self::StreamDoesNotExist => matches!(observed, StreamVersion::NoStream),
            Self::StreamAt(sequence) => observed == StreamVersion::Exact(sequence),
        }
    }
}

/// Whether a write's expectation holds against an observed head.
pub(crate) fn expectation_satisfied(expected: ExpectedVersion, observed: StreamVersion) -> bool {
    match expected {
        ExpectedVersion::Any => true,
        ExpectedVersion::NoStream => matches!(observed, StreamVersion::NoStream),
        ExpectedVersion::StreamExists => matches!(observed, StreamVersion::Exact(_)),
        ExpectedVersion::Exact(sequence) => observed == StreamVersion::Exact(sequence),
    }
}

/// An assertion over a stream the batch does not have to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamConstraint {
    stream: StreamRef,
    constraint: BatchConstraint,
}

impl StreamConstraint {
    /// The constrained stream.
    pub fn stream(&self) -> &StreamRef {
        &self.stream
    }

    /// The assertion.
    pub fn constraint(&self) -> BatchConstraint {
        self.constraint
    }
}

/// One stream's contribution to a batch: its expectation against the
/// pre-batch head and its events, already in the backend's wire form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchWrite<W> {
    stream: StreamRef,
    expected: ExpectedVersion,
    events: Vec<W>,
}

impl<W> BatchWrite<W> {
    /// The written stream.
    pub fn stream(&self) -> &StreamRef {
        &self.stream
    }

    /// The writer's assertion about the stream's pre-batch head.
    pub fn expected(&self) -> ExpectedVersion {
        self.expected
    }

    /// The events to store, oldest first.
    pub fn events(&self) -> &[W] {
        &self.events
    }
}

/// A set of writes and constraints that commit as one unit, or not at
/// all. Built only through [`BatchBuilder::build`], so a batch always
/// carries at least one write and at most one write per stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch<W> {
    writes: Vec<BatchWrite<W>>,
    constraints: Vec<StreamConstraint>,
}

impl<W> Batch<W> {
    /// The batch's writes.
    pub fn writes(&self) -> &[BatchWrite<W>] {
        &self.writes
    }

    /// The batch's constraints.
    pub fn constraints(&self) -> &[StreamConstraint] {
        &self.constraints
    }

    /// Every stream the batch touches or constrains, deduplicated and
    /// in a deterministic order. This is the set a backend must hold
    /// under lock for the whole transaction.
    pub fn locked_streams(&self) -> Vec<&StreamRef> {
        let mut streams: Vec<&StreamRef> = self
            .writes
            .iter()
            .map(BatchWrite::stream)
            .chain(self.constraints.iter().map(StreamConstraint::stream))
            .collect();
        streams.sort_unstable();
        streams.dedup();
        streams
    }
}

/// Assembles a [`Batch`] in a backend's wire form. The typed push lives
/// with the backend that owns `W`, which is where events are erased -
/// fallibly, before any transaction starts (ADR 0006).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchBuilder<W> {
    writes: Vec<BatchWrite<W>>,
    constraints: Vec<StreamConstraint>,
}

impl<W> BatchBuilder<W> {
    /// An empty builder; a backend handle hands out the one bound to
    /// its own wire form.
    pub fn new() -> Self {
        Self {
            writes: Vec::new(),
            constraints: Vec::new(),
        }
    }

    /// Assert something about a stream, whether or not the batch writes
    /// it. The assertion is evaluated under the same lock as the writes.
    pub fn require<Id>(&mut self, category: &str, id: &Id, constraint: BatchConstraint) -> &mut Self
    where
        Id: StreamId,
    {
        self.constraints.push(StreamConstraint {
            stream: StreamRef::new(category, id),
            constraint,
        });
        self
    }

    /// Record an already-erased write. The backend's typed `write`
    /// method is the public route in; this is what it pushes through
    /// once the events are in `W`.
    pub(crate) fn push(
        &mut self,
        stream: StreamRef,
        expected: ExpectedVersion,
        events: Vec<W>,
    ) -> Result<(), DuplicateWrite> {
        if self.writes.iter().any(|write| write.stream == stream) {
            return Err(DuplicateWrite(stream));
        }
        self.writes.push(BatchWrite {
            stream,
            expected,
            events,
        });
        Ok(())
    }

    /// Close the batch.
    pub fn build(self) -> Result<Batch<W>, WritelessBatch> {
        if self.writes.is_empty() {
            return Err(WritelessBatch);
        }
        Ok(Batch {
            writes: self.writes,
            constraints: self.constraints,
        })
    }
}

impl<W> Default for BatchBuilder<W> {
    fn default() -> Self {
        Self::new()
    }
}

/// A second write was pushed for a stream the batch already writes.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("a batch admits at most one write per stream; {0} already has one")]
pub struct DuplicateWrite(pub StreamRef);

/// A batch was built with constraints but nothing to commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("a batch must carry at least one write")]
pub struct WritelessBatch;

/// A constraint was not satisfied by the stream's observed head.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("constraint {constraint:?} on {stream} is not satisfied by observed {observed:?}")]
pub struct ConstraintViolation {
    stream: StreamRef,
    constraint: BatchConstraint,
    observed: StreamVersion,
}

impl ConstraintViolation {
    pub(crate) fn new(
        stream: StreamRef,
        constraint: BatchConstraint,
        observed: StreamVersion,
    ) -> Self {
        Self {
            stream,
            constraint,
            observed,
        }
    }

    /// The stream the assertion was made about.
    pub fn stream(&self) -> &StreamRef {
        &self.stream
    }

    /// The failed assertion.
    pub fn constraint(&self) -> BatchConstraint {
        self.constraint
    }

    /// The head observed under the batch's lock.
    pub fn observed(&self) -> StreamVersion {
        self.observed
    }
}

/// A write's optimistic check failed, in the same expected-versus-actual
/// shape as the append conflict, plus the stream it failed on.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("version conflict on {stream}: expected {expected:?}, observed {observed:?}")]
pub struct BatchConflict {
    stream: StreamRef,
    expected: ExpectedVersion,
    observed: StreamVersion,
}

impl BatchConflict {
    pub(crate) fn new(
        stream: StreamRef,
        expected: ExpectedVersion,
        observed: StreamVersion,
    ) -> Self {
        Self {
            stream,
            expected,
            observed,
        }
    }

    /// The stream whose check failed.
    pub fn stream(&self) -> &StreamRef {
        &self.stream
    }

    /// The failed assertion and the head it was checked against, in the
    /// append conflict's shape.
    ///
    /// A `BatchConflict` is only built when the check just failed, so
    /// the pair is a genuine conflict and the error is unreachable.
    pub fn as_version_conflict(&self) -> VersionConflict {
        VersionConflict::new(self.expected, self.observed)
            .expect("a BatchConflict is built only from a failed check")
    }
}

/// A write carried an intent key the store already holds: the
/// saga-outbox uniqueness index rejected the duplicate (E20's typed
/// outcome, pinned by the saga card's final review). This is the
/// framework's redelivery signal, never a version conflict: a
/// redelivered source event re-appends its reactions, and storage
/// rejects the second append of an intent key. Only a confirmed
/// storage-level rejection is this outcome; the saga runner confirms
/// the minted keys are present on the outbox stream before acking the
/// no-op.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("duplicate intent rejected on {stream}")]
pub struct DuplicateIntent {
    stream: StreamRef,
}

impl DuplicateIntent {
    /// Build the outcome on a confirmed storage-level uniqueness
    /// rejection inside the outbox category. Crate-internal: only the
    /// backends construct it, so a consumer cannot mint a redelivery
    /// signal.
    #[expect(dead_code, reason = "constructed by the E20 postgres fill")]
    pub(crate) fn new(stream: StreamRef) -> Self {
        Self { stream }
    }

    /// The outbox stream whose write carried the duplicate key.
    pub fn stream(&self) -> &StreamRef {
        &self.stream
    }
}

/// How a [`AtomicStreams::transact`] can fail. A conflict and a
/// violated constraint each roll the whole batch back; a lock timeout
/// is its own retryable class, never reported as a version conflict.
#[derive(Debug, Error)]
pub enum TransactError<E>
where
    E: std::error::Error,
{
    /// A write's expectation failed against the pre-batch head.
    #[error(transparent)]
    Conflict(BatchConflict),
    /// A constraint was not satisfied.
    #[error(transparent)]
    ConstraintViolated(ConstraintViolation),
    /// A write carried an intent key the store already holds. The
    /// batch rolled back whole; the redelivered reaction is a no-op.
    /// A backend whose store does not enforce the key never produces
    /// this (the in-memory backend accepts duplicate intents in v1).
    #[error(transparent)]
    DuplicateIntent(DuplicateIntent),
    /// A lock could not be taken inside the batch's wait bound. The
    /// batch rolled back whole and the call is safe to retry.
    #[error("timed out after {0:?} waiting for the writer lock; retryable")]
    LockTimeout(Duration),
    /// The backend failed before the batch could be decided.
    #[error(transparent)]
    Backend(#[from] E),
}

/// Committing several streams under one consistency check (ADR 0006).
/// A capability, not a requirement: a backend that cannot provide it
/// does not implement it, and the missing impl is the compatibility
/// statement.
///
/// Implemented by the backend's database handle, the value that owns
/// the pool or the store root - never by a single per-category store,
/// whose scope is too narrow for a cross-category batch.
#[trait_variant::make(Send)]
pub trait AtomicStreams {
    /// The batch this backend commits, in its own wire form. Obtained
    /// from the handle's own builder, so a batch built for one backend
    /// cannot be handed to another.
    type Batch: Send;
    /// Backend failure type wrapped by [`TransactError::Backend`].
    type Error: std::error::Error + Send + Sync;

    /// Commit every write in the batch, or none of them: all locks are
    /// held for the life of the write, every constraint and every
    /// write-side expectation is evaluated against the locked
    /// pre-batch heads, and any failure leaves the store untouched.
    async fn transact(&self, batch: Self::Batch) -> Result<(), TransactError<Self::Error>>;
}

/// A handle that hands out its own batch builder: ADR 0006's
/// ownership seam extended to generic consumers such as the saga
/// runner, which assembles a batch without naming the backend's wire
/// form. The supertrait binding makes the builder's wire form and the
/// committed [`Batch`] the same `W`, so builder and `transact` always
/// agree on the wire form. What the type does NOT pin is handle
/// identity: two handles of one backend share a wire form, so which
/// database a batch commits against stays the caller's discipline,
/// exactly as on E3's concrete path.
pub trait BatchSource: AtomicStreams<Batch = Batch<Self::Wire>> {
    /// The backend's wire form, fixed by the builder this handle hands
    /// out.
    type Wire: Send;

    /// A fresh builder bound to this handle's wire form.
    fn builder(&self) -> BatchBuilder<Self::Wire>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(category: &str, key: &str) -> StreamRef {
        StreamRef::new(category, &key.to_owned())
    }

    #[test]
    fn a_batch_with_no_writes_is_not_admitted() {
        let mut builder = BatchBuilder::<u8>::new();
        builder.require("kids", &"k1".to_owned(), BatchConstraint::StreamExists);
        assert_eq!(builder.build(), Err(WritelessBatch));
    }

    #[test]
    fn a_second_write_for_one_stream_is_a_build_error() {
        let mut builder = BatchBuilder::<u8>::new();
        builder
            .push(stream("kids", "k1"), ExpectedVersion::Any, vec![1])
            .expect("the first write for a stream is admitted");
        assert_eq!(
            builder.push(stream("kids", "k1"), ExpectedVersion::Any, vec![2]),
            Err(DuplicateWrite(stream("kids", "k1")))
        );
    }

    #[test]
    fn the_same_key_in_two_categories_is_two_streams() {
        let mut builder = BatchBuilder::<u8>::new();
        builder
            .push(stream("kids", "shared"), ExpectedVersion::Any, vec![1])
            .expect("first category");
        builder
            .push(stream("chores", "shared"), ExpectedVersion::Any, vec![2])
            .expect("a different category is a different stream");
        assert_eq!(builder.build().expect("two writes").writes().len(), 2);
    }

    #[test]
    fn locked_streams_dedups_writes_against_constraints() {
        let mut builder = BatchBuilder::<u8>::new();
        builder
            .push(stream("kids", "k1"), ExpectedVersion::Any, vec![1])
            .expect("write");
        builder
            .push(stream("chores", "c1"), ExpectedVersion::Any, vec![2])
            .expect("write");
        builder.require("kids", &"k1".to_owned(), BatchConstraint::StreamExists);
        builder.require(
            "pool",
            &"p1".to_owned(),
            BatchConstraint::StreamDoesNotExist,
        );

        let batch = builder.build().expect("two writes");
        let locked: Vec<String> = batch
            .locked_streams()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(locked, ["chores/c1", "kids/k1", "pool/p1"]);
    }

    #[test]
    fn constraints_read_the_head_the_way_append_does() {
        let one = StreamSequence::new(1).expect("1 is a position");
        assert!(BatchConstraint::StreamDoesNotExist.satisfied_by(StreamVersion::NoStream));
        assert!(!BatchConstraint::StreamExists.satisfied_by(StreamVersion::NoStream));
        assert!(BatchConstraint::StreamExists.satisfied_by(StreamVersion::Exact(one)));
        assert!(BatchConstraint::StreamAt(one).satisfied_by(StreamVersion::Exact(one)));
        assert!(!BatchConstraint::StreamAt(one).satisfied_by(StreamVersion::NoStream));
    }
}
