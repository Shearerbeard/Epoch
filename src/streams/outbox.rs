//! The outbox executor (E20; ADR 0010's outbox-saga section, landing
//! as that record's gate-A revision): a generic epoch runtime that
//! consumes a saga's outbox stream through the event feed, performs
//! effect intents through a caller-supplied port, and appends the
//! outcome events. Ordered at-least-once is the documented delivery
//! contract; idempotency is the consumer's documented obligation.
//!
//! Poison-intent policy: each intent gets a per-intent retry budget
//! (default 5, exponential backoff), derived durably from the
//! stream's own `Failed` records for the intent key, so it survives
//! executor crashes. During the retry window the group cursor HOLDS
//! at the failing intent - bounded, backoff-bounded blocking with
//! order preserved. On exhaustion the intent is parked
//! TERMINAL-FAILED on the outbox stream and the group advances past
//! it permanently; the compensation hook fires at park time.
//!
//! Every write the executor makes goes through the store's append
//! path, so the single-writer funnel's operating assumptions (ADR
//! 0010, and the postgres module doc) bind deployments unchanged.

use std::fmt;
use std::marker::PhantomData;
use std::num::NonZeroU32;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::decider::Event;

use super::feed::{AckError, ConsumerGroup, EventFeed, FeedPosition, PollLimit};
use super::saga::{BackoffSchedule, RenderedIntentKey, SagaId};
use super::{AppendError, EventBatch, EventStreams, ExpectedVersion, StreamState};

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
/// outcome of the executor's work on one. The category's event type
/// is this enum, so intents and outcomes share one stream and one
/// feed. Every record carries the intent key in its PAYLOAD - the
/// executor never reads envelopes - and `Intent` records additionally
/// carry the key in the ENVELOPE, because the storage-level
/// uniqueness index reads `event_metadata->>'intent'`. An outcome
/// carrying the key in its envelope would trip the same index that
/// rejects duplicate intents, so the envelope copy exists on intents
/// alone.
///
/// One category holds one event type, so every saga sharing this
/// category shares the effect payload type `F`: a deployment with
/// several sagas defines one consumer-wide effect enum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutboxEvent<F> {
    /// An effect intent, appended by the saga runner inside the
    /// reaction's atomic batch.
    Intent {
        /// The intent's rendered key; the runner also copies it into
        /// the envelope for the uniqueness index.
        intent: RenderedIntentKey,
        /// The effect payload the executor performs.
        request: F,
    },
    /// The effect succeeded.
    Done {
        /// The intent's rendered key.
        intent: RenderedIntentKey,
    },
    /// One perform attempt failed. The count of `Failed` records for
    /// a key IS the retry budget's durable state: it survives
    /// executor crashes because it is the stream itself.
    Failed {
        /// The intent's rendered key.
        intent: RenderedIntentKey,
        /// The port's error, rendered. Diagnostic-only: no domain
        /// logic branches on it.
        error: String,
    },
    /// TERMINAL-FAILED: the retry budget is exhausted and the group
    /// advances past this intent permanently. Carries the key so a
    /// crash between parking and ack replays as a no-op append. An
    /// audit fact, not a second trigger.
    Parked {
        /// The intent's rendered key.
        intent: RenderedIntentKey,
        /// The durable failed-attempt count at park time; a parked
        /// intent failed at least once.
        attempts: NonZeroU32,
        /// The last failure, rendered. Diagnostic-only.
        error: String,
    },
}

impl<F> Event for OutboxEvent<F> {
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

/// The per-intent retry budget: how many perform attempts an intent
/// gets before it parks TERMINAL-FAILED. Derived durably from the
/// stream's own `Failed` records for the intent key, so it survives
/// executor crashes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryBudget(NonZeroU32);

impl RetryBudget {
    /// Parse a budget; zero is not a budget.
    pub fn new(raw: u32) -> Result<Self, ZeroBudget> {
        NonZeroU32::new(raw).map(Self).ok_or(ZeroBudget)
    }

    /// The attempt count.
    pub fn get(self) -> u32 {
        self.0.get()
    }
}

impl Default for RetryBudget {
    /// The card's pinned default: 5 attempts.
    fn default() -> Self {
        Self(NonZeroU32::new(5).expect("5 is nonzero"))
    }
}

/// A retry budget of zero was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("a retry budget must allow at least one attempt")]
pub struct ZeroBudget;

/// The consumer's effect port: performs one effect request. Called
/// under at-least-once delivery - a crash between the external call
/// and the outcome append replays the intent, and the port's
/// idempotency absorbs the duplicate. That obligation is the
/// consumer's, documented here, and the port dedupes BY THE KEY: two
/// distinct reactions may carry identical payloads, so the intent key
/// is the only replay-safe identity.
#[trait_variant::make(Send)]
pub trait EffectPort<F> {
    /// Port failure type; rendered onto the `Failed` record as
    /// diagnostic text.
    type Error: std::error::Error + Send + Sync;

