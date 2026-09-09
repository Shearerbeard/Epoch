//! E20 golden frames (Layer 2, typed-holes): whole-frame spec tests
//! for the saga runner and the outbox executor, written from the E20
//! card's four-round reviewed spec BEFORE the bodies were filled, so
//! every fixture that crosses a hole fails on arrival. The coverage
//! manifest, including the exclusion rows, lives in
//! `docs/design/e20-saga-outbox-DESIGN.md`.
//!
//! Whole frame means the complete stored state, not substrings: each
//! assertion compares a full stream's records - payloads and envelopes
//! together - against an expected `RecordedEvent` list built by hand
//! from the spec.
//!
//! ```sh
//! cargo test --lib saga_outbox_golden
//! ```

use std::collections::HashMap;
use std::fmt;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::decider::Event;
use crate::streams::batch::{AtomicStreams, StreamRef};
use crate::streams::feed::{ConsumerGroup, EventFeed, FeedPosition, PollLimit};
use crate::streams::in_memory::{
    InMemoryBatchBuilder, InMemoryDatabase, InMemoryEventFeed, InMemoryEventStreams,
};
use crate::streams::outbox::{
    CompensationHook, EffectPort, ExecutorError, ExecutorStep, OutboxEvent, OutboxExecutor,
    ParkedNotice, RetryBudget, INTENT_METADATA_KEY, OUTBOX_CATEGORY,
};
use crate::streams::saga::{
    BackoffSchedule, Command, CommandGroup, EffectRequest, IntentGroup, Reaction, ReactionFold,
    RenderedIntentKey, RetryPolicy, RunnerStep, Saga, SagaError, SagaId, SagaRunner,
};
use crate::streams::{
    EventBatch, EventMetadata, EventStreams, ExpectedVersion, RecordedEvent, StreamState,
};

const SOURCE: &str = "orders";
const LEDGER: &str = "ledger";

// ---------------------------------------------------------------------------
// The consumer's types
// ---------------------------------------------------------------------------

