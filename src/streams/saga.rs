//! The saga runner (ADR 0010's outbox-saga section): a consumer's
//! pure [`Saga::react`] folded into one atomic batch per source event,
//! with the feed-cursor ack as a separate, explicit step after the
//! append commits.
//!
//! [`Reaction::Command`] arms append events to their target streams;
//! [`Reaction::EffectRequest`] arms record intents on the saga's
//! outbox stream, where the [`crate::streams::outbox`] executor picks
//! them up - effects are facts, never performed inside the reaction.
//! The window between the append and the ack is real; reaction
//! identity closes it. The runner mints a deterministic intent key
//! per reaction, and the abort classifier in [`Runner::step`] decides
//! each batch abort against those keys. Every write goes through the
//! atomic batch, so the single-writer funnel's assumptions (ADR 0010)
//! bind deployments unchanged.

use std::fmt;
use std::time::Duration;

use thiserror::Error;

use super::batch::{
    Batch, BatchBuilder, BatchConflict, ConstraintViolation, StreamRef, TransactError,
};
use super::feed::{AckError, ConsumerGroup, EventFeed, FeedEntry, FeedPosition, PollLimit};
use super::outbox::{Record, INTENT_METADATA_KEY, OUTBOX_CATEGORY};
use super::{BatchSource, EventMetadata, EventStreams, ExpectedVersion, StreamState};

// The identity and retry vocabulary lives in the private `keys` and
// `retry` modules behind the flat root re-exports; `IntentKey` and
// `ReactionIndex` are crate-internal: they appear in no public
// signature, only the runner mints.
pub(crate) use super::keys::{IntentKey, ReactionIndex};
use super::keys::{RenderedIntentKey, SagaId};
use super::retry::RetryPolicy;

/// What one source event produces: commands to append to target
/// streams, and effect requests to record as intents on the saga's
/// outbox stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reaction<C, F> {
    Command(Command<C>),
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
    /// Address a stream with an expectation.
    pub fn new(stream: StreamRef, expected: ExpectedVersion, payload: C) -> Self {
        Self {
            stream,
            expected,
            payload,
        }
    }

    pub fn stream(&self) -> &StreamRef {
        &self.stream
    }

    pub fn expected(&self) -> ExpectedVersion {
        self.expected
    }

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
    pub fn new(payload: F) -> Self {
        Self { payload }
    }

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
    type Event;
    type Command;
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

    /// Intent folds copy the key into the record payload; command
    /// folds leave it in the envelope alone.
    pub fn key(&self) -> &RenderedIntentKey {
        &self.key
    }

    /// Attach unchanged, one per record.
    pub fn metadata(&self) -> &EventMetadata {
        &self.metadata
    }

    pub fn payload(&self) -> &'a P {
        self.payload
    }
}

/// One stream's merged command group: every command one source
/// event's reactions addressed at this stream, concatenated in
/// reaction order, under the single expectation they all agreed on.
/// Commands that disagree on the expectation are rejected before the
/// fold runs. The fold pushes the group as ONE write - ADR 0006
/// admits at most one write per stream per batch.
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

    pub fn stream(&self) -> &StreamRef {
        &self.stream
    }

    pub fn expected(&self) -> ExpectedVersion {
        self.expected
    }

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

    pub fn stream(&self) -> &StreamRef {
        &self.stream
    }

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
    type Command;
    type Effect;
    /// Fold failure type; surfaces as [`Error::Fold`].
    type Error: std::error::Error + Send + Sync;

    /// Push one merged command group as ONE write to its stream,
    /// attaching each record's envelope unchanged.
    fn push_command_group(
        &self,
        builder: &mut B,
        group: CommandGroup<'_, Self::Command>,
    ) -> Result<(), Self::Error>;

    /// Push one merged intent group as ONE write of
    /// [`Record::Intent`] records to the saga's outbox stream.
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

/// The runner: polls the source feed as the saga's consumer group,
/// folds each delivered event's reactions into one atomic batch, and
/// acks the entry's position only after the append succeeds. v1 runs
/// one runner per saga; the group identity is the saga id.
///
/// Type parameters: `H` the batch handle, `Fe` the source feed, `Ob`
/// the outbox stream's store view (the classification re-read), `S`
/// the saga, `Fld` the consumer's fold.
pub struct Runner<H, Fe, Ob, S, Fld> {
    saga: S,
    handle: H,
    feed: Fe,
    outbox: Ob,
    fold: Fld,
    group: ConsumerGroup,
    saga_key: String,
    retry: RetryPolicy,
}

