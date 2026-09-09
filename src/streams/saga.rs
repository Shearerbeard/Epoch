//! The saga runner (E20; ADR 0010's outbox-saga section, landing as
//! that record's gate-A revision): a consumer's pure reaction folded
//! into ONE atomic batch append, with the feed-cursor ack as a
//! separate, explicit step after the append succeeds.
//!
//! A [`Saga`] maps one delivered source event to reactions:
//! [`Reaction::Command`] arms append events to their target streams,
//! [`Reaction::EffectRequest`] arms record effect intents on the
//! saga's outbox stream (the outbox school: effects as facts, never
//! performed inline). The runner folds every reaction of one source
//! event into a single `AtomicStreams::transact` - commands merged to
//! one write per stream, intents merged to one write on the outbox
//! stream, as ADR 0006 requires of a batch.
//!
//! The append is crash-atomic, but the window between it and the ack
//! is real: a crash there redelivers the source event, and the
//! re-run's reactions re-append. Reaction identity closes the window:
//! the runner mints a deterministic [`IntentKey`] per reaction -
//! (saga id, source stream, source position, reaction index) - and
//! storage rejects the second append of an intent key as a typed
//! [`DuplicateIntent`], which the runner classifies as the no-op it
//! is (after confirming the minted keys are present on the outbox
//! stream) and acks. A version conflict on a command arm is a real
//! conflict and surfaces; retryable aborts
//! (`TransactError::LockTimeout`) and indeterminate failures are
//! retried under the runner's [`RetryPolicy`] and then propagated,
//! never classified as conflicts.
//!
//! Every write the runner makes goes through the atomic batch, so the
//! single-writer funnel's operating assumptions (ADR 0010, and the
//! postgres module doc) bind deployments unchanged.

#![allow(dead_code)] // E20 skeleton: removed slice by slice as the holes fill.

use std::fmt;
use std::num::NonZeroU32;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::batch::{BatchBuilder, BatchConflict, ConstraintViolation, StreamRef};
use super::feed::{AckError, ConsumerGroup, EventFeed, FeedPosition, PollLimit};
use super::outbox::{OutboxEvent, OUTBOX_CATEGORY};
use super::{BatchSource, EventMetadata, EventStreams, ExpectedVersion};

/// A saga's identity: the runner's consumer-group name, the saga's
/// outbox stream key, and the first component of every intent key the
/// runner mints for it. Non-empty, because an anonymous saga cannot be
/// told apart from a forgotten argument.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SagaId(String);

impl SagaId {
    /// Parse a saga id; the empty string is not one.
    pub fn new(id: impl Into<String>) -> Result<Self, EmptySagaId> {
        let id = id.into();
        if id.is_empty() {
            Err(EmptySagaId)
        } else {
            Ok(Self(id))
        }
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A saga id was constructed with the empty string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("a saga id must not be empty")]
pub struct EmptySagaId;

/// A reaction's position within one `react` output: the index that
/// distinguishes two reactions of the same source event from each
/// other. Zero-based, matching the vector the saga returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReactionIndex(u64);

impl ReactionIndex {
    pub(crate) fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// The zero-based position.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// The deterministic identity of one reaction append (the card's
/// reaction-identity pin): the saga, the source event's stream, the
/// source event's committed position, and the reaction's index in that
/// event's `react` output. A redelivered source event re-mints exactly
/// the same keys, which is what makes storage's duplicate rejection a
/// no-op signal rather than a failure.
///
/// The spec's "source sequence" is realized as the source entry's
/// committed-log position: the feed delivers positions, not per-stream
/// sequences, and the position is unique per source event and stable
/// under redelivery.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IntentKey {
    saga: SagaId,
    source: StreamRef,
    position: FeedPosition,
    index: ReactionIndex,
}

impl IntentKey {
    /// Mint the key for one reaction of one delivered source event.
    /// The runner mints in production; the constructor is public so
    /// conformance tests can pin the rendering.
    pub fn mint(
        saga: &SagaId,
        source: StreamRef,
        position: FeedPosition,
        index: ReactionIndex,
    ) -> Self {
        Self {
            saga: saga.clone(),
            source,
            position,
            index,
        }
    }

    /// The saga the reaction belongs to.
    pub fn saga(&self) -> &SagaId {
        &self.saga
    }

