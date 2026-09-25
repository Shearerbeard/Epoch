//! The outbox executor (ADR 0010's outbox-saga section): a generic
//! runtime driving the outbox loop - poll the event feed, perform
//! each intent through the caller-supplied port, append the outcome
//! record. Delivery is ordered at-least-once; idempotency is the
//! consumer's documented obligation.
//!
//! Each intent carries a per-intent retry budget (default 5,
//! exponential backoff) derived durably from the stream's own
//! `Failed` records for the intent key, so it survives executor
//! crashes. Inside the window the group cursor HOLDS at the failing
//! intent - backoff-bounded blocking with order preserved; on
//! exhaustion the intent parks TERMINAL-FAILED, the compensation hook
//! fires once at park time, and the group advances past permanently.
//! Every write goes through the store's append path, so the
//! single-writer funnel's assumptions bind deployments unchanged.

use std::fmt;
use std::marker::PhantomData;
use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::decider::Event;

use super::feed::{AckError, ConsumerGroup, EventFeed, FeedPosition, PollLimit};
use super::keys::{RenderedIntentKey, SagaId};
use super::retry::{BackoffSchedule, RetryPolicy};
use super::{AppendError, EventBatch, EventStreams, ExpectedVersion, StreamState};

// The retry vocabulary lives in the private `retry` module; this
// re-export keeps the paths this module published before the split
// resolving.
pub use super::retry::{RetryBudget, ZeroBudget};

/// The outbox stream category, pinned framework-owned by ADR 0010's
/// outbox-saga section: the storage-level uniqueness index
/// (`0004-feed-cursors-and-intent-key.sql`) reads this name, so a
/// rename lands as its own migration step.
pub const OUTBOX_CATEGORY: &str = "saga-outbox";

/// The envelope key an intent's identity rides. The storage-level
/// uniqueness index reads exactly this expression
/// (`event_metadata->>'intent'`), so the key name is framework-owned
/// and never consumer-configurable.
pub const INTENT_METADATA_KEY: &str = "intent";

/// One record on a saga's outbox stream: an effect intent, or the
/// executor's outcome for one. Every record carries the intent key in
/// its PAYLOAD - the executor never reads envelopes - while `Intent`
/// records carry it in the ENVELOPE too, because the uniqueness index
/// reads `event_metadata->>'intent'`; an outcome envelope key would
/// trip that same index.
///
/// One category holds one event type, so every saga sharing this
/// category shares the effect payload type `F`: a deployment with
/// several sagas defines one consumer-wide effect enum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Record<F> {
    /// An effect intent, appended by the saga runner inside the
    /// reaction's atomic batch.
    Intent {
        intent: RenderedIntentKey,
        request: F,
    },
    Done {
        intent: RenderedIntentKey,
    },
    /// One perform attempt failed. The count of `Failed` records for
    /// a key IS the retry budget's durable state: it survives
    /// executor crashes because it is the stream itself.
    Failed {
        intent: RenderedIntentKey,
        /// The port's error, rendered; diagnostic only.
        error: String,
    },
    /// TERMINAL-FAILED: the retry budget is exhausted and the group
    /// advances past this intent permanently. Carries the key so a
    /// crash between parking and ack replays as a no-op append. An
    /// audit fact, not a second trigger.
    Parked {
        intent: RenderedIntentKey,
        /// The durable failed-attempt count at park time.
        attempts: NonZeroU32,
        /// The last failure, rendered; diagnostic only.
        error: String,
    },
}

impl<F> Event for Record<F> {
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
pub trait EffectPort<F> {
    /// Port failure type; rendered onto the `Failed` record as
    /// diagnostic text.
    type Error: std::error::Error + Send + Sync;

    /// `intent` is the port's idempotency identity.
    async fn perform(&self, intent: &RenderedIntentKey, request: &F) -> Result<(), Self::Error>;
}

/// What the compensation hook learns at park time: the parked intent,
/// its durable attempt count, and the last failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParkedNotice<F> {
    key: RenderedIntentKey,
    request: F,
    attempts: NonZeroU32,
    error: String,
}