    /// Perform the effect. `intent` is the intent's rendered key -
    /// the port's idempotency identity.
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

    /// The parked intent's rendered key.
    pub fn key(&self) -> &RenderedIntentKey {
        &self.key
    }

    /// The effect payload that never landed.
    pub fn request(&self) -> &F {
        &self.request
    }

    /// The durable failed-attempt count at park time.
    pub fn attempts(&self) -> NonZeroU32 {
        self.attempts
    }

    /// The last failure, rendered. Diagnostic-only.
    pub fn error(&self) -> &str {
        &self.error
    }
}

/// The consumer's compensation action, fired by the executor when an
/// intent parks TERMINAL-FAILED. Fires at park time, in the executor
/// process; a crash between the park append and the hook re-fires it
/// on replay, so the hook carries the same idempotency obligation
/// effects carry. Parking records are audit facts, never a second
/// trigger.
#[trait_variant::make(Send)]
pub trait CompensationHook<F> {
    /// Hook failure type; surfaces as [`ExecutorError::Hook`].
    type Error: std::error::Error + Send + Sync;

    /// Fire the compensation for one parked intent.
    async fn on_parked(&self, notice: &ParkedNotice<F>) -> Result<(), Self::Error>;
}

/// The executor: polls the outbox category as the saga's executor
/// group, performs each of the saga's intents through the port, and
/// appends the outcome. Entries from other sagas' streams in the
/// category, and outcome records, are skipped and acked past. During
/// an intent's retry window the group cursor HOLDS at the failing
/// intent; on exhaustion the intent parks and the group advances past
/// it permanently.
///
/// Type parameters: `Fe` the outbox feed, `St` the outbox stream's
/// store view (outcome appends and the failure count), `P` the port,
/// `K` the compensation hook, `F` the effect payload.
pub struct OutboxExecutor<Fe, St, P, K, F> {
    feed: Fe,
    store: St,
    port: P,
    hook: K,
    saga: SagaId,
    group: ConsumerGroup,
    budget: RetryBudget,
    backoff: BackoffSchedule,
    _marker: PhantomData<fn() -> F>,
}

impl<Fe, St, P, K, F> OutboxExecutor<Fe, St, P, K, F> {
    /// Assemble an executor for one saga's outbox stream. The group
    /// derives from the saga id: one executor group per saga, its
    /// cursor independent of the runner's (group progress is scoped
    /// per category).
    pub fn new(
        feed: Fe,
        store: St,
        port: P,
        hook: K,
        saga: SagaId,
        budget: RetryBudget,
        backoff: BackoffSchedule,
    ) -> Self {
        let group = ConsumerGroup::new(saga.as_str()).expect("a saga id is a non-empty group name");
        Self {
            feed,
            store,
            port,
            hook,
            saga,
            group,
            budget,
            backoff,
            _marker: PhantomData,
        }
    }

    /// The saga whose outbox stream this executor works.
    pub fn saga(&self) -> &SagaId {
        &self.saga
    }

    /// The executor's consumer group.
    pub fn group(&self) -> &ConsumerGroup {
        &self.group
    }

    /// The per-intent retry budget.
    pub fn budget(&self) -> RetryBudget {
        self.budget
    }