    /// The source event's stream.
    pub fn source(&self) -> &StreamRef {
        &self.source
    }

    /// The source event's committed position.
    pub fn position(&self) -> FeedPosition {
        self.position
    }

    /// The reaction's index in the source event's `react` output.
    pub fn index(&self) -> ReactionIndex {
        self.index
    }

    /// Render the key to its canonical envelope form. The rule: inside
    /// every component `\` renders as `\\` and `/` as `\/`, then the
    /// components join with `/` as `saga/category/key/position/index`.
    /// Escaping makes the rendering injective: two distinct keys never
    /// render equal, so a duplicate rejection can never be a false
    /// positive.
    pub fn render(&self) -> RenderedIntentKey {
        RenderedIntentKey::from_rendered(format!(
            "{}/{}/{}/{}/{}",
            escape_key_component(self.saga.as_str()),
            escape_key_component(self.source.category()),
            escape_key_component(self.source.key()),
            self.position.get(),
            self.index.get(),
        ))
    }
}

/// Escape one intent-key component under the documented rule.
fn escape_key_component(component: &str) -> String {
    component.replace('\\', "\\\\").replace('/', "\\/")
}

/// An intent key in its canonical rendered form: the string that rides
/// the `intent` envelope key and the outbox outcome payloads. Compared
/// bytewise, never parsed - the structured form is [`IntentKey`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RenderedIntentKey(String);

impl RenderedIntentKey {
    /// The rendered text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Wrap an already-rendered key. [`IntentKey::render`] is the only
    /// production source; stored facts are trusted at the read
    /// boundary, as any stored payload is.
    pub(crate) fn from_rendered(rendered: String) -> Self {
        Self(rendered)
    }
}

impl fmt::Display for RenderedIntentKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What one source event produces: commands to append to target
/// streams, and effect requests to record as intents on the saga's
/// outbox stream (the outbox school - effects as facts, never
/// performed inside the reaction).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reaction<C, F> {
    /// Append events to a target stream.
    Command(Command<C>),
    /// Record an effect intent for the outbox executor.
    EffectRequest(EffectRequest<F>),
}

/// A command reaction: events destined for one target stream, with the
/// writer's expectation against that stream's head. The payload is the
/// consumer's; the runner attaches the reaction's intent key as event
/// metadata at fold time, so consumers dedupe positionally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command<C> {
    stream: StreamRef,
    expected: ExpectedVersion,
    payload: C,
}

impl<C> Command<C> {
    /// Address a command at a stream with an expectation.
    pub fn new(stream: StreamRef, expected: ExpectedVersion, payload: C) -> Self {
        Self {
            stream,
            expected,
            payload,
        }
    }

    /// The target stream.
    pub fn stream(&self) -> &StreamRef {
        &self.stream
    }

    /// The expectation against the stream's pre-batch head.
    pub fn expected(&self) -> ExpectedVersion {
        self.expected
    }

    /// The consumer's command payload.
    pub fn payload(&self) -> &C {
        &self.payload
    }
}

/// An effect request: the payload the outbox executor later performs
/// through the consumer's port. Ordered at-least-once is the
/// documented delivery contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectRequest<F> {
    payload: F,
}

impl<F> EffectRequest<F> {
    /// Wrap an effect payload.
    pub fn new(payload: F) -> Self {
        Self { payload }
    }

    /// The effect payload.
    pub fn payload(&self) -> &F {
        &self.payload
    }
}

impl<C, F> From<Command<C>> for Reaction<C, F> {
    fn from(command: Command<C>) -> Self {
        Self::Command(command)
    }
}

impl<C, F> From<EffectRequest<F>> for Reaction<C, F> {
    fn from(request: EffectRequest<F>) -> Self {
        Self::EffectRequest(request)
    }
}

/// A consumer's pure reaction mapping (Fmodel-school: the domain
/// layer; no I/O inside the reaction). The runner owns everything
/// around it: polling, key minting, the atomic fold, classification,
/// and the ack.
pub trait Saga {
    /// The source event type the saga reacts to.
    type Event;
    /// The consumer's command payload type.
    type Command;
    /// The consumer's effect payload type.
    type Effect;

    /// The saga's identity: runner group, outbox stream key, and the
    /// first component of every intent key minted for it.
    fn id(&self) -> &SagaId;

