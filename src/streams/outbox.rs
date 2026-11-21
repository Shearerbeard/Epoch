//! Execute effect intents from the saga outbox with ordered,
//! at-least-once delivery. A replay skips any effect whose `Done`
//! record is already stored. Before that record is appended, a
//! crash can redeliver the intent, so ports and compensation hooks
//! must dedupe by intent key.

use std::fmt;
use std::marker::PhantomData;
use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::decider::Event;

use super::feed::{AckError, ConsumerGroup, EventFeed, FeedPosition, PollLimit};
use super::keys::{RenderedIntentKey, SagaId};
use super::retry::{BackoffSchedule, RetryBudget, RetryPolicy};
use super::{AppendError, EventBatch, EventStreams, ExpectedVersion, StreamState};

/// Reserved outbox category. The PostgreSQL uniqueness index reads this
/// name; renaming it needs a schema migration.
pub const OUTBOX_CATEGORY: &str = "saga-outbox";

/// Reserved metadata key read by the PostgreSQL intent-uniqueness index.
pub const INTENT_METADATA_KEY: &str = "intent";

/// Outbox intent and outcome records. Only intents carry the key in
/// their envelope: repeating that metadata on outcomes would trip
/// the PostgreSQL uniqueness index. All sagas in this category share
/// one effect payload type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Record<Effect> {
    /// An effect request committed with its command reactions.
    Intent {
        intent: RenderedIntentKey,
        request: Effect,
    },
    Done {
        intent: RenderedIntentKey,
    },
    /// A failed attempt; its count per key survives executor crashes.
    Failed {
        intent: RenderedIntentKey,
        /// The port's error, rendered; diagnostic only.
        error: String,
    },
    /// Terminal failure after the retry budget is exhausted. The key
    /// makes a replay after parking recognizable without a second append.
    Parked {
        intent: RenderedIntentKey,
        /// The durable failed-attempt count at park time.
        attempts: NonZeroU32,
        /// The last failure, rendered; diagnostic only.
        error: String,
    },
}

impl<Effect> Event for Record<Effect> {
    type EntityId = ();

    fn event_type(&self) -> String {
        match self {
            Self::Intent { .. } => "OutboxIntent",
            Self::Done { .. } => "OutboxDone",
            Self::Failed { .. } => "OutboxFailed",
            Self::Parked { .. } => "OutboxParked",
        }
        .to_owned()
    }

    fn get_id(&self) -> Self::EntityId {}
}

/// The consumer's effect port under at-least-once delivery: a crash
/// between the external call and the outcome append replays the
/// intent, and the port's idempotency absorbs the duplicate. Dedupe
/// by the intent key - two distinct reactions may carry identical
/// payloads, so the key is the only replay-safe identity.
#[trait_variant::make(Send)]
pub trait EffectPort<Effect> {
    /// Port failure type; rendered onto the `Failed` record as
    /// diagnostic text.
    type Error: std::error::Error + Send + Sync;

    /// `intent` is the port's idempotency identity.
    async fn perform(
        &self,
        intent: &RenderedIntentKey,
        request: &Effect,
    ) -> Result<(), Self::Error>;
}

/// What the compensation hook learns at park time: the parked intent,
/// its durable attempt count, and the last failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParkedNotice<Effect> {
    key: RenderedIntentKey,
    request: Effect,
    attempts: NonZeroU32,
    error: String,
}

impl<Effect> ParkedNotice<Effect> {
    pub(crate) fn new(
        key: RenderedIntentKey,
        request: Effect,
        attempts: NonZeroU32,
        error: String,
    ) -> Self {
        Self {
            key,
            request,
            attempts,
            error,
        }
    }

    pub fn key(&self) -> &RenderedIntentKey {
        &self.key
    }

    /// The effect payload that never landed.
    pub fn request(&self) -> &Effect {
        &self.request
    }

    pub fn attempts(&self) -> NonZeroU32 {
        self.attempts
    }

