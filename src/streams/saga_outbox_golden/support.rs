//! The shared golden harness: the consumer's types, the scripted saga,
//! the fold, the port, the hook, the rig, the expected-value builders,
//! and the retry-vocabulary constructor pins.

use std::collections::HashMap;
use std::fmt;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::decider::Event;
use crate::streams::feed::{ConsumerGroup, EventFeed, FeedPosition, PollLimit};
use crate::streams::in_memory::{
    InMemoryBatchBuilder, InMemoryDatabase, InMemoryEventFeed, InMemoryEventStreams,
};
use crate::streams::outbox::{
    CompensationHook, EffectPort, Executor, ParkedNotice, Record, RetryBudget, INTENT_METADATA_KEY,
    OUTBOX_CATEGORY,
};
use crate::streams::saga::{
    BackoffSchedule, CommandGroup, IntentGroup, Reaction, ReactionFold, RenderedIntentKey,
    RetryPolicy, Runner, Saga, SagaId,
};
use crate::streams::{
    EventBatch, EventMetadata, EventStreams, ExpectedVersion, RecordedEvent, StreamState,
};

pub(super) const SOURCE: &str = "orders";
pub(super) const LEDGER: &str = "ledger";

// ---------------------------------------------------------------------------
// The consumer's types
// ---------------------------------------------------------------------------

/// A source event the saga reacts to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum Src {
    Placed { item: u32 },
    Cancelled { item: u32 },
}

impl Event for Src {
    type EntityId = ();

    fn event_type(&self) -> String {
        "Src".to_owned()
    }

    fn get_id(&self) -> Self::EntityId {}
}

/// A command payload destined for a `ledger` stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Cmd {
    Reserved { item: u32 },
    Charged { item: u32 },
}

impl Event for Cmd {
    type EntityId = ();

    fn event_type(&self) -> String {
        "Cmd".to_owned()
    }

    fn get_id(&self) -> Self::EntityId {}
}

/// An effect payload performed through the port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Fx {
    Export { item: u32 },
    Notify { item: u32 },
}

// ---------------------------------------------------------------------------
// The consumer's saga, fold, port, and hook
// ---------------------------------------------------------------------------

/// A saga whose `react` is a scripted table: pure by construction,
/// the same event always yields the same reactions.
pub(super) struct Scripted {
    id: SagaId,
    script: HashMap<Src, Vec<Reaction<Cmd, Fx>>>,
}

impl Scripted {
    pub(super) fn new(id: &str, script: Vec<(Src, Vec<Reaction<Cmd, Fx>>)>) -> Self {
        Self {
            id: SagaId::new(id).expect("a test saga id is nonempty"),
            script: script.into_iter().collect(),
        }
    }
}

impl Saga for Scripted {
    type Event = Src;
    type Command = Cmd;
    type Effect = Fx;

    fn id(&self) -> &SagaId {
        &self.id
    }

    fn react(&self, event: &Src) -> Vec<Reaction<Cmd, Fx>> {
        self.script.get(event).cloned().unwrap_or_default()
    }
}

/// What the fold saw: one row per pushed group, so a golden can pin
/// ONE group per stream per batch, in reaction order.
#[derive(Default)]
pub(super) struct FoldLog {
    pub(super) command_groups: Mutex<Vec<(String, ExpectedVersion, usize)>>,
    pub(super) intent_groups: Mutex<Vec<(String, usize)>>,
}

/// The consumer's fold over the in-memory batch builder: erases typed
/// payloads at push, copies the payload key onto `Intent` records,
/// and attaches each record's envelope unchanged, per the
/// `ReactionFold` contract. Clones share the log, so the fixture reads
/// what the runner drove.
#[derive(Clone, Default)]
pub(super) struct MemFold {
    pub(super) log: Arc<FoldLog>,
}

/// The fold cannot fail in these fixtures; the error type exists
/// because the seam requires one.
#[derive(Debug)]
pub(super) struct FoldErr;

impl fmt::Display for FoldErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the fold rejected a group")
    }
}

impl std::error::Error for FoldErr {}

impl ReactionFold<InMemoryBatchBuilder> for MemFold {
    type Command = Cmd;
    type Effect = Fx;
    type Error = FoldErr;

    fn push_command_group(
        &self,
        builder: &mut InMemoryBatchBuilder,
        group: CommandGroup<'_, Cmd>,
    ) -> Result<(), FoldErr> {
        let records: Vec<RecordedEvent<Cmd>> = group
            .records()
            .iter()
            .map(|record| RecordedEvent::keyed(record.payload().clone(), record.metadata().clone()))
            .collect();
        let batch =
            EventBatch::from_records(records).expect("a command group holds at least one record");
        builder
            .write(
                group.stream().category(),
                &group.stream().key().to_owned(),
                group.expected(),
                &batch,
            )
            .expect("one group per stream per batch");
        self.log.command_groups.lock().unwrap().push((
            group.stream().to_string(),
            group.expected(),
            batch.records().len(),
        ));
        Ok(())
    }

