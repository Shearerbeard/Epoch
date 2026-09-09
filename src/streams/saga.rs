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
//! (saga id, source stream, source position, reaction index) - and a
//! redelivery is recognized by those keys. After the retry window,
//! every batch abort sends the runner back to the outbox stream for
//! the minted keys: key present means this source event's reactions
//! already committed, so the abort is a redelivery artifact - the
//! typed [`DuplicateIntent`] when the uniqueness index fired, a
//! conflict when the original commit moved a command arm's stream
//! past the arm's own expectation - and the entry acks as a no-op.
//! Key absent means a real command-arm conflict, surfaced as an
//! error. Retryable aborts (`TransactError::LockTimeout`) and
//! indeterminate failures are retried under the runner's
//! [`RetryPolicy`] and then propagated, never classified.
//!
//! Every write the runner makes goes through the atomic batch, so the
//! single-writer funnel's operating assumptions (ADR 0010, and the
//! postgres module doc) bind deployments unchanged.

use std::fmt;
use std::num::NonZeroU32;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::batch::{BatchBuilder, BatchConflict, ConstraintViolation, StreamRef, TransactError};
use super::feed::{AckError, ConsumerGroup, EventFeed, FeedPosition, PollLimit};
use super::outbox::{OutboxEvent, INTENT_METADATA_KEY, OUTBOX_CATEGORY};
use super::{BatchSource, EventMetadata, EventStreams, ExpectedVersion, StreamState};

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
/// for it and the reaction's rendered intent key. The envelope
/// carries the key under the framework-owned `intent` metadata key;
/// a fold attaches it unchanged. Intent records additionally carry
/// the key in their payload, so the executor never reads the
/// envelope.
#[derive(Debug, Clone)]
pub struct KeyedPayload<'a, P> {
    key: RenderedIntentKey,
    metadata: EventMetadata,
    payload: &'a P,
}

impl<'a, P> KeyedPayload<'a, P> {
    pub(crate) fn new(key: RenderedIntentKey, metadata: EventMetadata, payload: &'a P) -> Self {
        Self {
            key,
            metadata,
            payload,
        }
    }

    /// The reaction's rendered intent key. Intent folds copy it into
    /// the record payload (`OutboxEvent::Intent`); command folds
    /// leave it in the envelope alone.
    pub fn key(&self) -> &RenderedIntentKey {
        &self.key
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
    /// [`OutboxEvent::Intent`] records to the saga's outbox stream.
    /// Each record carries its key twice: in the payload (the
    /// executor's read source) copied from [`KeyedPayload::key`], and
    /// in the envelope attached unchanged (the storage-level
    /// uniqueness index reads `event_metadata->>'intent'`).
    fn push_intent_group(
        &self,
        builder: &mut B,
        group: IntentGroup<'_, Self::Effect>,
    ) -> Result<(), Self::Error>;
}

/// A bounded exponential backoff schedule: each wait doubles from
/// `base`, never exceeding `cap`. Carries no attempt count - the
/// runner's policy and the executor's budget each own their own
/// count, so the two can never disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackoffSchedule {
    base: Duration,
    cap: Duration,
}

impl BackoffSchedule {
    /// A schedule doubling from `base`, capped at `cap`. A base above
    /// its cap is not a schedule.
    pub fn new(base: Duration, cap: Duration) -> Result<Self, BaseExceedsCap> {
        if base > cap {
            Err(BaseExceedsCap)
        } else {
            Ok(Self { base, cap })
        }
    }

    /// The first wait.
    pub fn base(&self) -> Duration {
        self.base
    }

    /// No wait exceeds this.
    pub fn cap(&self) -> Duration {
        self.cap
    }

    /// The waits, doubling from the base and capped: an unbounded
    /// stream; the caller's own count bounds it.
    pub fn delays(&self) -> impl Iterator<Item = Duration> {
        let (mut delay, cap) = (self.base, self.cap);
        std::iter::from_fn(move || {
            let current = delay;
            delay = delay.saturating_mul(2).min(cap);
            Some(current)
        })
    }
}

impl Default for BackoffSchedule {
    /// 50ms base, 2s cap. Indicative until gate A.
    fn default() -> Self {
        Self {
            base: Duration::from_millis(50),
            cap: Duration::from_secs(2),
        }
    }
}

/// A backoff schedule whose base exceeds its cap was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("the backoff base exceeds its cap")]
pub struct BaseExceedsCap;