impl<F> ParkedNotice<F> {
    pub(crate) fn new(
        key: RenderedIntentKey,
        request: F,
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
    pub fn request(&self) -> &F {
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
pub trait CompensationHook<F> {
    /// Hook failure type; surfaces as [`ExecutorError::Hook`].
    type Error: std::error::Error + Send + Sync;

    async fn on_parked(&self, notice: &ParkedNotice<F>) -> Result<(), Self::Error>;
}

/// The executor: polls the outbox category as the saga's executor
/// group, performs each of the saga's intents through the port, and
/// appends the outcome. Entries from other sagas' streams in the
/// category, and outcome records, are skipped and acked past.
///
/// Type parameters: `Fe` the outbox feed, `St` the outbox stream's
/// store view (outcome appends and the failure count), `P` the port,
/// `K` the compensation hook, `F` the effect payload.
pub struct Executor<Fe, St, P, K, F> {
    feed: Fe,
    store: St,
    port: P,
    hook: K,
    saga: SagaId,
    group: ConsumerGroup,
    saga_key: String,
    policy: RetryPolicy,
    _marker: PhantomData<fn() -> F>,
}

impl<Fe, St, P, K, F> Executor<Fe, St, P, K, F> {
    /// Assemble an executor for one saga's outbox stream. The group
    /// derives from the saga id: one executor group per saga, its
    /// cursor independent of the runner's (group progress is scoped
    /// per category).
    pub fn new(feed: Fe, store: St, port: P, hook: K, saga: SagaId, policy: RetryPolicy) -> Self {
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

/// One intent's durable retry state on the outbox stream: what the
/// executor's decisions derive from, surviving its crashes.
#[derive(Default)]
struct DurableState {
    failed: u32,
    last_error: Option<String>,
    parked: Option<(NonZeroU32, String)>,
}

impl<Fe, St, P, K, F> Executor<Fe, St, P, K, F>
where
    Fe: EventFeed<Record<F>>,
    St: EventStreams<Record<F>, Id = String>,
    P: EffectPort<F>,
    K: CompensationHook<F>,
    F: Send + Sync + fmt::Debug,
{
    /// One poll-perform-record cycle over up to `limit` delivered
    /// entries. This saga's intents perform through the port - the key
    /// read from the record's payload, never the envelope - with
    /// `Done`/`Failed`/`Parked` recording the outcome; other sagas'
    /// streams in the shared category and outcome records are skipped
    /// and acked past.
    pub async fn step(
        &self,
        limit: PollLimit,
    ) -> Result<ExecutorStep, ExecutorError<K::Error, Fe::Error, St::Error>> {
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

    /// Work one of this saga's intents: settle from the stream's
    /// durable state when the budget is already spent - a standing
    /// park re-fires the idempotent hook and acks; a spent count with
    /// no park parks now, the port never asked past the budget.
    /// Otherwise perform through the port and record the outcome.
    /// Returns the held key when the failure stays inside the retry
    /// window.
    async fn work_intent(
        &self,
        position: FeedPosition,
        intent: RenderedIntentKey,
        request: F,
    ) -> Result<Option<RenderedIntentKey>, ExecutorError<K::Error, Fe::Error, St::Error>> {
        let state = self.durable_state(&intent).await?;
        if let Some((attempts, error)) = state.parked {
            self.settle_park(position, &intent, request, attempts, error)
                .await?;
            return Ok(None);
        }
        if state.failed >= self.policy.budget().get() {
            let attempts = NonZeroU32::new(state.failed)
                .expect("the budget is nonzero and the count reached it");
            let error = state.last_error.expect(
                "the count reached the budget by counting this key's Failed records, \
                 each of which set the last failure",
            );
            self.park(position, &intent, request, attempts, error)
                .await?;
            return Ok(None);
        }

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
                self.append_outcome(Record::Failed {
                    intent: intent.clone(),
                    error: port_err.to_string(),
                })
                .await?;
                let attempts = state.failed + 1;
                if attempts < self.policy.budget().get() {
                    // The window holds: the failing entry stays
                    // unacked and redelivers after the backoff, order
                    // preserved.
                    let delay = self
                        .policy
                        .schedule()
                        .delays()
                        .nth(state.failed as usize)
                        .expect("the delay stream is unbounded");
                    tokio::time::sleep(delay).await;
                    return Ok(Some(intent));
                }

                let attempts = NonZeroU32::new(attempts)
                    .expect("the budget is nonzero and the count reached it");
                let error = port_err.to_string();
                self.park(position, &intent, request, attempts, error)
                    .await?;
                Ok(None)
            }
        }
    }

    /// The intent's durable retry state, read from the saga's own
    /// stream: its `Failed` count for the key, the last failure, and
    /// a standing `Parked` record if the park already landed.
    async fn durable_state(
        &self,
        intent: &RenderedIntentKey,
    ) -> Result<DurableState, ExecutorError<K::Error, Fe::Error, St::Error>> {
        let stored = match self.store.load_stream(&self.saga_key).await {
            Ok(StreamState::Present(batch)) => batch.into_records(),
            Ok(StreamState::Missing) => Vec::new(),
            Err(error) => return Err(ExecutorError::Read(error)),
        };
        let mut state = DurableState::default();
        for record in stored {
            match record.into_parts().0 {
                Record::Failed {
                    intent: failed,
                    error,
                } if &failed == intent => {
                    state.failed += 1;
                    state.last_error = Some(error);
                }
                Record::Parked {
                    intent: parked,
                    attempts,
                    error,
                } if &parked == intent => {
                    state.parked = Some((attempts, error));
                }
                // Other keys' outcomes never count toward this
                // intent's budget.
                Record::Failed { .. } | Record::Parked { .. } => {}
                Record::Intent { .. } | Record::Done { .. } => {}
            }
        }
        Ok(state)
    }

    /// Append the `Parked` record, then settle the park.
    async fn park(
        &self,
        position: FeedPosition,
        intent: &RenderedIntentKey,
        request: F,
        attempts: NonZeroU32,
        error: String,
    ) -> Result<(), ExecutorError<K::Error, Fe::Error, St::Error>> {
        self.append_outcome(Record::Parked {
            intent: intent.clone(),
            attempts,
            error: error.clone(),
        })
        .await?;
        self.settle_park(position, intent, request, attempts, error)
            .await
    }

    /// Fire the compensation hook and ack past the intent. A crash
    /// between the park append and the hook re-fires it here on
    /// replay - the hook carries the same idempotency obligation the
    /// port does.
    async fn settle_park(
        &self,
        position: FeedPosition,
        intent: &RenderedIntentKey,
        request: F,
        attempts: NonZeroU32,
        error: String,
    ) -> Result<(), ExecutorError<K::Error, Fe::Error, St::Error>> {
        let notice = ParkedNotice::new(intent.clone(), request, attempts, error);
        self.hook
            .on_parked(&notice)
            .await
            .map_err(ExecutorError::Hook)?;
        self.feed.ack(&self.group, position).await?;
        Ok(())
    }

    /// Append one outcome record to the saga's outbox stream. The
    /// append error stays whole: a lock timeout remains retryable, a
    /// conflict does not.
    async fn append_outcome(
        &self,
        record: Record<F>,
    ) -> Result<(), ExecutorError<K::Error, Fe::Error, St::Error>> {
        let batch = EventBatch::new(vec![record]).expect("one event is nonempty");
        self.store
            .append(ExpectedVersion::Any, &self.saga_key, &batch)
            .await?;
        Ok(())
    }

    /// The poll loop: `step` forever, sleeping `interval` between
    /// polls. Errors propagate; restarting the loop is the operator's
    /// call.
    pub async fn run(
        &self,
        limit: PollLimit,
        interval: std::time::Duration,
    ) -> Result<(), ExecutorError<K::Error, Fe::Error, St::Error>> {
        loop {
            self.step(limit).await?;
            tokio::time::sleep(interval).await;
        }
    }
}

/// What one [`Executor::step`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutorStep {
    /// The poll delivered nothing; the cursor did not move.
    Idle,
    /// Work completed and the cursor advanced to here - performed,
    /// parked, and skipped entries included.
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
    /// The failure-count read failed.
    #[error(transparent)]
    Read(StoreE),
    /// An outcome append failed. The append error's own variants stay
    /// intact: a lock timeout is retryable, a conflict is not.
    #[error(transparent)]
    Append(#[from] AppendError<StoreE>),
}