    fn push_intent_group(
        &self,
        builder: &mut InMemoryBatchBuilder,
        group: IntentGroup<'_, Fx>,
    ) -> Result<(), FoldErr> {
        let records: Vec<RecordedEvent<Record<Fx>>> = group
            .records()
            .iter()
            .map(|record| {
                RecordedEvent::keyed(
                    Record::Intent {
                        intent: record.key().clone(),
                        request: record.payload().clone(),
                    },
                    record.metadata().clone(),
                )
            })
            .collect();
        let batch =
            EventBatch::from_records(records).expect("an intent group holds at least one record");
        builder
            .write(
                group.stream().category(),
                &group.stream().key().to_owned(),
                ExpectedVersion::Any,
                &batch,
            )
            .expect("one group per stream per batch");
        self.log
            .intent_groups
            .lock()
            .unwrap()
            .push((group.stream().to_string(), batch.records().len()));
        Ok(())
    }
}

/// A port that records every perform and can be scripted to fail a
/// fixed number of attempts per intent key. Clones share the record.
#[derive(Clone, Default)]
pub(super) struct Port {
    pub(super) calls: Arc<Mutex<Vec<(String, Fx)>>>,
    pub(super) fail_for: Arc<Mutex<HashMap<String, u32>>>,
}

/// The port's failure, rendered onto `Failed` records as diagnostics.
#[derive(Debug)]
pub(super) struct PortErr {
    reason: String,
}

impl fmt::Display for PortErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "port failure: {}", self.reason)
    }
}

impl std::error::Error for PortErr {}

impl PortErr {
    pub(super) fn flaky() -> Self {
        Self {
            reason: "flaky".to_owned(),
        }
    }

    pub(super) fn text(&self) -> String {
        self.to_string()
    }
}

impl EffectPort<Fx> for Port {
    type Error = PortErr;

    async fn perform(&self, intent: &RenderedIntentKey, request: &Fx) -> Result<(), PortErr> {
        let key = intent.as_str().to_owned();
        self.calls
            .lock()
            .unwrap()
            .push((key.clone(), request.clone()));
        let mut fail_for = self.fail_for.lock().unwrap();
        match fail_for.get_mut(&key) {
            Some(remaining) if *remaining > 0 => {
                *remaining -= 1;
                Err(PortErr::flaky())
            }
            _ => Ok(()),
        }
    }
}

/// A hook that records every fire and can be scripted to reject the
/// next fire. Clones share the record.
#[derive(Clone, Default)]
pub(super) struct Hook {
    pub(super) fires: Arc<Mutex<Vec<(String, u32, String)>>>,
    pub(super) reject_next: Arc<Mutex<bool>>,
}

#[derive(Debug)]
pub(super) struct HookErr;

impl fmt::Display for HookErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the hook rejected the notice")
    }
}

impl std::error::Error for HookErr {}

impl CompensationHook<Fx> for Hook {
    type Error = HookErr;