/// The runner's in-memory retry policy for retryable aborts
/// (`TransactError::LockTimeout`) and indeterminate failures
/// (connection loss, deadlock): a bounded count of tries under a
/// backoff schedule, then propagate. Classification never happens
/// inside the retry window - the redelivery rule applies only after
/// retryable aborts are retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    attempts: NonZeroU32,
    schedule: BackoffSchedule,
}

impl RetryPolicy {
    /// A policy of `attempts` total tries under `schedule`. Zero
    /// attempts is not a policy.
    pub fn new(attempts: u32, schedule: BackoffSchedule) -> Result<Self, ZeroAttempts> {
        NonZeroU32::new(attempts)
            .map(|attempts| Self { attempts, schedule })
            .ok_or(ZeroAttempts)
    }

    /// Total tries, including the first.
    pub fn attempts(&self) -> u32 {
        self.attempts.get()
    }

    /// The backoff schedule between tries.
    pub fn schedule(&self) -> BackoffSchedule {
        self.schedule
    }

    /// The waits between tries: `attempts - 1` of them, doubling from
    /// the schedule's base and capped.
    pub fn delays(&self) -> impl Iterator<Item = Duration> {
        self.schedule
            .delays()
            .take((self.attempts.get() - 1) as usize)
    }
}