    /// Map one delivered source event to its reactions. Pure: the
    /// same event always yields the same reactions, which is what
    /// makes redelivery safe to fold.
    fn react(&self, event: &Self::Event) -> Vec<Reaction<Self::Command, Self::Effect>>;
}

/// One reaction payload paired with the envelope the runner minted
/// for it. The envelope carries the reaction's intent key under the
/// framework-owned `intent` metadata key; a fold attaches it
/// unchanged.
#[derive(Debug, Clone)]
pub struct KeyedPayload<'a, P> {
    metadata: EventMetadata,
    payload: &'a P,
}

impl<'a, P> KeyedPayload<'a, P> {
    pub(crate) fn new(metadata: EventMetadata, payload: &'a P) -> Self {
        Self { metadata, payload }
    }

    /// The runner-minted envelope: attach unchanged, one per record.
    pub fn metadata(&self) -> &EventMetadata {
        &self.metadata
    }

    /// The reaction payload.
    pub fn payload(&self) -> &'a P {
        self.payload
    }
}

/// One stream's merged command group: every command one source
/// event's reactions addressed at this stream, concatenated in
/// reaction order, under the single expectation they all agreed on.
/// The fold pushes the group as ONE write - ADR 0006 admits at most
/// one write per stream per batch.
#[derive(Debug)]
pub struct CommandGroup<'a, C> {
    stream: StreamRef,
    expected: ExpectedVersion,
    records: Vec<KeyedPayload<'a, C>>,
}

impl<'a, C> CommandGroup<'a, C> {
    pub(crate) fn new(
        stream: StreamRef,
        expected: ExpectedVersion,
        records: Vec<KeyedPayload<'a, C>>,
    ) -> Self {
        Self {
            stream,
            expected,
            records,
        }
    }

    /// The stream the whole group writes.
    pub fn stream(&self) -> &StreamRef {
        &self.stream
    }

    /// The expectation every command in the group carried. The runner
    /// rejects a group whose commands disagree before the fold runs.
    pub fn expected(&self) -> ExpectedVersion {
        self.expected
    }

    /// The merged payloads with their envelopes, in reaction order.
    pub fn records(&self) -> &[KeyedPayload<'a, C>] {
        &self.records
    }
}

/// The merged intent group: every effect request of one source event,
/// destined for the saga's own outbox stream. Carries no expectation:
/// the outbox stream is append-only accumulation, so the framework
/// fixes the intent write's expectation at [`ExpectedVersion::Any`].
#[derive(Debug)]
pub struct IntentGroup<'a, F> {
    stream: StreamRef,
    records: Vec<KeyedPayload<'a, F>>,
}

impl<'a, F> IntentGroup<'a, F> {
    pub(crate) fn new(stream: StreamRef, records: Vec<KeyedPayload<'a, F>>) -> Self {
        Self { stream, records }
    }

    /// The saga's outbox stream.
    pub fn stream(&self) -> &StreamRef {
        &self.stream
    }

    /// The merged effect payloads with their envelopes, in reaction
    /// order.
    pub fn records(&self) -> &[KeyedPayload<'a, F>] {
        &self.records
    }
}

/// The consumer's translation from merged reaction groups to batch
/// writes: the erasure seam ADR 0006 pins, where typed payloads meet
/// the backend's builder. Erasure happens here, at push, under the
/// backend's own bound. The framework owns grouping, expectation
/// agreement, key minting, and the envelopes; the fold owns only the
/// per-backend `write` call.
pub trait ReactionFold<B> {
    /// The consumer's command payload type.
    type Command;
    /// The consumer's effect payload type.
    type Effect;
    /// Fold failure type; surfaces as [`SagaError::Fold`].
    type Error: std::error::Error + Send + Sync;

    /// Push one merged command group as ONE write to its stream,
    /// attaching each record's envelope unchanged.
    fn push_command_group(
        &self,
        builder: &mut B,
        group: CommandGroup<'_, Self::Command>,
    ) -> Result<(), Self::Error>;

    /// Push one merged intent group as ONE write of
    /// [`OutboxEvent::Intent`] records to the saga's outbox stream,
    /// attaching each record's envelope unchanged. The intent key
    /// rides the envelope, never the payload: the storage-level
    /// uniqueness index reads `event_metadata->>'intent'`.
    fn push_intent_group(
        &self,
        builder: &mut B,
        group: IntentGroup<'_, Self::Effect>,
    ) -> Result<(), Self::Error>;
}