    pub fn error(&self) -> &str {
        &self.error
    }
}

/// The consumer's compensation action, fired by the executor when an
/// intent parks TERMINAL-FAILED. Fires at park time, in the executor
/// process; a crash between the park append and the hook re-fires it
/// on replay, so the hook carries the same idempotency obligation
/// effects carry.
#[trait_variant::make(Send)]
pub trait CompensationHook<Effect> {
    /// Hook failure type; surfaces as [`ExecutorError::Hook`].
    type Error: std::error::Error + Send + Sync;

    async fn on_parked(&self, notice: &ParkedNotice<Effect>) -> Result<(), Self::Error>;
}

/// Consumes one saga's intents from the shared outbox category. Other
/// sagas' entries and outcome records advance the cursor without
/// invoking the port.
pub struct Executor<Feed, Store, Port, Hook, Effect> {
    feed: Feed,
    store: Store,
    port: Port,
    hook: Hook,
    saga: SagaId,
    group: ConsumerGroup,
    saga_key: String,
    policy: RetryPolicy,
    _marker: PhantomData<fn() -> Effect>,
}

impl<Feed, Store, Port, Hook, Effect> Executor<Feed, Store, Port, Hook, Effect> {
    /// Assemble an executor for one saga's outbox stream. The group
    /// derives from the saga id: one executor group per saga, its
    /// cursor independent of the runner's (group progress is scoped
    /// per category).
    pub fn new(
        feed: Feed,
        store: Store,
        port: Port,
        hook: Hook,
        saga: SagaId,
        policy: RetryPolicy,
    ) -> Self {
        let group = ConsumerGroup::new(saga.as_str()).expect("a saga id is nonempty group name");
        let saga_key = saga.as_str().to_owned();
        Self {
            feed,
            store,
            port,
            hook,
            saga,
            group,
            saga_key,
            policy,
            _marker: PhantomData,
        }
    }

    pub fn saga(&self) -> &SagaId {
        &self.saga
    }

    pub fn group(&self) -> &ConsumerGroup {
        &self.group
    }

    pub fn budget(&self) -> RetryBudget {
        self.policy.budget()
    }

    pub fn backoff(&self) -> BackoffSchedule {
        self.policy.schedule()
    }
}

/// One intent's durable replay state. `Done` is terminal over later
/// matching outcomes; a restart's retry budget applies to `Attempted`.
enum DurableState {
    Fresh,
    Attempted {
        failed: NonZeroU32,
        last_error: String,
    },
    Parked {
        attempts: NonZeroU32,
        error: String,
    },
    Done,
}