impl Default for RetryPolicy {
    /// The default policy: 5 attempts under the default schedule.
    /// Indicative until gate A.
    fn default() -> Self {
        Self {
            attempts: NonZeroU32::new(5).expect("5 is nonzero"),
            schedule: BackoffSchedule::default(),
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
    /// Redelivery classification, per the card's pinned rule: after
    /// the retry window, every batch abort sends the runner back to
    /// the outbox stream for the minted keys. Key present means this
    /// source event's reactions already committed and the abort is a
    /// redelivery artifact - the typed `TransactError::DuplicateIntent`
    /// when the uniqueness index fired, a conflict or violated
    /// constraint when the original commit moved a command arm's
    /// stream past the arm's own expectation - and the entry acks as
    /// a no-op. Key absent means a real command-arm failure,
    /// surfaced. Lock timeouts and indeterminate backend failures are
    /// retried under the runner's policy, then propagated, never
    /// classified.
    pub async fn step(
        &self,
        limit: PollLimit,
    ) -> Result<RunnerStep, SagaError<Fld::Error, Fe::Error, Ob::Error, H::Error>> {
        let entries = self
            .feed
            .poll(&self.group, limit)
            .await
            .map_err(SagaError::Poll)?;
        if entries.is_empty() {
            return Ok(RunnerStep::Idle);
        }

        let mut acked_to = None;
        for entry in &entries {
            let position = entry.position();
            let reactions = self.saga.react(entry.record().event());
            if reactions.is_empty() {
                self.feed
                    .ack(&self.group, position)
                    .await
                    .map_err(SagaError::Ack)?;
                acked_to = Some(position);
                continue;
            }

            // The retry window. The fold consumes the groups by value
            // and the batch type is not Clone across the generic, so
            // every attempt regroups the same reactions: react is
            // pure, so every attempt rebuilds an identical batch.
            let mut delays = self.retry.delays();
            let mut attempts_left = self.retry.attempts();
            loop {
                // Mint one key per reaction, in reaction order, and
                // group as we go: commands per target stream in
                // first-seen order, effects onto the saga's outbox
                // stream. A second command for an already-grouped
                // stream must agree on the expectation, or the group
                // is rejected here - before any fold call, before any
                // write.
                let mut command_groups: Vec<CommandGroup<'_, S::Command>> = Vec::new();
                let mut intents = Vec::new();
                let mut minted_keys: Vec<RenderedIntentKey> = Vec::new();
                for (index, reaction) in reactions.iter().enumerate() {
                    let key = IntentKey::mint(
                        self.saga.id(),
                        entry.stream().clone(),
                        position,
                        ReactionIndex::new(index as u64),
                    )
                    .render();
                    let mut envelope = EventMetadata::new();
                    envelope.insert(INTENT_METADATA_KEY, key.as_str());
                    match reaction {
                        Reaction::Command(command) => {
                            let record = KeyedPayload::new(key, envelope, command.payload());
                            match command_groups
                                .iter_mut()
                                .find(|group| group.stream() == command.stream())
                            {
                                Some(group) => {
                                    if group.expected() != command.expected() {
                                        return Err(SagaError::ConflictingExpectations(
                                            command.stream().clone(),
                                        ));
                                    }
                                    group.records.push(record);
                                }
                                None => command_groups.push(CommandGroup::new(
                                    command.stream().clone(),
                                    command.expected(),
                                    vec![record],
                                )),
                            }
                        }
                        Reaction::EffectRequest(request) => {
                            minted_keys.push(key.clone());
                            intents.push(KeyedPayload::new(key, envelope, request.payload()));
                        }
                    }
                }

                let mut builder = self.handle.builder();
                for group in command_groups {
                    self.fold
                        .push_command_group(&mut builder, group)
                        .map_err(SagaError::Fold)?;
                }
                if !intents.is_empty() {
                    let group = IntentGroup::new(self.outbox_stream(), intents);
                    self.fold
                        .push_intent_group(&mut builder, group)
                        .map_err(SagaError::Fold)?;
                }
                let batch = builder.build().expect(
                    "the reaction set is nonempty, so the fold was handed at least \
                     one group; a writeless batch means the fold accepted a group \
                     and pushed no write - the fold-contract violation whose \
                     wording gate A owns and whose variant the error surface lacks",
                );

                match self.handle.transact(batch).await {
                    Ok(()) => {
                        self.feed
                            .ack(&self.group, position)
                            .await
                            .map_err(SagaError::Ack)?;
                        acked_to = Some(position);
                        break;
                    }
                    Err(TransactError::LockTimeout(bound)) => {
                        attempts_left -= 1;
                        if attempts_left == 0 {
                            return Err(SagaError::LockTimeout(bound));
                        }
                        tokio::time::sleep(delays.next().expect("a wait per retry")).await;
                    }
                    Err(TransactError::Backend(error)) => {
                        attempts_left -= 1;
                        if attempts_left == 0 {
                            return Err(SagaError::Backend(error));
                        }
                        tokio::time::sleep(delays.next().expect("a wait per retry")).await;
                    }
                    Err(
                        abort @ (TransactError::Conflict(_)
                        | TransactError::ConstraintViolated(_)
                        | TransactError::DuplicateIntent(_)),
                    ) => {
                        // Classification runs only here, after the retry
                        // window, on the last attempt's abort: the re-read
                        // asks whether any of THIS entry's minted effect
                        // keys stands on the outbox stream.
                        let key_present = match self
                            .outbox
                            .load_stream(&self.saga.id().as_str().to_owned())
                            .await
                        {
                            Ok(StreamState::Present(batch)) => {
                                batch.records().iter().any(|record| {
                                    matches!(
                                        record.event(),
                                        OutboxEvent::Intent { intent, .. }
                                            if minted_keys.contains(intent)
                                    )
                                })
                            }
                            Ok(StreamState::Missing) => false,
                            Err(error) => return Err(SagaError::OutboxRead(error)),
                        };
                        if key_present {
                            // The reactions already committed in an earlier
                            // attempt or lifetime: the abort is a redelivery
                            // artifact, and the entry acks as a no-op.
                            self.feed
                                .ack(&self.group, position)
                                .await
                                .map_err(SagaError::Ack)?;
                            acked_to = Some(position);
                            break;
                        }
                        return Err(match abort {
                            TransactError::Conflict(conflict) => SagaError::Conflict(conflict),
                            TransactError::ConstraintViolated(violation) => {
                                SagaError::ConstraintViolated(violation)
                            }
                            TransactError::DuplicateIntent(_) => {
                                SagaError::IntentMissing(minted_keys.first().cloned().expect(
                                    "the duplicate-intent outcome fires only when an \
                                     intent write carried a key the store already \
                                     holds, and intent writes carry exactly this \
                                     entry's minted effect keys",
                                ))
                            }
                            TransactError::LockTimeout(_) | TransactError::Backend(_) => {
                                unreachable!("retryable aborts retry above and never classify")
                            }
                        });
                    }
                }
            }
        }

        Ok(RunnerStep::Advanced {
            acked_to: acked_to.expect("entries were nonempty and every entry acked"),
        })
    }

    /// The poll loop: `step` forever, sleeping `interval` between
    /// polls. Errors propagate; restarting the loop is the operator's
    /// call.
    pub async fn run(
        &self,
        limit: PollLimit,
        interval: Duration,
    ) -> Result<(), SagaError<Fld::Error, Fe::Error, Ob::Error, H::Error>> {
        loop {
            self.step(limit).await?;
            tokio::time::sleep(interval).await;
        }
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
    /// A batch constraint was not satisfied. Reactions carry no
    /// constraints, so this is reachable only when a fold exceeds its
    /// contract and adds one; surfaced rather than panicked, because
    /// the fold is consumer code.
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
