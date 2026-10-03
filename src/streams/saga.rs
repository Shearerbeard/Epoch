//! Turn a source event's command and effect-request reactions into one
//! atomic batch. Effect requests become outbox intents, not external
//! calls. A separate feed ack follows the commit; stable reaction
//! keys let the runner recognize a committed batch on redelivery.

use std::fmt;
use std::time::Duration;

use thiserror::Error;

use super::batch::{
    Batch, BatchBuilder, BatchConflict, ConstraintViolation, StreamRef, TransactError,
};
use super::feed::{AckError, ConsumerGroup, EventFeed, FeedEntry, FeedPosition, PollLimit};
use super::outbox::{Record, INTENT_METADATA_KEY, OUTBOX_CATEGORY};
use super::{BatchSource, EventMetadata, EventStreams, ExpectedVersion, StreamState};

pub(crate) use super::keys::{IntentKey, ReactionIndex};
use super::keys::{RenderedIntentKey, SagaId};
use super::retry::RetryPolicy;

/// A command appends to a target stream; an effect request records an
/// outbox intent for later execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reaction<CommandPayload, EffectPayload> {
    Command(Command<CommandPayload>),
    EffectRequest(EffectRequest<EffectPayload>),
}

/// A target stream, its expected version, and the command payload.
/// Consumers may use the runner's event-metadata key for deduplication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command<Payload> {
    stream: StreamRef,
    expected: ExpectedVersion,
    payload: Payload,
}

impl<Payload> Command<Payload> {
    /// Address a stream with an expectation.
    pub fn new(stream: StreamRef, expected: ExpectedVersion, payload: Payload) -> Self {
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

    pub fn payload(&self) -> &Payload {
        &self.payload
    }
}

/// A payload recorded in the outbox for at-least-once execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectRequest<Payload> {
    payload: Payload,
}

impl<Payload> EffectRequest<Payload> {
    pub fn new(payload: Payload) -> Self {
        Self { payload }
    }

    pub fn payload(&self) -> &Payload {
        &self.payload
    }
}

impl<CommandPayload, EffectPayload> From<Command<CommandPayload>>
    for Reaction<CommandPayload, EffectPayload>
{
    fn from(command: Command<CommandPayload>) -> Self {
        Self::Command(command)
    }
}

impl<CommandPayload, EffectPayload> From<EffectRequest<EffectPayload>>
    for Reaction<CommandPayload, EffectPayload>
{
    fn from(request: EffectRequest<EffectPayload>) -> Self {
        Self::EffectRequest(request)
    }
}

/// Map a delivered event to repeatable reactions without performing I/O.
/// A replay must produce the same reactions in the same order.
pub trait Saga {
    type Event;
    type Command;
    type Effect;

    /// Identity shared by the runner group and the outbox stream.
    fn id(&self) -> &SagaId;

    /// Reactions must be stable on redelivery.
    fn react(&self, event: &Self::Event) -> Vec<Reaction<Self::Command, Self::Effect>>;
}

/// The runner's reaction key, metadata, and consumer payload. The fold
/// must preserve the metadata when it writes a record.
#[derive(Debug, Clone)]
pub struct KeyedPayload<'a, Payload> {
    key: RenderedIntentKey,
    metadata: EventMetadata,
    payload: &'a Payload,
}

impl<'a, Payload> KeyedPayload<'a, Payload> {
    pub(crate) fn new(
        key: RenderedIntentKey,
        metadata: EventMetadata,
        payload: &'a Payload,
    ) -> Self {
        Self {
            key,
            metadata,
            payload,
        }
    }

    /// Intent records copy this key into their payload as well.
    pub fn key(&self) -> &RenderedIntentKey {
        &self.key
    }

    /// Attach this envelope to the written record unchanged.
    pub fn metadata(&self) -> &EventMetadata {
        &self.metadata
    }

    pub fn payload(&self) -> &'a Payload {
        self.payload
    }
}

/// Commands for one target stream, in reaction order. The runner
/// rejects conflicting expectations before calling the fold; the fold
/// must emit one write for this group.
#[derive(Debug)]
pub struct CommandGroup<'a, Payload> {
    stream: StreamRef,
    expected: ExpectedVersion,
    records: Vec<KeyedPayload<'a, Payload>>,
}

impl<'a, Payload> CommandGroup<'a, Payload> {
    pub(crate) fn new(
        stream: StreamRef,
        expected: ExpectedVersion,
        records: Vec<KeyedPayload<'a, Payload>>,
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

    pub fn records(&self) -> &[KeyedPayload<'a, Payload>] {
        &self.records
    }
}

/// Effect requests for one outbox stream. The fold writes them together
/// at [`ExpectedVersion::Any`].
#[derive(Debug)]
pub struct IntentGroup<'a, Payload> {
    stream: StreamRef,
    records: Vec<KeyedPayload<'a, Payload>>,
}

impl<'a, Payload> IntentGroup<'a, Payload> {
    pub(crate) fn new(stream: StreamRef, records: Vec<KeyedPayload<'a, Payload>>) -> Self {
        Self { stream, records }
    }

    pub fn stream(&self) -> &StreamRef {
        &self.stream
    }

    pub fn records(&self) -> &[KeyedPayload<'a, Payload>] {
        &self.records
    }
}