impl<Feed, Store, Port, Hook, Effect> Executor<Feed, Store, Port, Hook, Effect>
where
    Feed: EventFeed<Record<Effect>>,
    Store: EventStreams<Record<Effect>, Id = String>,
    Port: EffectPort<Effect>,
    Hook: CompensationHook<Effect>,
    Effect: Send + Sync + fmt::Debug,
{
    /// Process up to `limit` entries, recording outcomes before acking.
    pub async fn step(
        &self,
        limit: PollLimit,
    ) -> Result<ExecutorStep, ExecutorError<Hook::Error, Feed::Error, Store::Error>> {
        let entries = self
            .feed
            .poll(&self.group, limit)
            .await
            .map_err(ExecutorError::Poll)?;
        if entries.is_empty() {
            return Ok(ExecutorStep::Idle);
        }

        let mut acked_to = None;
        for entry in entries {
            let (position, stream, record) = entry.into_parts();
            if stream.key() != self.saga.as_str() {
                // Another saga's stream in the shared category:
                // consume the entry, never perform it.
                self.feed.ack(&self.group, position).await?;
                acked_to = Some(position);
                continue;
            }

            let (event, _) = record.into_parts();
            match event {
                Record::Done { .. } | Record::Failed { .. } | Record::Parked { .. } => {
                    self.feed.ack(&self.group, position).await?;
                }
                Record::Intent { intent, request } => {
                    if let Some(held) = self.work_intent(position, intent, request).await? {
                        return Ok(ExecutorStep::Holding { intent: held });
                    }
                }
            }
            acked_to = Some(position);
        }

        Ok(ExecutorStep::Advanced {
            acked_to: acked_to.expect("entries were nonempty and every entry acked or returned"),
        })
    }

    /// On replay, a stored success acknowledges before any port call or
    /// failure-budget handling, and a standing park settles before the
    /// port is called again. A failed attempt within the retry budget
    /// holds the cursor.
    async fn work_intent(
        &self,
        position: FeedPosition,
        intent: RenderedIntentKey,
        request: Effect,
    ) -> Result<Option<RenderedIntentKey>, ExecutorError<Hook::Error, Feed::Error, Store::Error>>
    {
        let state = self.durable_state(&intent).await?;
        let failures = match state {
            DurableState::Done => {
                self.feed.ack(&self.group, position).await?;
                return Ok(None);
            }
            DurableState::Parked { attempts, error } => {
                self.settle_park(position, &intent, request, attempts, error)
                    .await?;
                return Ok(None);
            }
            DurableState::Attempted { failed, last_error } => {
                if failed.get() >= self.policy.budget().get() {
                    self.park(position, &intent, request, failed, last_error)
                        .await?;
                    return Ok(None);
                }
                failed.get()
            }
            DurableState::Fresh => 0,
        };

        match self.port.perform(&intent, &request).await {
            Ok(()) => {
                self.append_outcome(Record::Done {
                    intent: intent.clone(),
                })
                .await?;
                self.feed.ack(&self.group, position).await?;
                Ok(None)
            }
            Err(port_err) => {
                let error = port_err.to_string();
                self.append_outcome(Record::Failed {
                    intent: intent.clone(),
                    error: error.clone(),
                })
                .await?;
                let attempts = failures + 1;
                if attempts < self.policy.budget().get() {
                    // The window holds: the failing entry stays
                    // unacked and redelivers after the backoff, order
                    // preserved.
                    let delay = self
                        .policy
                        .schedule()
                        .delays()
                        .nth(failures as usize)
                        .expect("the delay stream is unbounded");
                    tokio::time::sleep(delay).await;
                    return Ok(Some(intent));
                }

                let attempts = NonZeroU32::new(attempts)
                    .expect("the budget is nonzero and the count reached it");
                self.park(position, &intent, request, attempts, error)
                    .await?;
                Ok(None)
            }
        }
    }

    /// Rebuild one intent's replay state from its stored outcomes.
    async fn durable_state(
        &self,
        intent: &RenderedIntentKey,
    ) -> Result<DurableState, ExecutorError<Hook::Error, Feed::Error, Store::Error>> {
        let stored = match self.store.load_stream(&self.saga_key).await {
            Ok(StreamState::Present(batch)) => batch.into_records(),
            Ok(StreamState::Missing) => Vec::new(),
            Err(error) => return Err(ExecutorError::Read(error)),
        };
        let mut state = DurableState::Fresh;
        for record in stored {
            state = match (state, record.into_parts().0) {
                (state @ DurableState::Done, _) => state,
                (_, Record::Done { intent: done }) if &done == intent => DurableState::Done,
                (
                    _,
                    Record::Parked {
                        intent: parked,
                        attempts,
                        error,
                    },
                ) if &parked == intent => DurableState::Parked { attempts, error },
                (state @ DurableState::Parked { .. }, _) => state,
                (
                    DurableState::Attempted { failed: count, .. },
                    Record::Failed {
                        intent: failed,
                        error,
                    },
                ) if &failed == intent => DurableState::Attempted {
                    failed: count
                        .checked_add(1)
                        .expect("failed-attempt count exceeds u32"),
                    last_error: error,
                },
                (
                    _,
                    Record::Failed {
                        intent: failed,
                        error,
                    },
                ) if &failed == intent => DurableState::Attempted {
                    failed: NonZeroU32::MIN,
                    last_error: error,
                },
                // Outcomes for other keys never count toward this
                // intent.
                (state, Record::Failed { .. }) => state,
                (state, Record::Parked { .. }) => state,
                (state, Record::Done { .. }) => state,
                (state, Record::Intent { .. }) => state,
            };
        }
        Ok(state)
    }

    /// Persist the park before invoking the hook.
    async fn park(
        &self,
        position: FeedPosition,
        intent: &RenderedIntentKey,
        request: Effect,
        attempts: NonZeroU32,
        error: String,
    ) -> Result<(), ExecutorError<Hook::Error, Feed::Error, Store::Error>> {
        self.append_outcome(Record::Parked {
            intent: intent.clone(),
            attempts,
            error: error.clone(),
        })
        .await?;
        self.settle_park(position, intent, request, attempts, error)
            .await
    }

    /// Replays can invoke the hook again after a persisted park.
    async fn settle_park(
        &self,
        position: FeedPosition,
        intent: &RenderedIntentKey,
        request: Effect,
        attempts: NonZeroU32,
        error: String,
    ) -> Result<(), ExecutorError<Hook::Error, Feed::Error, Store::Error>> {
        let notice = ParkedNotice::new(intent.clone(), request, attempts, error);
        self.hook
            .on_parked(&notice)
            .await
            .map_err(ExecutorError::Hook)?;
        self.feed.ack(&self.group, position).await?;
        Ok(())
    }

    /// Preserve the append error so callers can distinguish retryable
    /// lock timeouts from conflicts.
    async fn append_outcome(
        &self,
        record: Record<Effect>,
    ) -> Result<(), ExecutorError<Hook::Error, Feed::Error, Store::Error>> {
        let batch = EventBatch::new(vec![record]).expect("one event is nonempty");
        self.store
            .append(ExpectedVersion::Any, &self.saga_key, &batch)
            .await?;
        Ok(())
    }

    /// Poll until an error; callers decide whether to restart.
    pub async fn run(
        &self,
        limit: PollLimit,
        interval: std::time::Duration,
    ) -> Result<(), ExecutorError<Hook::Error, Feed::Error, Store::Error>> {
        loop {
            self.step(limit).await?;
            tokio::time::sleep(interval).await;
        }
    }
}