/// A source event the saga reacts to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Src {
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
enum Cmd {
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
enum Fx {
    Export { item: u32 },
    Notify { item: u32 },
}

// ---------------------------------------------------------------------------
// The consumer's saga, fold, port, and hook
// ---------------------------------------------------------------------------

/// A saga whose `react` is a scripted table: pure by construction,
/// the same event always yields the same reactions.
struct Scripted {
    id: SagaId,
    script: HashMap<Src, Vec<Reaction<Cmd, Fx>>>,
}

impl Scripted {
    fn new(id: &str, script: Vec<(Src, Vec<Reaction<Cmd, Fx>>)>) -> Self {
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
struct FoldLog {
    command_groups: Mutex<Vec<(String, ExpectedVersion, usize)>>,
    intent_groups: Mutex<Vec<(String, usize)>>,
}

/// The consumer's fold over the in-memory batch builder: erases typed
/// payloads at push, copies the payload key onto `Intent` records,
/// and attaches each record's envelope unchanged, per the
/// `ReactionFold` contract. Clones share the log, so the fixture reads
/// what the runner drove.
#[derive(Clone, Default)]
struct MemFold {
    log: Arc<FoldLog>,
}

/// The fold cannot fail in these fixtures; the error type exists
/// because the seam requires one.
#[derive(Debug)]
struct FoldErr;

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
        let records: Vec<RecordedEvent<OutboxEvent<Fx>>> = group
            .records()
            .iter()
            .map(|record| {
                RecordedEvent::keyed(
                    OutboxEvent::Intent {
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
struct Port {
    calls: Arc<Mutex<Vec<(String, Fx)>>>,
    fail_for: Arc<Mutex<HashMap<String, u32>>>,
}

/// The port's failure, rendered onto `Failed` records as diagnostics.
#[derive(Debug)]
struct PortErr {
    reason: String,
}

impl fmt::Display for PortErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "port failure: {}", self.reason)
    }
}

impl std::error::Error for PortErr {}

impl PortErr {
    fn flaky() -> Self {
        Self {
            reason: "flaky".to_owned(),
        }
    }

    fn text(&self) -> String {
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
struct Hook {
    fires: Arc<Mutex<Vec<(String, u32, String)>>>,
    reject_next: Arc<Mutex<bool>>,
}

#[derive(Debug)]
struct HookErr;

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
struct Rig {
    db: InMemoryDatabase,
    source: InMemoryEventStreams<Src>,
    source_feed: InMemoryEventFeed<Src>,
    outbox: InMemoryEventStreams<OutboxEvent<Fx>>,
    outbox_feed: InMemoryEventFeed<OutboxEvent<Fx>>,
    ledger: InMemoryEventStreams<Cmd>,
}

/// A scratch group the rig uses only to observe committed positions.
fn probe() -> ConsumerGroup {
    ConsumerGroup::new("probe").expect("a test group is nonempty")
}

impl Rig {
    fn new() -> Self {
        Self::with_source_category(SOURCE)
    }

    fn with_source_category(name: &str) -> Self {
        let db = InMemoryDatabase::new();
        let source = db.category::<Src>(name).expect("fresh source category");
        let source_feed = db.feed::<Src>(name).expect("claimed source category");
        let outbox = db
            .category::<OutboxEvent<Fx>>(OUTBOX_CATEGORY)
            .expect("fresh outbox category");
        let outbox_feed = db
            .feed::<OutboxEvent<Fx>>(OUTBOX_CATEGORY)
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
    async fn place_on(&self, stream: &str, event: Src) -> FeedPosition {
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

    async fn place(&self, event: Src) -> FeedPosition {
        self.place_on("o-1", event).await
    }

    /// The last committed position in the outbox category.
    async fn outbox_tip(&self) -> FeedPosition {
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

    #[allow(clippy::type_complexity)]
    fn runner(
        &self,
        saga: Scripted,
        fold: MemFold,
    ) -> SagaRunner<
        InMemoryDatabase,
        InMemoryEventFeed<Src>,
        InMemoryEventStreams<OutboxEvent<Fx>>,
        Scripted,
        MemFold,
    > {
        SagaRunner::new(
            saga,
            self.db.clone(),
            self.source_feed.clone(),
            self.outbox.clone(),
            fold,
            RetryPolicy::default(),
        )
    }

    #[allow(clippy::type_complexity)]
    fn executor(
        &self,
        saga: &str,
        port: Port,
        hook: Hook,
        budget: u32,
    ) -> OutboxExecutor<
        InMemoryEventFeed<OutboxEvent<Fx>>,
        InMemoryEventStreams<OutboxEvent<Fx>>,
        Port,
        Hook,
        Fx,
    > {
        OutboxExecutor::new(
            self.outbox_feed.clone(),
            self.outbox.clone(),
            port,
            hook,
            SagaId::new(saga).expect("a test saga id is nonempty"),
            RetryBudget::new(budget).expect("a test budget is nonzero"),
            BackoffSchedule::new(Duration::from_millis(1), Duration::from_millis(2))
                .expect("the test base does not exceed its cap"),
        )
    }

    /// The saga's outbox stream, whole: payload and envelope per
    /// record, oldest first.
    async fn outbox_records(&self, saga: &str) -> Vec<(OutboxEvent<Fx>, EventMetadata)> {
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
    async fn ledger_records(&self, stream: &str) -> Vec<(Cmd, EventMetadata)> {
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
    async fn seed_ledger(&self, stream: &str, event: Cmd) {
        let batch = EventBatch::new(vec![event]).expect("one event is nonempty");
        self.ledger
            .append(ExpectedVersion::Any, &stream.to_owned(), &batch)
            .await
            .expect("ledger seed append succeeds");
    }

    /// A bare intent on a saga's outbox stream, keyed by rendered
    /// text, with the envelope a runner would have attached.
    async fn seed_intent(&self, saga: &str, key: &str, request: Fx) {
        let record = RecordedEvent::keyed(
            OutboxEvent::Intent {
                intent: RenderedIntentKey::from_rendered(key.to_owned()),
                request,
            },
            intent_envelope(key),
        );
        self.append_outbox(saga, record).await;
    }

    /// A bare failed record on a saga's outbox stream.
    async fn seed_failed(&self, saga: &str, key: &str, error: &str) {
        let record = RecordedEvent::new(OutboxEvent::Failed {
            intent: RenderedIntentKey::from_rendered(key.to_owned()),
            error: error.to_owned(),
        });
        self.append_outbox(saga, record).await;
    }

    /// A bare parked record on a saga's outbox stream.
    async fn seed_parked(&self, saga: &str, key: &str, attempts: u32, error: &str) {
        let record = RecordedEvent::new(OutboxEvent::Parked {
            intent: RenderedIntentKey::from_rendered(key.to_owned()),
            attempts: NonZeroU32::new(attempts).expect("a parked count is nonzero"),
            error: error.to_owned(),
        });
        self.append_outbox(saga, record).await;
    }

    async fn append_outbox(&self, saga: &str, record: RecordedEvent<OutboxEvent<Fx>>) {
        let batch = EventBatch::from_records(vec![record]).expect("one record is nonempty");
        self.outbox
            .append(ExpectedVersion::Any, &saga.to_owned(), &batch)
            .await
            .expect("outbox seed append succeeds");
    }

    /// The positions still undelivered for `group`, oldest first:
    /// the observable cursor state.
    async fn outstanding_for_group(&self, group: &str) -> Vec<u64> {
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
fn intent_envelope(key: &str) -> EventMetadata {
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

fn rendered(saga: &str, category: &str, key: &str, position: u64, index: u64) -> String {
    format!(
        "{}/{}/{}/{}/{}",
        esc(saga),
        esc(category),
        esc(key),
        position,
        index
    )
}

fn limit(n: usize) -> PollLimit {
    PollLimit::new(n).expect("a test limit is nonzero")
}

async fn wait_until<Fut>(probe: impl Fn() -> Fut)
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
async fn stream_len<E>(store: &InMemoryEventStreams<E>, id: &str) -> usize
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

// ---------------------------------------------------------------------------
// Runner fixtures
// ---------------------------------------------------------------------------

/// One event, two command arms on two streams and one effect arm: the
/// step commits every stream in one batch, acks the entry, and every
/// reaction record carries its minted key in its envelope.
#[tokio::test]
async fn runner_step_folds_reactions_into_one_batch_and_acks() {
    let rig = Rig::new();
    let position = rig.place(Src::Placed { item: 7 }).await;

    let saga = Scripted::new(
        "saga-1",
        vec![(
            Src::Placed { item: 7 },
            vec![
                Reaction::Command(Command::new(
                    StreamRef::new(LEDGER, &"o-1".to_owned()),
                    ExpectedVersion::NoStream,
                    Cmd::Reserved { item: 7 },
                )),
                Reaction::Command(Command::new(
                    StreamRef::new(LEDGER, &"o-2".to_owned()),
                    ExpectedVersion::NoStream,
                    Cmd::Charged { item: 7 },
                )),
                Reaction::EffectRequest(EffectRequest::new(Fx::Export { item: 7 })),
            ],
        )],
    );
    let fold = MemFold::default();
    let runner = rig.runner(saga, fold.clone());

    let step = runner.step(limit(10)).await.expect("the step succeeds");
    assert_eq!(
        step,
        RunnerStep::Advanced { acked_to: position },
        "the entry is processed and its position acked"
    );

    // Whole streams: the command records carry their own keys, and the
    // intent carries its key in the payload AND the envelope.
    let key0 = rendered("saga-1", SOURCE, "o-1", position.get(), 0);
    let key1 = rendered("saga-1", SOURCE, "o-1", position.get(), 1);
    let key2 = rendered("saga-1", SOURCE, "o-1", position.get(), 2);
    assert_eq!(
        rig.ledger_records("o-1").await,
        vec![(Cmd::Reserved { item: 7 }, intent_envelope(&key0))]
    );
    assert_eq!(
        rig.ledger_records("o-2").await,
        vec![(Cmd::Charged { item: 7 }, intent_envelope(&key1))]
    );
    assert_eq!(
        rig.outbox_records("saga-1").await,
        vec![(
            OutboxEvent::Intent {
                intent: RenderedIntentKey::from_rendered(key2.clone()),
                request: Fx::Export { item: 7 },
            },
            intent_envelope(&key2)
        )]
    );

    // One group per stream per batch, in reaction order.
    assert_eq!(
        &*fold.log.command_groups.lock().unwrap(),
        &[
            ("ledger/o-1".to_owned(), ExpectedVersion::NoStream, 1),
            ("ledger/o-2".to_owned(), ExpectedVersion::NoStream, 1),
        ]
    );
    assert_eq!(
        &*fold.log.intent_groups.lock().unwrap(),
        &[("saga-outbox/saga-1".to_owned(), 1)]
    );

    // The cursor stands at the acked entry: the next step is idle.
    assert_eq!(
        runner.step(limit(10)).await.expect("second step"),
        RunnerStep::Idle
    );
}

/// Two commands to one stream and two effects to the outbox stream,
/// all from one event: each stream gets ONE merged write, payloads
/// concatenated in reaction order.
#[tokio::test]
async fn commands_to_one_stream_merge_into_one_write() {
    let rig = Rig::new();
    let position = rig.place(Src::Placed { item: 8 }).await;

    let saga = Scripted::new(
        "saga-2",
        vec![(
            Src::Placed { item: 8 },
            vec![
                Reaction::Command(Command::new(
                    StreamRef::new(LEDGER, &"o-2".to_owned()),
                    ExpectedVersion::NoStream,
                    Cmd::Reserved { item: 8 },
                )),
                Reaction::Command(Command::new(
                    StreamRef::new(LEDGER, &"o-2".to_owned()),
                    ExpectedVersion::NoStream,
                    Cmd::Charged { item: 8 },
                )),
                Reaction::EffectRequest(EffectRequest::new(Fx::Export { item: 8 })),
                Reaction::EffectRequest(EffectRequest::new(Fx::Notify { item: 8 })),
            ],
        )],
    );
    let fold = MemFold::default();
    let runner = rig.runner(saga, fold.clone());

    let step = runner.step(limit(10)).await.expect("the step succeeds");
    assert_eq!(step, RunnerStep::Advanced { acked_to: position });

    let key0 = rendered("saga-2", SOURCE, "o-1", position.get(), 0);
    let key1 = rendered("saga-2", SOURCE, "o-1", position.get(), 1);
    let key2 = rendered("saga-2", SOURCE, "o-1", position.get(), 2);
    let key3 = rendered("saga-2", SOURCE, "o-1", position.get(), 3);
    assert_eq!(
        rig.ledger_records("o-2").await,
        vec![
            (Cmd::Reserved { item: 8 }, intent_envelope(&key0)),
            (Cmd::Charged { item: 8 }, intent_envelope(&key1)),
        ]
    );
    assert_eq!(
        rig.outbox_records("saga-2").await,
        vec![
            (
                OutboxEvent::Intent {
                    intent: RenderedIntentKey::from_rendered(key2.clone()),
                    request: Fx::Export { item: 8 },
                },
                intent_envelope(&key2)
            ),
            (
                OutboxEvent::Intent {
                    intent: RenderedIntentKey::from_rendered(key3.clone()),
                    request: Fx::Notify { item: 8 },
                },
                intent_envelope(&key3)
            ),
        ]
    );

    assert_eq!(
        &*fold.log.command_groups.lock().unwrap(),
        &[("ledger/o-2".to_owned(), ExpectedVersion::NoStream, 2)]
    );
    assert_eq!(
        &*fold.log.intent_groups.lock().unwrap(),
        &[("saga-outbox/saga-2".to_owned(), 2)]
    );
}

/// Commands one event addressed at one stream must agree on their
/// expectation: the runner rejects the group before any write, and the
/// entry is not acked.
#[tokio::test]
async fn a_split_expectation_is_rejected_before_the_fold() {
    let rig = Rig::new();
    let position = rig.place(Src::Placed { item: 9 }).await;

    let one = crate::streams::StreamSequence::new(1).expect("1 is a position");
    let saga = Scripted::new(
        "saga-3",
        vec![(
            Src::Placed { item: 9 },
            vec![
                Reaction::Command(Command::new(
                    StreamRef::new(LEDGER, &"o-3".to_owned()),
                    ExpectedVersion::NoStream,
                    Cmd::Reserved { item: 9 },
                )),
                Reaction::Command(Command::new(
                    StreamRef::new(LEDGER, &"o-3".to_owned()),
                    ExpectedVersion::Exact(one),
                    Cmd::Charged { item: 9 },
                )),
            ],
        )],
    );
    let fold = MemFold::default();
    let runner = rig.runner(saga, fold.clone());

    let error = runner
        .step(limit(10))
        .await
        .expect_err("the group conflicts");
    assert!(matches!(error, SagaError::ConflictingExpectations(_)));

    assert!(rig.ledger_records("o-3").await.is_empty());
    assert!(rig.outbox_records("saga-3").await.is_empty());
    assert!(
        fold.log.command_groups.lock().unwrap().is_empty(),
        "the fold never runs for a rejected group"
    );
    assert_eq!(
        rig.outstanding_for_group("saga-3").await,
        vec![position.get()],
        "the entry is not acked and reappears on the next poll"
    );
}

/// An event with no reactions acks without a batch.
#[tokio::test]
async fn an_empty_reaction_set_acks_without_a_batch() {
    let rig = Rig::new();
    let position = rig.place(Src::Cancelled { item: 10 }).await;

    let saga = Scripted::new("saga-4", vec![(Src::Placed { item: 10 }, Vec::new())]);
    let fold = MemFold::default();
    let runner = rig.runner(saga, fold.clone());

    let step = runner.step(limit(10)).await.expect("the step succeeds");
    assert_eq!(step, RunnerStep::Advanced { acked_to: position });
    assert!(rig.outbox_records("saga-4").await.is_empty());
    assert!(
        fold.log.command_groups.lock().unwrap().is_empty()
            && fold.log.intent_groups.lock().unwrap().is_empty()
    );
    assert_eq!(
        runner.step(limit(10)).await.expect("second step"),
        RunnerStep::Idle
    );
}

/// A poll that delivers nothing is idle, and the cursor does not move.
#[tokio::test]
async fn a_poll_with_no_entries_is_idle() {
    let rig = Rig::new();
    let saga = Scripted::new("saga-5", Vec::new());
    let runner = rig.runner(saga, MemFold::default());
    assert_eq!(
        runner.step(limit(10)).await.expect("the step succeeds"),
        RunnerStep::Idle
    );
}

/// The crash window between append and ack, replayed: the reactions
/// already committed (the batch landed, the ack did not), so the
/// replay's `NoStream` command arm conflicts and the classification
/// rule must find the minted intent key on the outbox stream and ack
/// the redelivery as a no-op - no second append anywhere.
#[tokio::test]
async fn a_replayed_no_stream_command_acks_as_a_noop_through_the_conflict_path() {
    let rig = Rig::new();
    let position = rig.place(Src::Placed { item: 11 }).await;

    // The crashed attempt's committed state, by hand: the command with
    // its envelope key and the intent with payload and envelope keys.
    let key0 = rendered("saga-6", SOURCE, "o-1", position.get(), 0);
    let key1 = rendered("saga-6", SOURCE, "o-1", position.get(), 1);
    let mut builder = rig.db.batch();
    builder
        .write(
            LEDGER,
            &"o-4".to_owned(),
            ExpectedVersion::NoStream,
            &EventBatch::from_records(vec![RecordedEvent::keyed(
                Cmd::Reserved { item: 11 },
                intent_envelope(&key0),
            )])
            .expect("one record is nonempty"),
        )
        .expect("one write per stream");
    builder
        .write(
            OUTBOX_CATEGORY,
            &"saga-6".to_owned(),
            ExpectedVersion::Any,
            &EventBatch::from_records(vec![RecordedEvent::keyed(
                OutboxEvent::Intent {
                    intent: RenderedIntentKey::from_rendered(key1.clone()),
                    request: Fx::Export { item: 11 },
                },
                intent_envelope(&key1),
            )])
            .expect("one record is nonempty"),
        )
        .expect("one write per stream");
    rig.db
        .transact(builder.build().expect("the batch has writes"))
        .await
        .expect("the crashed attempt's batch commits");

    let saga = Scripted::new(
        "saga-6",
        vec![(
            Src::Placed { item: 11 },
            vec![
                Reaction::Command(Command::new(
                    StreamRef::new(LEDGER, &"o-4".to_owned()),
                    ExpectedVersion::NoStream,
                    Cmd::Reserved { item: 11 },
                )),
                Reaction::EffectRequest(EffectRequest::new(Fx::Export { item: 11 })),
            ],
        )],
    );
    let runner = rig.runner(saga, MemFold::default());

    // The replay: react re-runs, the batch aborts on the NoStream
    // command arm, and the re-read finds the intent key present.
    let step = runner
        .step(limit(10))
        .await
        .expect("the redelivery acks as a no-op");
    assert_eq!(step, RunnerStep::Advanced { acked_to: position });

    // Nothing doubled: each stream still holds exactly its one record.
    assert_eq!(
        rig.ledger_records("o-4").await,
        vec![(Cmd::Reserved { item: 11 }, intent_envelope(&key0))]
    );
    assert_eq!(
        rig.outbox_records("saga-6").await,
        vec![(
            OutboxEvent::Intent {
                intent: RenderedIntentKey::from_rendered(key1.clone()),
                request: Fx::Export { item: 11 },
            },
            intent_envelope(&key1)
        )]
    );
    assert_eq!(
        runner.step(limit(10)).await.expect("second step"),
        RunnerStep::Idle
    );
}

/// A command arm that really conflicts - the stream moved past the
/// arm's expectation and the minted keys are nowhere on the outbox
/// stream - surfaces as an error, stores nothing, and does not ack.
#[tokio::test]
async fn a_real_conflict_surfaces_and_does_not_ack() {
    let rig = Rig::new();
    rig.seed_ledger("o-5", Cmd::Reserved { item: 12 }).await;
    let position = rig.place(Src::Placed { item: 12 }).await;

    let saga = Scripted::new(
        "saga-7",
        vec![(
            Src::Placed { item: 12 },
            vec![
                Reaction::Command(Command::new(
                    StreamRef::new(LEDGER, &"o-5".to_owned()),
                    ExpectedVersion::NoStream,
                    Cmd::Charged { item: 12 },
                )),
                Reaction::EffectRequest(EffectRequest::new(Fx::Export { item: 12 })),
            ],
        )],
    );
    let runner = rig.runner(saga, MemFold::default());

    let error = runner
        .step(limit(10))
        .await
        .expect_err("the conflict surfaces");
    assert!(matches!(error, SagaError::Conflict(_)));

    assert_eq!(
        rig.ledger_records("o-5").await,
        vec![(Cmd::Reserved { item: 12 }, EventMetadata::new())],
        "the pre-seeded record stands and nothing partial landed"
    );
    assert!(
        rig.outbox_records("saga-7").await.is_empty(),
        "the batch rolled back whole: no intent records"
    );
    assert_eq!(
        rig.outstanding_for_group("saga-7").await,
        vec![position.get()],
        "the failing entry is not acked"
    );
}

/// Each entry acks after its own batch commits: an error on a later
/// entry leaves the earlier entries acked, so only the failing entry
/// redelivers.
#[tokio::test]
async fn a_conflict_mid_step_leaves_earlier_entries_acked() {
    let rig = Rig::new();
    rig.seed_ledger("o-7", Cmd::Reserved { item: 13 }).await;
    let first = rig.place(Src::Placed { item: 13 }).await;
    let second = rig.place(Src::Placed { item: 14 }).await;

    let saga = Scripted::new(
        "saga-8",
        vec![
            (
                Src::Placed { item: 13 },
                vec![Reaction::Command(Command::new(
                    StreamRef::new(LEDGER, &"o-6".to_owned()),
                    ExpectedVersion::NoStream,
                    Cmd::Reserved { item: 13 },
                ))],
            ),
            (
                Src::Placed { item: 14 },
                vec![Reaction::Command(Command::new(
                    StreamRef::new(LEDGER, &"o-7".to_owned()),
                    ExpectedVersion::NoStream,
                    Cmd::Charged { item: 14 },
                ))],
            ),
        ],
    );
    let runner = rig.runner(saga, MemFold::default());

    let error = runner
        .step(limit(10))
        .await
        .expect_err("the second entry conflicts");
    assert!(matches!(error, SagaError::Conflict(_)));

    let key0 = rendered("saga-8", SOURCE, "o-1", first.get(), 0);
    assert_eq!(
        rig.ledger_records("o-6").await,
        vec![(Cmd::Reserved { item: 13 }, intent_envelope(&key0))],
        "the first entry's batch committed and stayed"
    );
    assert_eq!(
        rig.outstanding_for_group("saga-8").await,
        vec![second.get()],
        "only the failing entry redelivers"
    );
}

/// The rendered key's escaping is injective: the classic collision
/// (saga "a/b" on stream "c" vs saga "a" on stream "b/c") renders to
/// two different strings, each the canonical escaped form. The
/// rendering rule itself is pinned by every envelope assertion above;
/// this fixture pins it under adversarial components.
#[tokio::test]
async fn key_rendering_escapes_components_injectively() {
    // Saga "a/b" reacting on stream "c": key a\/b/orders/c/1/0.
    let rig_a = Rig::new();
    let position_a = rig_a.place_on("c", Src::Placed { item: 1 }).await;
    let saga_a = Scripted::new(
        "a/b",
        vec![(
            Src::Placed { item: 1 },
            vec![Reaction::Command(Command::new(
                StreamRef::new(LEDGER, &"c".to_owned()),
                ExpectedVersion::Any,
                Cmd::Reserved { item: 1 },
            ))],
        )],
    );
    rig_a
        .runner(saga_a, MemFold::default())
        .step(limit(10))
        .await
        .expect("the step succeeds");

    // Saga "a" reacting on stream "b/c": key a/orders/b\/c/1/0.
    let rig_b = Rig::new();
    let position_b = rig_b.place_on("b/c", Src::Placed { item: 1 }).await;
    let saga_b = Scripted::new(
        "a",
        vec![(
            Src::Placed { item: 1 },
            vec![Reaction::Command(Command::new(
                StreamRef::new(LEDGER, &"b/c".to_owned()),
                ExpectedVersion::Any,
                Cmd::Reserved { item: 1 },
            ))],
        )],
    );
    rig_b
        .runner(saga_b, MemFold::default())
        .step(limit(10))
        .await
        .expect("the step succeeds");

    let records_a = rig_a.ledger_records("c").await;
    let records_b = rig_b.ledger_records("b/c").await;
    let rendered_a = records_a[0]
        .1
        .get(INTENT_METADATA_KEY)
        .expect("the command record carries its key")
        .to_owned();
    let rendered_b = records_b[0]
        .1
        .get(INTENT_METADATA_KEY)
        .expect("the command record carries its key")
        .to_owned();

    assert_eq!(
        position_a, position_b,
        "the fixtures hold the same position"
    );
    assert_eq!(
        rendered_a,
        rendered("a/b", SOURCE, "c", 1, 0),
        "the slash in the saga id is escaped"
    );
    assert_eq!(
        rendered_b,
        rendered("a", SOURCE, "b/c", 1, 0),
        "the slash in the stream key is escaped"
    );
    assert_ne!(rendered_a, rendered_b, "escaping makes rendering injective");
}

/// The poll loop drains the backlog: every delivered event folds, its
/// intents land, and the loop keeps polling until stopped.
#[tokio::test]
async fn runner_run_drains_the_backlog() {
    let rig = Rig::new();
    let script: Vec<(Src, Vec<Reaction<Cmd, Fx>>)> = (20..23)
        .map(|item| {
            (
                Src::Placed { item },
                vec![
                    Reaction::Command(Command::new(
                        StreamRef::new(LEDGER, &"o-r".to_owned()),
                        ExpectedVersion::Any,
                        Cmd::Reserved { item },
                    )),
                    Reaction::EffectRequest(EffectRequest::new(Fx::Export { item })),
                ],
            )
        })
        .collect();
    let saga = Scripted::new("saga-run", script);
    for item in 20..23 {
        rig.place(Src::Placed { item }).await;
    }

    let runner = rig.runner(saga, MemFold::default());
    let task = tokio::spawn(async move {
        runner
            .run(limit(5), Duration::from_millis(10))
            .await
            .expect("the loop only stops by abort")
    });

    let outbox = rig.outbox.clone();
    let ledger = rig.ledger.clone();
    wait_until(|| {
        let outbox = outbox.clone();
        let ledger = ledger.clone();
        async move {
            stream_len(&outbox, "saga-run").await >= 3 && stream_len(&ledger, "o-r").await >= 3
        }
    })
    .await;

    let outbox_records = rig.outbox_records("saga-run").await;
    assert_eq!(outbox_records.len(), 3);
    assert!(outbox_records
        .iter()
        .all(|(event, _)| matches!(event, OutboxEvent::Intent { .. })));
    assert_eq!(rig.ledger_records("o-r").await.len(), 3);
    task.abort();
}
// ---------------------------------------------------------------------------
// Executor fixtures
// ---------------------------------------------------------------------------

/// The executor performs an intent through the port - keyed, not
/// payload-matched - appends `Done`, and acks past the intent.
#[tokio::test]
async fn executor_performs_intents_and_appends_done() {
    let rig = Rig::new();
    let key = rendered("saga-9", SOURCE, "o-1", 1, 0);
    rig.seed_intent("saga-9", &key, Fx::Export { item: 30 })
        .await;
    let intent_position = rig.outbox_tip().await;

    let port = Port::default();
    let executor = rig.executor("saga-9", port.clone(), Hook::default(), 5);
    let step = executor.step(limit(10)).await.expect("the step succeeds");
    assert_eq!(
        step,
        ExecutorStep::Advanced {
            acked_to: intent_position
        }
    );
    assert_eq!(
        &*port.calls.lock().unwrap(),
        &[(key.clone(), Fx::Export { item: 30 })],
        "the port sees the rendered key and the payload"
    );
    assert_eq!(
        rig.outbox_records("saga-9").await,
        vec![
            (
                OutboxEvent::Intent {
                    intent: RenderedIntentKey::from_rendered(key.clone()),
                    request: Fx::Export { item: 30 },
                },
                intent_envelope(&key)
            ),
            (
                OutboxEvent::Done {
                    intent: RenderedIntentKey::from_rendered(key.clone()),
                },
                EventMetadata::new()
            ),
        ],
        "the intent stands and the done record lands after it"
    );

    // The done record itself is an outcome entry on the next poll:
    // skipped and acked, never re-performed.
    let done_position = rig.outbox_tip().await;
    let step = executor
        .step(limit(10))
        .await
        .expect("the second step succeeds");
    assert_eq!(
        step,
        ExecutorStep::Advanced {
            acked_to: done_position
        }
    );
    assert_eq!(port.calls.lock().unwrap().len(), 1);
    assert_eq!(
        executor.step(limit(10)).await.expect("third step"),
        ExecutorStep::Idle
    );
}

/// Entries from another saga's stream in the shared category are
/// skipped and acked past, never performed.
#[tokio::test]
async fn executor_skips_foreign_sagas_streams() {
    let rig = Rig::new();
    let foreign_key = rendered("saga-a", SOURCE, "o-1", 1, 0);
    let own_key = rendered("saga-b", SOURCE, "o-1", 2, 0);
    rig.seed_intent("saga-a", &foreign_key, Fx::Export { item: 40 })
        .await;
    rig.seed_intent("saga-b", &own_key, Fx::Export { item: 41 })
        .await;
    let own_position = rig.outbox_tip().await;

    let port = Port::default();
    let executor = rig.executor("saga-b", port.clone(), Hook::default(), 5);
    let step = executor.step(limit(10)).await.expect("the step succeeds");
    assert_eq!(
        step,
        ExecutorStep::Advanced {
            acked_to: own_position
        },
        "both entries are consumed: one skipped, one performed"
    );
    assert_eq!(
        &*port.calls.lock().unwrap(),
        &[(own_key.clone(), Fx::Export { item: 41 })],
        "the foreign intent is never performed"
    );
    assert_eq!(
        rig.outbox_records("saga-a").await.len(),
        1,
        "the foreign stream is untouched"
    );
    assert_eq!(
        rig.outbox_records("saga-b").await,
        vec![
            (
                OutboxEvent::Intent {
                    intent: RenderedIntentKey::from_rendered(own_key.clone()),
                    request: Fx::Export { item: 41 },
                },
                intent_envelope(&own_key)
            ),
            (
                OutboxEvent::Done {
                    intent: RenderedIntentKey::from_rendered(own_key),
                },
                EventMetadata::new()
            ),
        ]
    );
}

/// A failing effect appends `Failed` with the port's rendered error
/// and HOLDS the cursor at the intent: the next poll redelivers the
/// same intent and the port is called again.
#[tokio::test]
async fn a_failing_effect_appends_failed_and_holds_the_cursor() {
    let rig = Rig::new();
    let key = rendered("saga-11", SOURCE, "o-1", 1, 0);
    rig.seed_intent("saga-11", &key, Fx::Export { item: 50 })
        .await;

    let port = Port::default();
    port.fail_for.lock().unwrap().insert(key.clone(), 5);
    let executor = rig.executor("saga-11", port.clone(), Hook::default(), 3);

    let step = executor.step(limit(10)).await.expect("the step succeeds");
    assert_eq!(
        step,
        ExecutorStep::Holding {
            intent: RenderedIntentKey::from_rendered(key.clone())
        }
    );
    assert_eq!(
        rig.outbox_records("saga-11").await,
        vec![
            (
                OutboxEvent::Intent {
                    intent: RenderedIntentKey::from_rendered(key.clone()),
                    request: Fx::Export { item: 50 },
                },
                intent_envelope(&key)
            ),
            (
                OutboxEvent::Failed {
                    intent: RenderedIntentKey::from_rendered(key.clone()),
                    error: PortErr::flaky().text(),
                },
                EventMetadata::new()
            ),
        ]
    );
    assert_eq!(port.calls.lock().unwrap().len(), 1);

    // The held cursor redelivers the intent; the second attempt fails
    // again inside the budget, so the executor holds again.
    let step = executor
        .step(limit(10))
        .await
        .expect("the second step succeeds");
    assert_eq!(
        step,
        ExecutorStep::Holding {
            intent: RenderedIntentKey::from_rendered(key.clone())
        }
    );
    assert_eq!(port.calls.lock().unwrap().len(), 2);
    assert_eq!(
        rig.outbox_records("saga-11").await.len(),
        3,
        "intent, first failed, second failed"
    );
}

/// Budget exhaustion: the last failed attempt appends `Parked` with
/// the durable attempt count, fires the hook exactly once with the
/// notice, and acks past the intent permanently. The parked record
/// and the failed records after it are outcome entries - skipped and
/// acked, and the hook does not fire for them.
#[tokio::test]
async fn budget_exhaustion_parks_and_fires_the_hook_once() {
    let rig = Rig::new();
    let key = rendered("saga-12", SOURCE, "o-1", 1, 0);
    rig.seed_intent("saga-12", &key, Fx::Export { item: 60 })
        .await;
    let intent_position = rig.outbox_tip().await;

    let port = Port::default();
    port.fail_for.lock().unwrap().insert(key.clone(), 5);
    let hook = Hook::default();
    let executor = rig.executor("saga-12", port.clone(), hook.clone(), 1);

    let step = executor.step(limit(10)).await.expect("the step parks");
    assert_eq!(
        step,
        ExecutorStep::Advanced {
            acked_to: intent_position
        },
        "the group advances past the parked intent permanently"
    );
    assert_eq!(
        rig.outbox_records("saga-12").await,
        vec![
            (
                OutboxEvent::Intent {
                    intent: RenderedIntentKey::from_rendered(key.clone()),
                    request: Fx::Export { item: 60 },
                },
                intent_envelope(&key)
            ),
            (
                OutboxEvent::Failed {
                    intent: RenderedIntentKey::from_rendered(key.clone()),
                    error: PortErr::flaky().text(),
                },
                EventMetadata::new()
            ),
            (
                OutboxEvent::Parked {
                    intent: RenderedIntentKey::from_rendered(key.clone()),
                    attempts: NonZeroU32::new(1).expect("1 is nonzero"),
                    error: PortErr::flaky().text(),
                },
                EventMetadata::new()
            ),
        ]
    );
    assert_eq!(
        &*hook.fires.lock().unwrap(),
        &[(key.clone(), 1, PortErr::flaky().text())],
        "the hook fires once, at park time, with the durable count"
    );
    assert_eq!(port.calls.lock().unwrap().len(), 1);

    // The parked record itself arrives on the next poll: an audit
    // fact, not a second trigger. Skipped, acked, no hook, no port.
    let parked_position = rig.outbox_tip().await;
    let step = executor
        .step(limit(10))
        .await
        .expect("the second step succeeds");
    assert_eq!(
        step,
        ExecutorStep::Advanced {
            acked_to: parked_position
        }
    );
    assert_eq!(hook.fires.lock().unwrap().len(), 1);
    assert_eq!(port.calls.lock().unwrap().len(), 1);
    assert_eq!(rig.outbox_records("saga-12").await.len(), 3);
}

/// The crash between parking and ack, replayed: the parked record
/// already stands and the failed count is at the budget, so the
/// replay re-fires the idempotent hook, appends nothing, and acks
/// past. The port is never asked to perform past its budget.
#[tokio::test]
async fn a_replayed_park_refires_the_hook_and_appends_nothing() {
    let rig = Rig::new();
    let key = rendered("saga-13", SOURCE, "o-1", 1, 0);
    let parked_error = "port failure: seed";
    rig.seed_intent("saga-13", &key, Fx::Export { item: 70 })
        .await;
    rig.seed_failed("saga-13", &key, parked_error).await;
    rig.seed_failed("saga-13", &key, parked_error).await;
    rig.seed_parked("saga-13", &key, 2, parked_error).await;
    let parked_position = rig.outbox_tip().await;

    let port = Port::default();
    let hook = Hook::default();
    let executor = rig.executor("saga-13", port.clone(), hook.clone(), 2);

    let step = executor
        .step(limit(10))
        .await
        .expect("the replay acks as a no-op");
    assert_eq!(
        step,
        ExecutorStep::Advanced {
            acked_to: parked_position
        },
        "the replay acks through the parked record"
    );
    assert_eq!(
        &*hook.fires.lock().unwrap(),
        &[(key.clone(), 2, parked_error.to_owned())],
        "the hook re-fires with the durable notice"
    );
    assert!(
        port.calls.lock().unwrap().is_empty(),
        "the budget is exhausted: no further perform"
    );
    assert_eq!(
        rig.outbox_records("saga-13").await.len(),
        4,
        "intent, two failed, parked - nothing appended on replay"
    );
    assert_eq!(
        executor.step(limit(10)).await.expect("second step"),
        ExecutorStep::Idle
    );
}

/// A hook that rejects the notice at park time: the parked record
/// stands, the cursor does not advance, and the next step re-fires
/// the hook - which is the idempotency obligation replaying - and
/// then acks without a second park.
#[tokio::test]
async fn a_rejecting_hook_leaves_the_park_standing_and_refires() {
    let rig = Rig::new();
    let key = rendered("saga-14", SOURCE, "o-1", 1, 0);
    rig.seed_intent("saga-14", &key, Fx::Export { item: 80 })
        .await;
    let intent_position = rig.outbox_tip().await;

    let port = Port::default();
    port.fail_for.lock().unwrap().insert(key.clone(), 5);
    let hook = Hook::default();
    *hook.reject_next.lock().unwrap() = true;
    let executor = rig.executor("saga-14", port.clone(), hook.clone(), 1);

    let error = executor
        .step(limit(10))
        .await
        .expect_err("the hook rejects the park");
    assert!(matches!(error, ExecutorError::Hook(_)));
    assert_eq!(
        rig.outbox_records("saga-14").await.len(),
        3,
        "intent, failed, parked - the park stands"
    );

    // The cursor never moved past the intent, so it redelivers.
    let entries = rig
        .outbox_feed
        .poll(
            &ConsumerGroup::new("saga-14").expect("a test group is nonempty"),
            limit(10),
        )
        .await
        .expect("probe poll succeeds");
    assert_eq!(
        entries.first().expect("the intent redelivers").position(),
        intent_position
    );

    let step = executor
        .step(limit(10))
        .await
        .expect("the re-fire succeeds");
    assert!(matches!(step, ExecutorStep::Advanced { .. }));
    assert_eq!(hook.fires.lock().unwrap().len(), 2);
    assert_eq!(
        rig.outbox_records("saga-14").await.len(),
        3,
        "no second park: the replay's append is the no-op"
    );
    assert_eq!(port.calls.lock().unwrap().len(), 1);
}

/// The crash between the last `Failed` and the `Parked` append,
/// replayed: the durable count is at the budget with no park on
/// record, so the replay parks NOW - `Parked` with the durable count
/// and the last recorded failure - fires the hook once, acks through,
/// and never performs past the budget.
#[tokio::test]
async fn a_crash_before_the_park_lands_parks_on_the_replay() {
    let rig = Rig::new();
    let key = rendered("saga-15", SOURCE, "o-1", 1, 0);
    let failed_error = "port failure: seed";
    rig.seed_intent("saga-15", &key, Fx::Export { item: 85 })
        .await;
    rig.seed_failed("saga-15", &key, failed_error).await;
    rig.seed_failed("saga-15", &key, failed_error).await;
    let failed_position = rig.outbox_tip().await;

    let port = Port::default();
    let hook = Hook::default();
    let executor = rig.executor("saga-15", port.clone(), hook.clone(), 2);

    let step = executor.step(limit(10)).await.expect("the replay parks");
    assert_eq!(
        step,
        ExecutorStep::Advanced {
            acked_to: failed_position
        },
        "the park lands; the page's own entries ack through, the parked record arrives next poll"
    );
    let parked_position = rig.outbox_tip().await;
    assert!(parked_position.get() > failed_position.get());

    assert_eq!(
        &*hook.fires.lock().unwrap(),
        &[(key.clone(), 2, failed_error.to_owned())],
        "the hook fires once at park time with the durable count and last failure"
    );
    assert!(
        port.calls.lock().unwrap().is_empty(),
        "the budget is exhausted: no further perform"
    );
    assert_eq!(
        rig.outbox_records("saga-15").await,
        vec![
            (
                OutboxEvent::Intent {
                    intent: RenderedIntentKey::from_rendered(key.clone()),
                    request: Fx::Export { item: 85 },
                },
                intent_envelope(&key)
            ),
            (
                OutboxEvent::Failed {
                    intent: RenderedIntentKey::from_rendered(key.clone()),
                    error: failed_error.to_owned(),
                },
                EventMetadata::new()
            ),
            (
                OutboxEvent::Failed {
                    intent: RenderedIntentKey::from_rendered(key.clone()),
                    error: failed_error.to_owned(),
                },
                EventMetadata::new()
            ),
            (
                OutboxEvent::Parked {
                    intent: RenderedIntentKey::from_rendered(key.clone()),
                    attempts: NonZeroU32::new(2).expect("2 is nonzero"),
                    error: failed_error.to_owned(),
                },
                EventMetadata::new()
            ),
        ]
    );
    assert_eq!(
        executor
            .step(limit(10))
            .await
            .expect("the parked record skips"),
        ExecutorStep::Advanced {
            acked_to: parked_position
        }
    );
    assert_eq!(
        hook.fires.lock().unwrap().len(),
        1,
        "an audit fact, not a trigger"
    );
    assert_eq!(
        executor.step(limit(10)).await.expect("third step"),
        ExecutorStep::Idle
    );
}

/// The executor's poll loop drains the backlog: every intent is
/// performed and recorded, in order, until the loop is stopped.
#[tokio::test]
async fn executor_run_drains_the_backlog() {
    let rig = Rig::new();
    let key_a = rendered("saga-19", SOURCE, "o-1", 1, 0);
    let key_b = rendered("saga-19", SOURCE, "o-1", 2, 0);
    rig.seed_intent("saga-19", &key_a, Fx::Export { item: 90 })
        .await;
    rig.seed_intent("saga-19", &key_b, Fx::Notify { item: 91 })
        .await;

    let port = Port::default();
    let executor = rig.executor("saga-19", port.clone(), Hook::default(), 5);
    let task = tokio::spawn(async move {
        executor
            .run(limit(5), Duration::from_millis(10))
            .await
            .expect("the loop only stops by abort")
    });

    let outbox = rig.outbox.clone();
    let calls = port.clone();
    wait_until(|| {
        let outbox = outbox.clone();
        let calls = calls.clone();
        async move {
            stream_len(&outbox, "saga-19").await >= 4 && calls.calls.lock().unwrap().len() >= 2
        }
    })
    .await;

    assert_eq!(
        &*port.calls.lock().unwrap(),
        &[
            (key_a.clone(), Fx::Export { item: 90 }),
            (key_b.clone(), Fx::Notify { item: 91 })
        ],
        "intents perform in stream order"
    );
    let records = rig.outbox_records("saga-19").await;
    assert_eq!(records.len(), 4);
    assert!(matches!(records[2].0, OutboxEvent::Done { .. }));
    assert!(matches!(records[3].0, OutboxEvent::Done { .. }));
    task.abort();
}

// ---------------------------------------------------------------------------
// Constructor pins (implemented surface; green on arrival by design)
// ---------------------------------------------------------------------------

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
    assert!(RetryPolicy::new(0, BackoffSchedule::default()).is_err());
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