    /// The backoff schedule inside a retry window.
    pub fn backoff(&self) -> BackoffSchedule {
        self.backoff
    }
}

impl<Fe, St, P, K, F> OutboxExecutor<Fe, St, P, K, F>
where
    Fe: EventFeed<OutboxEvent<F>>,
    St: EventStreams<OutboxEvent<F>, Id = String>,
    P: EffectPort<F>,
    K: CompensationHook<F>,
    F: Send + Sync + fmt::Debug,
{
    /// One poll-perform-record cycle over up to `limit` delivered
    /// entries. For one of this saga's intents - the key read from
    /// the record's payload, never the envelope: perform through the
    /// port; on success append `Done` and ack; on failure append
    /// `Failed` and, under budget, HOLD the cursor (the next poll
    /// redelivers the intent after the backoff); at the budget,
    /// append `Parked`, fire the hook, and ack past the intent
    /// permanently. A replay that finds the `Parked` record already
    /// present re-fires the hook (idempotent under replay) and acks:
    /// the append is the no-op.
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
                // Outcome records are audit facts, not triggers.
                OutboxEvent::Done { .. }
                | OutboxEvent::Failed { .. }
                | OutboxEvent::Parked { .. } => {
                    self.feed.ack(&self.group, position).await?;
                    acked_to = Some(position);
                }
                OutboxEvent::Intent { intent, request } => {
                    // The budget's durable state is the saga's own
                    // stream: its `Failed` records for this intent key.
                    let stored = match self.store.load_stream(&self.saga.as_str().to_owned()).await
                    {
                        Ok(StreamState::Present(batch)) => batch.into_records(),
                        Ok(StreamState::Missing) => Vec::new(),
                        Err(error) => return Err(ExecutorError::Read(error)),
                    };
                    let mut failed_count = 0u32;
                    let mut last_failed_error: Option<String> = None;
                    let mut parked: Option<(NonZeroU32, String)> = None;
                    for record in stored {
                        let (event, _) = record.into_parts();
                        match event {
                            OutboxEvent::Failed {
                                intent: failed,
                                error,
                            } if failed == intent => {
                                failed_count += 1;
                                last_failed_error = Some(error);
                            }
                            OutboxEvent::Parked {
                                intent: parked_key,
                                attempts,
                                error,
                            } if parked_key == intent => {
                                parked = Some((attempts, error));
                            }
                            _ => {}
                        }
                    }

                    // The crash between park and ack, replayed: the
                    // park stands, the append is the no-op, and the
                    // hook - the park's idempotent side effect -
                    // re-fires before the entry acks.
                    if let Some((attempts, error)) = parked {
                        let notice = ParkedNotice::new(intent, request, attempts, error);
                        self.hook
                            .on_parked(&notice)
                            .await
                            .map_err(ExecutorError::Hook)?;
                        self.feed.ack(&self.group, position).await?;
                        acked_to = Some(position);
                        continue;
                    }

                    // The crash between the last Failed and the Parked
                    // append, replayed: the budget is spent durably, so
                    // park now - the port is never asked past it.
                    if failed_count >= self.budget.get() {
                        let attempts = NonZeroU32::new(failed_count)
                            .expect("the budget is nonzero and the count reached it");
                        let error = last_failed_error.unwrap_or_default();
                        let park = OutboxEvent::Parked {
                            intent: intent.clone(),
                            attempts,
                            error: error.clone(),
                        };
                        let batch = EventBatch::new(vec![park]).expect("one event is nonempty");
                        self.store
                            .append(ExpectedVersion::Any, &self.saga.as_str().to_owned(), &batch)
                            .await?;
                        let notice = ParkedNotice::new(intent, request, attempts, error);
                        self.hook
                            .on_parked(&notice)
                            .await
                            .map_err(ExecutorError::Hook)?;
                        self.feed.ack(&self.group, position).await?;
                        acked_to = Some(position);
                        continue;
                    }

                    match self.port.perform(&intent, &request).await {
                        Ok(()) => {
                            let done = OutboxEvent::Done {
                                intent: intent.clone(),
                            };
                            let batch = EventBatch::new(vec![done]).expect("one event is nonempty");
                            self.store
                                .append(
                                    ExpectedVersion::Any,
                                    &self.saga.as_str().to_owned(),
                                    &batch,
                                )
                                .await?;
                            self.feed.ack(&self.group, position).await?;
                            acked_to = Some(position);
                        }
                        Err(port_err) => {
                            let attempts = failed_count + 1;
                            let failure = OutboxEvent::Failed {
                                intent: intent.clone(),
                                error: port_err.to_string(),
                            };
                            let batch =
                                EventBatch::new(vec![failure]).expect("one event is nonempty");
                            self.store
                                .append(
                                    ExpectedVersion::Any,
                                    &self.saga.as_str().to_owned(),
                                    &batch,
                                )
                                .await?;
                            if attempts < self.budget.get() {
                                // The retry window holds: the failing
                                // entry stays unacked and redelivers
                                // after the backoff, order preserved.
                                let delay = self
                                    .backoff
                                    .delays()
                                    .nth(failed_count as usize)
                                    .expect("the delay stream is unbounded");
                                tokio::time::sleep(delay).await;
                                return Ok(ExecutorStep::Holding { intent });
                            }

                            // The budget is spent on this fresh
                            // failure: park and fire the hook.
                            let attempts = NonZeroU32::new(attempts)
                                .expect("the budget is nonzero and the count reached it");
                            let error = port_err.to_string();
                            let park = OutboxEvent::Parked {
                                intent: intent.clone(),
                                attempts,
                                error: error.clone(),
                            };
                            let batch = EventBatch::new(vec![park]).expect("one event is nonempty");
                            self.store
                                .append(
                                    ExpectedVersion::Any,
                                    &self.saga.as_str().to_owned(),
                                    &batch,
                                )
                                .await?;
                            let notice = ParkedNotice::new(intent, request, attempts, error);
                            self.hook
                                .on_parked(&notice)
                                .await
                                .map_err(ExecutorError::Hook)?;
                            self.feed.ack(&self.group, position).await?;
                            acked_to = Some(position);
                        }
                    }
                }
            }
        }

        Ok(ExecutorStep::Advanced {
            acked_to: acked_to.expect("entries were nonempty and every entry acked or returned"),
        })
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

/// What one [`OutboxExecutor::step`] did.
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
    /// An intent failed inside its retry window: its `Failed` record
    /// is appended and the cursor HOLDS at the intent until the
    /// backoff expires.
    Holding {
        /// The failing intent's rendered key.
        intent: RenderedIntentKey,
    },
}

/// How an [`OutboxExecutor::step`] can fail. Port failures are not
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