    async fn on_parked(&self, notice: &ParkedNotice<Fx>) -> Result<(), HookErr> {
        self.fires.lock().unwrap().push((
            notice.key().as_str().to_owned(),
            notice.attempts().get(),
            notice.error().to_owned(),
        ));
        if *self.reject_next.lock().unwrap() {
            *self.reject_next.lock().unwrap() = false;
            return Err(HookErr);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The rig
// ---------------------------------------------------------------------------

/// One shared-root database with the views the runner, the executor,
/// and the assertions need. Every fixture is single-threaded, so feed
/// positions are the log's 1-based indexes and stay deterministic.
pub(super) struct Rig {
    pub(super) db: InMemoryDatabase,
    pub(super) source: InMemoryEventStreams<Src>,
    pub(super) source_feed: InMemoryEventFeed<Src>,
    pub(super) outbox: InMemoryEventStreams<Record<Fx>>,
    pub(super) outbox_feed: InMemoryEventFeed<Record<Fx>>,
    pub(super) ledger: InMemoryEventStreams<Cmd>,
}

/// A scratch group the rig uses only to observe committed positions.
fn probe() -> ConsumerGroup {
    ConsumerGroup::new("probe").expect("a test group is nonempty")
}

pub(super) type MemRunner = Runner<
    InMemoryDatabase,
    InMemoryEventFeed<Src>,
    InMemoryEventStreams<Record<Fx>>,
    Scripted,
    MemFold,
>;

pub(super) type MemExecutor =
    Executor<InMemoryEventFeed<Record<Fx>>, InMemoryEventStreams<Record<Fx>>, Port, Hook, Fx>;

impl Rig {
    pub(super) fn new() -> Self {
        let db = InMemoryDatabase::new();
        let source = db.category::<Src>(SOURCE).expect("fresh source category");
        let source_feed = db.feed::<Src>(SOURCE).expect("claimed source category");
        let outbox = db
            .category::<Record<Fx>>(OUTBOX_CATEGORY)
            .expect("fresh outbox category");
        let outbox_feed = db
            .feed::<Record<Fx>>(OUTBOX_CATEGORY)
            .expect("claimed outbox category");
        let ledger = db.category::<Cmd>(LEDGER).expect("fresh ledger category");
        Self {
            db,
            source,
            source_feed,
            outbox,
            outbox_feed,
            ledger,
        }
    }

    /// Append one bare source event to `stream`; returns its committed
    /// position in the shared log.
    pub(super) async fn place_on(&self, stream: &str, event: Src) -> FeedPosition {
        let batch = EventBatch::new(vec![event]).expect("one event is nonempty");
        self.source
            .append(ExpectedVersion::Any, &stream.to_owned(), &batch)
            .await
            .expect("source append succeeds");
        let entries = self
            .source_feed
            .poll(&probe(), limit(1000))
            .await
            .expect("probe poll succeeds");
        entries
            .last()
            .expect("the append just landed in the log")
            .position()
    }

    pub(super) async fn place(&self, event: Src) -> FeedPosition {
        self.place_on("o-1", event).await
    }

    /// The last committed position in the outbox category.
    pub(super) async fn outbox_tip(&self) -> FeedPosition {
        let entries = self
            .outbox_feed
            .poll(&probe(), limit(1000))
            .await
            .expect("probe poll succeeds");
        entries
            .last()
            .expect("the outbox category holds at least one record")
            .position()
    }

    pub(super) fn runner(&self, saga: Scripted, fold: MemFold) -> MemRunner {
        Runner::new(
            saga,
            self.db.clone(),
            self.source_feed.clone(),
            self.outbox.clone(),
            fold,
            RetryPolicy::default(),
        )
    }

    pub(super) fn executor(&self, saga: &str, port: Port, hook: Hook, budget: u32) -> MemExecutor {
        Executor::new(
            self.outbox_feed.clone(),
            self.outbox.clone(),
            port,
            hook,
            SagaId::new(saga).expect("a test saga id is nonempty"),
            RetryPolicy::new(
                RetryBudget::new(budget).expect("a test budget is nonzero"),
                BackoffSchedule::new(Duration::from_millis(1), Duration::from_millis(2))
                    .expect("the test base does not exceed its cap"),
            ),
        )
    }

    /// The saga's outbox stream, whole: payload and envelope per
    /// record, oldest first.
    pub(super) async fn outbox_records(&self, saga: &str) -> Vec<(Record<Fx>, EventMetadata)> {
        records_of(&self.outbox, &saga.to_owned())
            .await
            .into_iter()
            .map(|record| {
                let (event, metadata) = record.into_parts();
                (event, metadata)
            })
            .collect()
    }

    /// One ledger stream, whole: payload and envelope per record.
    pub(super) async fn ledger_records(&self, stream: &str) -> Vec<(Cmd, EventMetadata)> {
        records_of(&self.ledger, &stream.to_owned())
            .await
            .into_iter()
            .map(|record| {
                let (event, metadata) = record.into_parts();
                (event, metadata)
            })
            .collect()
    }

    /// A bare event on a ledger stream, for pre-seeding conflicts.
    pub(super) async fn seed_ledger(&self, stream: &str, event: Cmd) {
        let batch = EventBatch::new(vec![event]).expect("one event is nonempty");
        self.ledger
            .append(ExpectedVersion::Any, &stream.to_owned(), &batch)
            .await
            .expect("ledger seed append succeeds");
    }

    /// A bare intent on a saga's outbox stream, keyed by rendered
    /// text, with the envelope a runner would have attached.
    pub(super) async fn seed_intent(&self, saga: &str, key: &str, request: Fx) {
        let record = RecordedEvent::keyed(
            Record::Intent {
                intent: RenderedIntentKey::from_rendered(key.to_owned()),
                request,
            },
            intent_envelope(key),
        );
        self.append_outbox(saga, record).await;
    }

    /// A bare failed record on a saga's outbox stream.
    pub(super) async fn seed_failed(&self, saga: &str, key: &str, error: &str) {
        let record = RecordedEvent::new(Record::Failed {
            intent: RenderedIntentKey::from_rendered(key.to_owned()),
            error: error.to_owned(),
        });
        self.append_outbox(saga, record).await;
    }

    /// A bare parked record on a saga's outbox stream.
    pub(super) async fn seed_parked(&self, saga: &str, key: &str, attempts: u32, error: &str) {
        let record = RecordedEvent::new(Record::Parked {
            intent: RenderedIntentKey::from_rendered(key.to_owned()),
            attempts: NonZeroU32::new(attempts).expect("a parked count is nonzero"),
            error: error.to_owned(),
        });
        self.append_outbox(saga, record).await;
    }

    async fn append_outbox(&self, saga: &str, record: RecordedEvent<Record<Fx>>) {
        let batch = EventBatch::from_records(vec![record]).expect("one record is nonempty");
        self.outbox
            .append(ExpectedVersion::Any, &saga.to_owned(), &batch)
            .await
            .expect("outbox seed append succeeds");
    }

    /// The positions still undelivered for `group`, oldest first:
    /// the observable cursor state.
    pub(super) async fn outstanding_for_group(&self, group: &str) -> Vec<u64> {
        let entries = self
            .source_feed
            .poll(
                &ConsumerGroup::new(group).expect("a test group is nonempty"),
                limit(1000),
            )
            .await
            .expect("probe poll succeeds");
        entries.iter().map(|entry| entry.position().get()).collect()
    }
}

async fn records_of<E>(store: &InMemoryEventStreams<E>, id: &String) -> Vec<RecordedEvent<E>>
where
    E: Event + Clone + Send + Sync + fmt::Debug + 'static,
{
    match store.load_stream(id).await.expect("load succeeds") {
        StreamState::Present(batch) => batch.into_records(),
        StreamState::Missing => Vec::new(),
    }
}

/// The expected envelope of one reaction record: exactly the intent
/// key under the framework-owned metadata name, and nothing else.
pub(super) fn intent_envelope(key: &str) -> EventMetadata {
    let mut metadata = EventMetadata::new();
    metadata.insert(INTENT_METADATA_KEY, key);
    metadata
}

/// The spec's rendering rule, restated for the goldens: inside every
/// component `\` renders as `\\` and `/` as `\/`, then the components
/// join with `/` as `saga/category/key/position/index`.
fn esc(component: &str) -> String {
    component.replace('\\', "\\\\").replace('/', "\\/")
}

pub(super) fn rendered(saga: &str, category: &str, key: &str, position: u64, index: u64) -> String {
    format!(
        "{}/{}/{}/{}/{}",
        esc(saga),
        esc(category),
        esc(key),
        position,
        index
    )
}

pub(super) fn limit(n: usize) -> PollLimit {
    PollLimit::new(n).expect("a test limit is nonzero")
}

pub(super) async fn wait_until<Fut>(probe: impl Fn() -> Fut)
where
    Fut: std::future::Future<Output = bool>,
{
    tokio::time::timeout(Duration::from_secs(5), async {
        while !probe().await {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the condition holds before the deadline");
}

/// A stream's current record count, through the public load.
pub(super) async fn stream_len<E>(store: &InMemoryEventStreams<E>, id: &str) -> usize
where
    E: Event + Clone + Send + Sync + fmt::Debug + 'static,
{
    match store
        .load_stream(&id.to_owned())
        .await
        .expect("load succeeds")
    {
        StreamState::Present(batch) => batch.records().len(),
        StreamState::Missing => 0,
    }
}
/// The retry and budget constructors enforce their documented rules,
/// and the default policy's delays double from the base without
/// exceeding the cap.
#[test]
fn retry_and_budget_constructors_pin_the_spec() {
    let base = Duration::from_millis(50);
    let cap = Duration::from_secs(2);
    assert!(
        BackoffSchedule::new(cap, base).is_err(),
        "a base above its cap is not a schedule"
    );
    assert!(RetryBudget::new(0).is_err());
    assert_eq!(RetryBudget::default().get(), 5);

    let policy = RetryPolicy::default();
    assert_eq!(policy.attempts(), 5);
    assert_eq!(
        policy.delays().collect::<Vec<_>>(),
        vec![
            base,
            Duration::from_millis(100),
            Duration::from_millis(200),
            Duration::from_millis(400),
        ],
        "attempts minus one waits, doubling from the base"
    );

    // A capped schedule saturates instead of overflowing.
    let tiny = BackoffSchedule::new(Duration::from_millis(3), Duration::from_millis(5))
        .expect("the base does not exceed the cap");
    assert_eq!(
        tiny.delays().take(4).collect::<Vec<_>>(),
        vec![
            Duration::from_millis(3),
            Duration::from_millis(5),
            Duration::from_millis(5),
            Duration::from_millis(5),
        ]
    );
}