/// Result of one executor poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutorStep {
    /// The poll delivered nothing; the cursor did not move.
    Idle,
    /// Work completed and the cursor advanced to here - performed,
    /// parked, acknowledged from a stored `Done`, and skipped entries
    /// included.
    Advanced {
        /// The position the group's cursor now stands at.
        acked_to: FeedPosition,
    },
    /// An intent failed inside its retry window: the cursor held at
    /// the failing intent.
    Holding {
        /// The failing intent's rendered key.
        intent: RenderedIntentKey,
    },
}

/// How an [`Executor::step`] can fail. Port failures are not
/// here: they are `Failed` records, the retry protocol's input.
#[derive(Debug, Error)]
pub enum ExecutorError<HookE, FeedE, StoreE>
where
    HookE: std::error::Error,
    FeedE: std::error::Error,
    StoreE: std::error::Error,
{
    /// The compensation hook rejected a parked notice. The parked
    /// record stands; the cursor does not advance, so the hook
    /// re-fires on the next step.
    #[error(transparent)]
    Hook(HookE),
    /// The outbox feed's poll failed.
    #[error(transparent)]
    Poll(FeedE),
    /// The outbox feed rejected the ack.
    #[error(transparent)]
    Ack(#[from] AckError<FeedE>),
    /// The replay-state read failed.
    #[error(transparent)]
    Read(StoreE),
    /// An outcome append failed. The append error's own variants stay
    /// intact: a lock timeout is retryable, a conflict is not.
    #[error(transparent)]
    Append(#[from] AppendError<StoreE>),
}