/// Convert typed reaction groups into backend batch writes. The runner
/// groups commands and mints keys; the fold writes each group once,
/// retaining every record's metadata.
pub trait ReactionFold<B> {
    type Command;
    type Effect;
    /// Fold failure type; surfaces as [`Error::Fold`].
    type Error: std::error::Error + Send + Sync;

    /// Emit one write per group, preserving each record's envelope.
    fn push_command_group(
        &self,
        builder: &mut B,
        group: CommandGroup<'_, Self::Command>,
    ) -> Result<(), Self::Error>;

    /// Emit one outbox write of [`Record::Intent`] events, copying each
    /// key into the payload and preserving its envelope for uniqueness.
    fn push_intent_group(
        &self,
        builder: &mut B,
        group: IntentGroup<'_, Self::Effect>,
    ) -> Result<(), Self::Error>;
}

/// Commits a saga's reactions and acks each feed entry afterward. One
/// runner owns one saga's consumer group.
pub struct Runner<Handle, Feed, Outbox, SagaImpl, Fold> {
    saga: SagaImpl,
    handle: Handle,
    feed: Feed,
    outbox: Outbox,
    fold: Fold,
    group: ConsumerGroup,
    saga_key: String,
    retry: RetryPolicy,
}

impl<Handle, Feed, Outbox, SagaImpl, Fold> Runner<Handle, Feed, Outbox, SagaImpl, Fold>
where
    SagaImpl: Saga,
{
    /// Assemble a runner. The consumer group derives from the saga id:
    /// one runner per saga, and the group's cursor is the saga's own.
    pub fn new(
        saga: SagaImpl,
        handle: Handle,
        feed: Feed,
        outbox: Outbox,
        fold: Fold,
        retry: RetryPolicy,
    ) -> Self {
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

    pub fn saga(&self) -> &SagaImpl {
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
struct GroupedReactions<'a, CommandPayload, EffectPayload> {
    commands: Vec<CommandGroup<'a, CommandPayload>>,
    intents: Vec<KeyedPayload<'a, EffectPayload>>,
    effect_keys: Vec<RenderedIntentKey>,
}

impl<Handle, Feed, Outbox, SagaImpl, Fold> Runner<Handle, Feed, Outbox, SagaImpl, Fold>
where
    Handle: BatchSource,
    Feed: EventFeed<SagaImpl::Event>,
    Outbox: EventStreams<Record<SagaImpl::Effect>, Id = String>,
    SagaImpl: Saga,
    SagaImpl::Event: Send + Sync + fmt::Debug,
    SagaImpl::Effect: Send + Sync + fmt::Debug,
    Fold: ReactionFold<
        BatchBuilder<Handle::Wire>,
        Command = SagaImpl::Command,
        Effect = SagaImpl::Effect,
    >,
{
    /// Process up to `limit` entries. An empty reaction set acks
    /// without a batch. Retryable errors are never classified as
    /// redelivery; a standing outbox intent key can identify one.
    pub async fn step(
        &self,
        limit: PollLimit,
    ) -> Result<RunnerStep, Error<Fold::Error, Feed::Error, Outbox::Error, Handle::Error>> {
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

    /// Rebuild the batch on each retry, and ack only after commit or
    /// confirmed redelivery.
    async fn commit_entry(
        &self,
        entry: &FeedEntry<SagaImpl::Event>,
        reactions: &[Reaction<SagaImpl::Command, SagaImpl::Effect>],
    ) -> Result<(), Error<Fold::Error, Feed::Error, Outbox::Error, Handle::Error>> {
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

    /// Merge commands per stream and reject disagreeing expectations
    /// before the fold writes anything.
    fn group_reactions<'a>(
        &self,
        entry: &'a FeedEntry<SagaImpl::Event>,
        position: FeedPosition,
        reactions: &'a [Reaction<SagaImpl::Command, SagaImpl::Effect>],
    ) -> Result<GroupedReactions<'a, SagaImpl::Command, SagaImpl::Effect>, StreamRef> {
        let mut command_groups: Vec<CommandGroup<'_, SagaImpl::Command>> = Vec::new();
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

    /// Build one batch for all command streams and outbox intents.
    fn build_batch(
        &self,
        commands: Vec<CommandGroup<'_, SagaImpl::Command>>,
        intents: Vec<KeyedPayload<'_, SagaImpl::Effect>>,
    ) -> Result<Batch<Handle::Wire>, Fold::Error> {
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

    /// Re-read minted intent keys after a non-retryable abort. A key
    /// already on the outbox stream identifies an earlier commit; no
    /// key means the abort is real and the cursor stays put.
    async fn classify_abort(
        &self,
        abort: TransactError<Handle::Error>,
        position: FeedPosition,
        effect_keys: &[RenderedIntentKey],
    ) -> Result<(), Error<Fold::Error, Feed::Error, Outbox::Error, Handle::Error>> {
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

    /// Poll until an error; callers decide whether to restart.
    pub async fn run(
        &self,
        limit: PollLimit,
        interval: Duration,
    ) -> Result<(), Error<Fold::Error, Feed::Error, Outbox::Error, Handle::Error>> {
        loop {
            self.step(limit).await?;
            tokio::time::sleep(interval).await;
        }
    }
}

/// Result of one runner poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerStep {
    /// No entry was delivered; the cursor did not move.
    Idle,
    /// Entries were acknowledged, including confirmed redeliveries.
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