impl<H, Fe, Ob, S, Fld> Runner<H, Fe, Ob, S, Fld>
where
    S: Saga,
{
    /// Assemble a runner. The consumer group derives from the saga id:
    /// one runner per saga, and the group's cursor is the saga's own.
    pub fn new(saga: S, handle: H, feed: Fe, outbox: Ob, fold: Fld, retry: RetryPolicy) -> Self {
        let group =
            ConsumerGroup::new(saga.id().as_str()).expect("a saga id is a nonempty group name");
        let saga_key = saga.id().as_str().to_owned();
        Self {
            saga,
            handle,
            feed,
            outbox,
            fold,
            group,
            saga_key,
            retry,
        }
    }

    pub fn saga(&self) -> &S {
        &self.saga
    }

    pub fn group(&self) -> &ConsumerGroup {
        &self.group
    }

    pub fn retry(&self) -> RetryPolicy {
        self.retry
    }

    /// The saga's outbox stream: the outbox category, keyed by saga id.
    pub fn outbox_stream(&self) -> StreamRef {
        StreamRef::new(OUTBOX_CATEGORY, &self.saga_key)
    }
}

/// One attempt's grouping of an entry's reactions: what the fold
/// consumes and the keys the abort classifier re-reads.
struct GroupedReactions<'a, C, F> {
    commands: Vec<CommandGroup<'a, C>>,
    intents: Vec<KeyedPayload<'a, F>>,
    effect_keys: Vec<RenderedIntentKey>,
}