/// The runner's in-memory retry policy for retryable aborts
/// (`TransactError::LockTimeout`) and indeterminate failures
/// (connection loss, deadlock): bounded exponential backoff, then
/// propagate. Classification never happens inside the retry window -
/// the redelivery rule applies only after retryable aborts are
/// retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    attempts: NonZeroU32,
    base: Duration,
    cap: Duration,
}

impl RetryPolicy {
    /// A policy of `attempts` total tries, doubling from `base`,
    /// capped at `cap`. Zero attempts is not a policy.
    pub fn new(attempts: u32, base: Duration, cap: Duration) -> Result<Self, ZeroAttempts> {
        NonZeroU32::new(attempts)
            .map(|attempts| Self {
                attempts,
                base,
                cap,
            })
            .ok_or(ZeroAttempts)
    }

    /// Total tries, including the first.
    pub fn attempts(&self) -> u32 {
        self.attempts.get()
    }

    /// The first retry's wait.
    pub fn base(&self) -> Duration {
        self.base
    }

    /// No wait exceeds this.
    pub fn cap(&self) -> Duration {
        self.cap
    }

    /// The waits between tries: `attempts - 1` of them, doubling from
    /// the base and capped.
    pub fn delays(&self) -> impl Iterator<Item = Duration> {
        let (mut delay, cap) = (self.base, self.cap);
        std::iter::from_fn(move || {
            let current = delay;
            delay = delay.saturating_mul(2).min(cap);
            Some(current)
        })
        .take((self.attempts.get() - 1) as usize)
    }
}

impl Default for RetryPolicy {
    /// The default policy: 5 attempts from a 50ms base, capped at 2s.
    /// Indicative until gate A.
    fn default() -> Self {
        Self {
            attempts: NonZeroU32::new(5).expect("5 is nonzero"),
            base: Duration::from_millis(50),
            cap: Duration::from_secs(2),
        }
    }
}

/// A retry policy was requested with zero attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("a retry policy must allow at least one attempt")]
pub struct ZeroAttempts;

/// The runner: polls the source feed as the saga's consumer group,
/// folds each delivered event's reactions into one atomic batch, and
/// acks the entry's position only after the append succeeds. v1 runs
/// one runner per saga; the group identity is the saga id.
///
/// Type parameters: `H` the batch handle, `Fe` the source feed, `Ob`
/// the outbox stream's store view (the classification re-read), `S`
/// the saga, `Fld` the consumer's fold.
pub struct SagaRunner<H, Fe, Ob, S, Fld> {
    saga: S,
    handle: H,
    feed: Fe,
    outbox: Ob,
    fold: Fld,
    group: ConsumerGroup,
    retry: RetryPolicy,
}

impl<H, Fe, Ob, S, Fld> SagaRunner<H, Fe, Ob, S, Fld>
where
    S: Saga,
{
    /// Assemble a runner. The consumer group derives from the saga id:
    /// one runner per saga, and the group's cursor is the saga's own.
    pub fn new(saga: S, handle: H, feed: Fe, outbox: Ob, fold: Fld, retry: RetryPolicy) -> Self {
        let group =
            ConsumerGroup::new(saga.id().as_str()).expect("a saga id is a non-empty group name");
        Self {
            saga,
            handle,
            feed,
            outbox,
            fold,
            group,
            retry,
        }
    }

    /// The saga this runner runs.
    pub fn saga(&self) -> &S {
        &self.saga
    }

    /// The runner's consumer group.
    pub fn group(&self) -> &ConsumerGroup {
        &self.group
    }

    /// The retry policy.
    pub fn retry(&self) -> RetryPolicy {
        self.retry
    }

    /// The saga's outbox stream: the outbox category, keyed by saga
    /// id. The category name is indicative until gate A (the migration
    /// owns the storage-level name).
    pub fn outbox_stream(&self) -> StreamRef {
        StreamRef::new(OUTBOX_CATEGORY, &self.saga.id().as_str().to_owned())
    }
}