impl<H, Fe, Ob, S, Fld> Runner<H, Fe, Ob, S, Fld>
where
    H: BatchSource,
    Fe: EventFeed<S::Event>,
    Ob: EventStreams<Record<S::Effect>, Id = String>,
    S: Saga,
    S::Event: Send + Sync + fmt::Debug,
    S::Effect: Send + Sync + fmt::Debug,
    Fld: ReactionFold<BatchBuilder<H::Wire>, Command = S::Command, Effect = S::Effect>,
{
    /// One poll-react-transact-ack cycle over up to `limit` delivered
    /// entries. Each entry's reactions fold into ONE batch - commands
    /// merged to one write per stream, intents to one write on the
    /// saga's outbox stream - and the entry acks only after its batch
    /// commits. An empty reaction set acks without a batch.
    ///
    /// A non-retryable abort classifies per ADR 0010: the runner
    /// re-reads the outbox stream for the minted effect keys. A
    /// standing key acks the redelivery as a no-op; no key surfaces
    /// the real conflict. Lock timeouts and backend failures retry
    /// under the policy, then propagate - never classify.
    pub async fn step(
        &self,
        limit: PollLimit,
    ) -> Result<RunnerStep, Error<Fld::Error, Fe::Error, Ob::Error, H::Error>> {
        let entries = self
            .feed
            .poll(&self.group, limit)
            .await
            .map_err(Error::Poll)?;
        if entries.is_empty() {
            return Ok(RunnerStep::Idle);
        }

        let mut acked_to = None;
        for entry in &entries {
            let position = entry.position();
            let reactions = self.saga.react(entry.record().event());
            if reactions.is_empty() {
                self.feed.ack(&self.group, position).await?;
            } else {
                self.commit_entry(entry, &reactions).await?;
            }
            acked_to = Some(position);
        }

        Ok(RunnerStep::Advanced {
            acked_to: acked_to.expect("entries were nonempty and every entry acked"),
        })
    }

    /// One entry's commit cycle. The folded reactions transact under
    /// the retry policy; the ack follows the commit - directly, or as
    /// the classified no-op a redelivery resolves to. The fold
    /// consumes groups by value, so every attempt regroups the same
    /// reactions; `react` is pure, so every attempt builds an
    /// identical batch.
    async fn commit_entry(
        &self,
        entry: &FeedEntry<S::Event>,
        reactions: &[Reaction<S::Command, S::Effect>],
    ) -> Result<(), Error<Fld::Error, Fe::Error, Ob::Error, H::Error>> {
        let position = entry.position();
        let mut delays = self.retry.delays();
        let mut attempts_left = self.retry.attempts();
        loop {
            let grouped = self
                .group_reactions(entry, position, reactions)
                .map_err(Error::ConflictingExpectations)?;
            let GroupedReactions {
                commands,
                intents,
                effect_keys,
            } = grouped;
            let batch = self.build_batch(commands, intents).map_err(Error::Fold)?;
            match self.handle.transact(batch).await {
                Ok(()) => {
                    self.feed.ack(&self.group, position).await?;
                    return Ok(());
                }
                Err(TransactError::LockTimeout(bound)) => {
                    attempts_left -= 1;
                    if attempts_left == 0 {
                        return Err(Error::LockTimeout(bound));
                    }
                    tokio::time::sleep(delays.next().expect("a wait per retry")).await;
                }
                Err(TransactError::Backend(error)) => {
                    attempts_left -= 1;
                    if attempts_left == 0 {
                        return Err(Error::Backend(error));
                    }
                    tokio::time::sleep(delays.next().expect("a wait per retry")).await;
                }
                Err(
                    abort @ (TransactError::Conflict(_)
                    | TransactError::ConstraintViolated(_)
                    | TransactError::DuplicateIntent(_)),
                ) => {
                    return self.classify_abort(abort, position, &effect_keys).await;
                }
            }
        }
    }

    /// Group one entry's reactions for the fold. Commands merge per
    /// stream in first-seen order; a second command for an already
    /// grouped stream carries the same expectation, or the whole
    /// group is rejected here - before any fold call or write.
    /// Effects collect for the outbox write, and the minted effect
    /// keys are what the abort classifier later re-reads. The
    /// conflicting stream names the rejection.
    fn group_reactions<'a>(
        &self,
        entry: &'a FeedEntry<S::Event>,
        position: FeedPosition,
        reactions: &'a [Reaction<S::Command, S::Effect>],
    ) -> Result<GroupedReactions<'a, S::Command, S::Effect>, StreamRef> {
        let mut command_groups: Vec<CommandGroup<'_, S::Command>> = Vec::new();
        let mut intents = Vec::new();
        let mut effect_keys = Vec::new();
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
                                return Err(command.stream().clone());
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
                    effect_keys.push(key.clone());
                    intents.push(KeyedPayload::new(key, envelope, request.payload()));
                }
            }
        }
        Ok(GroupedReactions {
            commands: command_groups,
            intents,
            effect_keys,
        })
    }

    /// Fold the grouped reactions into one batch: one write per
    /// command stream plus the merged intent write. A writeless
    /// result is a fold-contract violation - a nonempty reaction set
    /// handed the fold at least one group.
    fn build_batch(
        &self,
        commands: Vec<CommandGroup<'_, S::Command>>,
        intents: Vec<KeyedPayload<'_, S::Effect>>,
    ) -> Result<Batch<H::Wire>, Fld::Error> {
        let mut builder = self.handle.builder();
        for group in commands {
            self.fold.push_command_group(&mut builder, group)?;
        }
        if !intents.is_empty() {
            let group = IntentGroup::new(self.outbox_stream(), intents);
            self.fold.push_intent_group(&mut builder, group)?;
        }
        Ok(builder
            .build()
            .expect("a nonempty reaction set gives the fold at least one group"))
    }

    /// Classify a non-retryable abort (ADR 0010): re-read the outbox
    /// stream for this entry's minted effect keys. A key standing
    /// means the reactions already committed in an earlier attempt or
    /// lifetime - the abort is a redelivery artifact, and the entry
    /// acks as a no-op. No key means a real command-arm failure,
    /// surfaced.
    async fn classify_abort(
        &self,
        abort: TransactError<H::Error>,
        position: FeedPosition,
        effect_keys: &[RenderedIntentKey],
    ) -> Result<(), Error<Fld::Error, Fe::Error, Ob::Error, H::Error>> {
        let key_present = match self.outbox.load_stream(&self.saga_key).await {
            Ok(StreamState::Present(batch)) => batch.records().iter().any(|record| {
                matches!(
                    record.event(),
                    Record::Intent { intent, .. } if effect_keys.contains(intent)
                )
            }),
            Ok(StreamState::Missing) => false,
            Err(error) => return Err(Error::OutboxRead(error)),
        };
        if key_present {
            self.feed.ack(&self.group, position).await?;
            return Ok(());
        }
        Err(match abort {
            TransactError::Conflict(conflict) => Error::Conflict(conflict),
            TransactError::ConstraintViolated(violation) => Error::ConstraintViolated(violation),
            TransactError::DuplicateIntent(_) => Error::IntentMissing(
                effect_keys
                    .first()
                    .cloned()
                    .expect("a duplicate-intent abort implies this entry minted effect keys"),
            ),
            TransactError::LockTimeout(_) | TransactError::Backend(_) => {
                unreachable!("retryable aborts retry inside the window and never classify")
            }
        })
    }

    /// The poll loop: `step` forever, sleeping `interval` between
    /// polls. Errors propagate; restarting the loop is the operator's
    /// call.
    pub async fn run(
        &self,
        limit: PollLimit,
        interval: Duration,
    ) -> Result<(), Error<Fld::Error, Fe::Error, Ob::Error, H::Error>> {
        loop {
            self.step(limit).await?;
            tokio::time::sleep(interval).await;
        }
    }
}

/// What one [`Runner::step`] did.
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

/// How a [`Runner::step`] can fail. A version conflict or
/// constraint violation is a real command-arm failure, surfaced per
/// the classification rule; a lock timeout past the retry budget is
/// still retryable by the caller; a backend failure is indeterminate
/// and was already retried.
#[derive(Debug, Error)]
pub enum Error<FoldE, FeedE, StoreE, BatchE>
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