impl<H, Fe, Ob, S, Fld> SagaRunner<H, Fe, Ob, S, Fld>
where
    H: BatchSource,
    Fe: EventFeed<S::Event>,
    Ob: EventStreams<OutboxEvent<S::Effect>, Id = String>,
    S: Saga,
    S::Event: Send + Sync + fmt::Debug,
    S::Effect: Send + Sync + fmt::Debug,
    Fld: ReactionFold<BatchBuilder<H::Wire>, Command = S::Command, Effect = S::Effect>,
{
    /// One poll-react-transact-ack cycle over up to `limit` delivered
    /// entries. Each entry's reactions fold into ONE batch: commands
    /// merged to one write per stream, intents merged to one write on
    /// the saga's outbox stream; the entry's position is acked only
    /// after its batch commits. An empty reaction set acks without a
    /// batch.
    ///
    /// Redelivery classification, per the card's pinned rule: a typed
    /// `TransactError::DuplicateIntent` after the retry window means
    /// this source event's reactions already committed - confirmed by
    /// re-reading the outbox stream for the minted keys - and the
    /// entry acks as a no-op. A conflict or violated constraint is a
    /// real command-arm failure and surfaces. Lock timeouts and
    /// indeterminate backend failures are retried under the runner's
    /// policy, then propagated, never classified.
    #[expect(unused_variables, reason = "todo!() body; filled by E20")]
    pub async fn step(
        &self,
        limit: PollLimit,
    ) -> Result<RunnerStep, SagaError<Fld::Error, Fe::Error, Ob::Error, H::Error>> {
        todo!()
    }

    /// The poll loop: `step` forever, sleeping `interval` between
    /// polls. Errors propagate; restarting the loop is the operator's
    /// call.
    #[expect(unused_variables, reason = "todo!() body; filled by E20")]
    pub async fn run(
        &self,
        limit: PollLimit,
        interval: Duration,
    ) -> Result<(), SagaError<Fld::Error, Fe::Error, Ob::Error, H::Error>> {
        todo!()
    }
}

/// What one [`SagaRunner::step`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerStep {
    /// The poll delivered nothing; the cursor did not move.
    Idle,
    /// Entries were processed and the cursor advanced to here. A
    /// redelivered entry whose duplicate intents were rejected counts
    /// as processed: the no-op is the protocol working.
    Advanced {
        /// The position the group's cursor now stands at.
        acked_to: FeedPosition,
    },
}

/// How a [`SagaRunner::step`] can fail. A version conflict or
/// constraint violation is a real command-arm failure, surfaced per
/// the classification rule; a lock timeout past the retry budget is
/// still retryable by the caller; a backend failure is indeterminate
/// and was already retried.
#[derive(Debug, Error)]
pub enum SagaError<FoldE, FeedE, StoreE, BatchE>
where
    FoldE: std::error::Error,
    FeedE: std::error::Error,
    StoreE: std::error::Error,
    BatchE: std::error::Error,
{
    /// The consumer's fold rejected a merged group.
    #[error(transparent)]
    Fold(FoldE),
    /// The source feed's poll failed.
    #[error(transparent)]
    Poll(FeedE),
    /// The source feed rejected the ack.
    #[error(transparent)]
    Ack(#[from] AckError<FeedE>),
    /// The classification re-read of the outbox stream failed.
    #[error(transparent)]
    OutboxRead(StoreE),
    /// Commands one source event addressed at one stream disagreed on
    /// their expectation: a consumer defect, not a store state.
    #[error("commands in one stream-group disagreed on their expectation: {0}")]
    ConflictingExpectations(StreamRef),
    /// A command arm's expectation failed against the pre-batch head:
    /// a real conflict, never a redelivery artifact.
    #[error(transparent)]
    Conflict(#[from] BatchConflict),
    /// A batch constraint was not satisfied.
    #[error(transparent)]
    ConstraintViolated(#[from] ConstraintViolation),
    /// The typed duplicate-intent outcome fired but a minted key was
    /// absent from the outbox stream on re-read: the store's state
    /// contradicts the protocol.
    #[error("duplicate-intent outcome but key {0} is absent from the outbox stream")]
    IntentMissing(RenderedIntentKey),
    /// The lock wait outlasted the retry budget. Still retryable by
    /// the caller; never classified as a conflict.
    #[error("lock wait outlasted the retry budget after {0:?}; still retryable")]
    LockTimeout(Duration),
    /// The backend failed indeterminately; retried under the policy,
    /// then propagated. Never classified as a conflict.
    #[error(transparent)]
    Backend(BatchE),
}
