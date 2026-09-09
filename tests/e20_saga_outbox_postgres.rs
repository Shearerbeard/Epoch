//! E20 gate-M (the live-postgres crash table): the five crash windows
//! of the saga runner and the outbox executor, proven against the live
//! compose postgres - the store whose intent-key uniqueness index and
//! global feed positions the in-memory golden harness
//! (`src/streams/saga_outbox_golden.rs`) cannot exhibit. The consumer
//! harness here mirrors the golden's shapes - `Src`/`Cmd`/`Fx`, a
//! scripted saga, a `ReactionFold`, an `EffectPort`, a
//! `CompensationHook` - but self-contained and postgres-shaped, with
//! serde derives on the consumer types because the pg wire form is
//! JSONB.
//!
//! The suite migrates the schema once per scenario and writes only
//! ULID-nonce ids, so runs never collide and storage is never
//! cleared. Two namespaces see cross-run traffic by design: the
//! `ledger` category (per-run unique stream keys) and the
//! framework-owned `saga-outbox` category, whose feed carries every
//! prior run's rows. The executor scenarios therefore step in bounded
//! loops - each step skips and acks foreign entries - until THIS
//! scenario's expected state holds.
//!
//! ```sh
//! cargo test --features postgres --test e20_saga_outbox_postgres -- --nocapture
//! ```

#![cfg(feature = "postgres")]

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use epoch::decider::Event;
use epoch::streams::feed::{ConsumerGroup, EventFeed, FeedPosition, PollLimit};
use epoch::streams::outbox::{
    CompensationHook, EffectPort, ExecutorStep, OutboxEvent, OutboxExecutor, ParkedNotice,
    RetryBudget, INTENT_METADATA_KEY, OUTBOX_CATEGORY,
};
use epoch::streams::postgres::{
    pool_from_conn_str, PgBatchBuilder, PgDatabase, PgEventFeed, PgEventStreams,
};
use epoch::streams::saga::{
    BackoffSchedule, Command, CommandGroup, EffectRequest, IntentGroup, Reaction, ReactionFold,
    RenderedIntentKey, RetryPolicy, RunnerStep, Saga, SagaError, SagaId, SagaRunner,
};
use epoch::streams::spec::under_deadline;
use epoch::streams::{
    AtomicStreams, EventBatch, EventMetadata, EventStreams, ExpectedVersion, RecordedEvent,
    StreamRef, StreamState,
};

/// The ledger category commands land in (the golden's name; the
/// stream keys are per-run unique, so the shared category never
/// collides).
const LEDGER: &str = "ledger";

/// How many entries one executor poll may deliver: generous, because
/// the shared outbox category carries every prior run's rows.
const POLL_LIMIT: usize = 200;

/// The bound on step loops that chew through the shared outbox
/// backlog before THIS scenario's entry is reached.
const STEP_BOUND: usize = 200;

/// One runner/executor step's own deadline: a hang fails loudly, not
/// slowly.
const STEP_DEADLINE: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// The consumer's types
// ---------------------------------------------------------------------------

/// A source event the saga reacts to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
enum Src {
    Placed {
        item: u32,
    },
    #[allow(dead_code)] // the golden harness's shape, kept whole
    Cancelled {
        item: u32,
    },
}

impl Event for Src {
    type EntityId = ();

    fn event_type(&self) -> String {
        "Src".to_owned()
    }

    fn get_id(&self) -> Self::EntityId {}
}

/// A command payload destined for a `ledger` stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum Fx {
    Export {
        item: u32,
    },
    #[allow(dead_code)] // the golden harness's shape, kept whole
    Notify {
        item: u32,
    },
}

impl Event for Fx {
    type EntityId = ();

    fn event_type(&self) -> String {
        "Fx".to_owned()
    }

    fn get_id(&self) -> Self::EntityId {}
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

/// The fold cannot fail in these scenarios; the error type exists
/// because the seam requires one.
#[derive(Debug)]
struct FoldErr;

impl fmt::Display for FoldErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the fold rejected a group")
    }
}

impl std::error::Error for FoldErr {}

/// The consumer's fold over the postgres batch builder: erases typed
/// payloads at push, copies the payload key onto `Intent` records,
/// and attaches each record's envelope unchanged, per the
/// `ReactionFold` contract. Stateless: nothing to share across
/// clones.
struct PgFold;

impl ReactionFold<PgBatchBuilder> for PgFold {
    type Command = Cmd;
    type Effect = Fx;
    type Error = FoldErr;

    fn push_command_group(
        &self,
        builder: &mut PgBatchBuilder,
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
            .map_err(|_| FoldErr)
    }

    fn push_intent_group(
        &self,
        builder: &mut PgBatchBuilder,
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
            .map_err(|_| FoldErr)
    }
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

/// The simulated crash's panic payload: distinguishes the port's
/// staged crash from any other panic the spawned step could raise.
#[derive(Debug)]
struct SimulatedCrash;

/// A port that records every perform, applies the "external effect"
/// only the FIRST time a key is seen (the dedupe set that absorbs
/// at-least-once redelivery at the port, not the framework), can be
/// scripted to fail a fixed number of attempts per key, and can be
/// poisoned to crash - `panic_any` - on the perform that applies a
/// given key, which is the crash between the external call and the
/// outcome append. Clones share the record.
#[derive(Clone, Default)]
struct Port {
    calls: Arc<Mutex<Vec<(String, Fx)>>>,
    applied: Arc<Mutex<Vec<String>>>,
    fail_for: Arc<Mutex<HashMap<String, u32>>>,
    crash_on_apply: Arc<Mutex<Vec<String>>>,
}

impl Port {
    /// A port that shares this port's external system - the call log
    /// and the applied set - but carries none of its scripting: the
    /// fresh-process-after-a-crash shape.
    fn healthy_sibling(&self) -> Port {
        Port {
            calls: Arc::clone(&self.calls),
            applied: Arc::clone(&self.applied),
            fail_for: Arc::default(),
            crash_on_apply: Arc::default(),
        }
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
        let applied_now = {
            let mut applied = self.applied.lock().unwrap();
            if applied.contains(&key) {
                false
            } else {
                applied.push(key.clone());
                true
            }
        };
        let failed = {
            let mut fail_for = self.fail_for.lock().unwrap();
            match fail_for.get_mut(&key) {
                Some(remaining) if *remaining > 0 => {
                    *remaining -= 1;
                    true
                }
                _ => false,
            }
        };
        if failed {
            return Err(PortErr::flaky());
        }
        if applied_now && self.crash_on_apply.lock().unwrap().contains(&key) {
            std::panic::panic_any(SimulatedCrash);
        }
        Ok(())
    }
}

/// A hook that records every fire. Clones share the record.
#[derive(Clone, Default)]
struct Hook {
    fires: Arc<Mutex<Vec<(String, u32, String)>>>,
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
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The rig
// ---------------------------------------------------------------------------

/// One pool over the live compose postgres, migrated once, with the
/// store and feed views the runner, the executor, and the assertions
/// need. The source category is per-run unique, so the source feed
/// sees only this run's rows; the outbox category is the shared
/// framework-owned one, so executor polls see foreign backlogs.
struct Rig {
    source_category: String,
    db: PgDatabase,
    source: PgEventStreams<String, Src>,
    source_feed: PgEventFeed<Src>,
    outbox: PgEventStreams<String, OutboxEvent<Fx>>,
    outbox_feed: PgEventFeed<OutboxEvent<Fx>>,
    ledger: PgEventStreams<String, Cmd>,
}

impl Rig {
    async fn new() -> Self {
        let pool = pool_from_conn_str(&conn_str())
            .await
            .expect("pg pool from EPOCH_PG_TEST_URL");
        let source_category = unique("e20-src");
        let source: PgEventStreams<String, Src> =
            PgEventStreams::new(pool.clone(), &source_category);
        source.migrate().await.expect("schema migrates");
        Self {
            source_category: source_category.clone(),
            db: PgDatabase::new(pool.clone()),
            source,
            source_feed: PgEventFeed::new(pool.clone(), &source_category),
            outbox: PgEventStreams::new(pool.clone(), OUTBOX_CATEGORY),
            outbox_feed: PgEventFeed::new(pool.clone(), OUTBOX_CATEGORY),
            ledger: PgEventStreams::new(pool, LEDGER),
        }
    }

    #[allow(clippy::type_complexity)]
    fn runner(
        &self,
        saga: Scripted,
    ) -> SagaRunner<
        PgDatabase,
        PgEventFeed<Src>,
        PgEventStreams<String, OutboxEvent<Fx>>,
        Scripted,
        PgFold,
    > {
        SagaRunner::new(
            saga,
            self.db.clone(),
            self.source_feed.clone(),
            self.outbox.clone(),
            PgFold,
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
        PgEventFeed<OutboxEvent<Fx>>,
        PgEventStreams<String, OutboxEvent<Fx>>,
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

    /// Append one bare source event to `stream`; returns its committed
    /// position in the shared log.
    async fn place_on(&self, stream: &str, event: Src) -> FeedPosition {
        self.source
            .append(
                ExpectedVersion::Any,
                &stream.to_owned(),
                &EventBatch::new(vec![event]).expect("one event is nonempty"),
            )
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

    /// The saga's outbox stream, whole: payload and envelope per
    /// record, oldest first.
    async fn outbox_records(&self, saga: &str) -> Vec<(OutboxEvent<Fx>, EventMetadata)> {
        stream_records(&self.outbox, saga).await
    }

    /// One ledger stream, whole: payload and envelope per record.
    async fn ledger_records(&self, stream: &str) -> Vec<(Cmd, EventMetadata)> {
        stream_records(&self.ledger, stream).await
    }

    /// A bare event on a ledger stream, for pre-seeding conflicts.
    async fn seed_ledger(&self, stream: &str, event: Cmd) {
        self.ledger
            .append(
                ExpectedVersion::Any,
                &stream.to_owned(),
                &EventBatch::new(vec![event]).expect("one event is nonempty"),
            )
            .await
            .expect("ledger seed append succeeds");
    }
}

/// A scratch group the rig uses only to observe committed positions.
fn probe() -> ConsumerGroup {
    ConsumerGroup::new("probe").expect("a test group is nonempty")
}

async fn stream_records<E>(store: &PgEventStreams<String, E>, id: &str) -> Vec<(E, EventMetadata)>
where
    E: Event + Serialize + DeserializeOwned + Send + Sync + fmt::Debug,
{
    match store
        .load_stream(&id.to_owned())
        .await
        .expect("load succeeds")
    {
        StreamState::Present(batch) => batch
            .into_records()
            .into_iter()
            .map(|record| record.into_parts())
            .collect(),
        StreamState::Missing => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Assertion matchers and expected-value builders
// ---------------------------------------------------------------------------

/// The expected envelope of one reaction record: exactly the intent
/// key under the framework-owned metadata name, and nothing else.
fn intent_envelope(key: &str) -> EventMetadata {
    let mut metadata = EventMetadata::new();
    metadata.insert(INTENT_METADATA_KEY, key);
    metadata
}

/// One ledger record: the payload with its intent-key envelope.
fn is_command_record(record: &(Cmd, EventMetadata), payload: &Cmd, key: &str) -> bool {
    record.0 == *payload && record.1.get(INTENT_METADATA_KEY) == Some(key)
}

/// One intent record: the key in payload AND envelope, the request
/// the saga recorded.
fn is_intent_record(record: &(OutboxEvent<Fx>, EventMetadata), key: &str, payload: &Fx) -> bool {
    match record {
        (OutboxEvent::Intent { intent, request }, metadata) => {
            intent.as_str() == key && request == payload && metadata == &intent_envelope(key)
        }
        _ => false,
    }
}

/// One Done record: the key in the payload, no envelope.
fn is_done_record(record: &(OutboxEvent<Fx>, EventMetadata), key: &str) -> bool {
    match record {
        (OutboxEvent::Done { intent }, metadata) => intent.as_str() == key && metadata.is_empty(),
        _ => false,
    }
}

/// One Failed record: the key and the rendered port error, no
/// envelope.
fn is_failed_record(record: &(OutboxEvent<Fx>, EventMetadata), key: &str, error: &str) -> bool {
    match record {
        (
            OutboxEvent::Failed {
                intent,
                error: text,
            },
            metadata,
        ) => intent.as_str() == key && text.as_str() == error && metadata.is_empty(),
        _ => false,
    }
}

/// One Parked record: the key, the durable attempt count, the last
/// rendered failure, no envelope.
fn is_parked_record(
    record: &(OutboxEvent<Fx>, EventMetadata),
    key: &str,
    attempts: u32,
    error: &str,
) -> bool {
    match record {
        (
            OutboxEvent::Parked {
                intent,
                attempts: parked,
                error: text,
            },
            metadata,
        ) => {
            intent.as_str() == key
                && parked.get() == attempts
                && text.as_str() == error
                && metadata.is_empty()
        }
        _ => false,
    }
}

/// The spec's rendering rule, restated from the goldens: inside every
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

/// `RenderedIntentKey::from_rendered` is crate-internal, so an
/// integration test reaches the same constructor through the type's
/// public serde form: a rendered key is a transparent newtype over
/// its text.
fn key_of(text: String) -> RenderedIntentKey {
    serde_json::from_value(serde_json::Value::String(text))
        .expect("a rendered key is a JSON string")
}

fn conn_str() -> String {
    let _ = dotenv::dotenv();
    std::env::var("EPOCH_PG_TEST_URL").expect("EPOCH_PG_TEST_URL must be set (see .env.example)")
}

/// Stream ids carry a ULID nonce, so runs never collide and storage
/// is never cleared.
fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", rusty_ulid::generate_ulid_string())
}

fn limit(n: usize) -> PollLimit {
    PollLimit::new(n).expect("a test limit is nonzero")
}

// ---------------------------------------------------------------------------
// The five crash windows
// ---------------------------------------------------------------------------

/// THE typed-outcome proof. The source event's command arm expects
/// `Any` - it can never conflict, so only the uniqueness index can
/// abort the replayed batch. The crashed attempt's batch (command +
/// intent, no ack) is committed by hand; a fresh runner replays, the
/// intent insert hits `stream_events_outbox_intent`, the typed
/// `TransactError::DuplicateIntent` surfaces, the classification
/// re-read finds the minted key present, and the entry acks as a
/// no-op. Without the unique-violation mapping this test fails with a
/// propagated `Backend` error after retries - that is the point.
#[tokio::test(flavor = "multi_thread")]
async fn crash_between_append_and_ack_rejects_the_duplicate_intent() {
    under_deadline(async {
        let rig = Rig::new().await;
        let saga_id = unique("e20-dup-saga");
        let ledger_stream = unique("e20-ledger");
        let position = rig.place_on("o-1", Src::Placed { item: 7 }).await;

        // The crashed attempt's minted keys, recomputed by hand from
        // the rendering rule.
        let key0 = rendered(&saga_id, &rig.source_category, "o-1", position.get(), 0);
        let key1 = rendered(&saga_id, &rig.source_category, "o-1", position.get(), 1);

        // The batch that committed before the crash: the command and
        // the intent, each with its envelope key. The ack never
        // landed.
        let mut builder = rig.db.batch();
        builder
            .write(
                LEDGER,
                &ledger_stream,
                ExpectedVersion::Any,
                &EventBatch::from_records(vec![RecordedEvent::keyed(
                    Cmd::Reserved { item: 7 },
                    intent_envelope(&key0),
                )])
                .expect("one record is nonempty"),
            )
            .expect("the command write encodes");
        builder
            .write(
                OUTBOX_CATEGORY,
                &saga_id,
                ExpectedVersion::Any,
                &EventBatch::from_records(vec![RecordedEvent::keyed(
                    OutboxEvent::Intent {
                        intent: key_of(key1.clone()),
                        request: Fx::Export { item: 7 },
                    },
                    intent_envelope(&key1),
                )])
                .expect("one record is nonempty"),
            )
            .expect("the intent write encodes");
        rig.db
            .transact(builder.build().expect("the batch has writes"))
            .await
            .expect("the crashed attempt's batch commits");

        // A fresh runner: its group cursor has never acked - the
        // crash state.
        let saga = Scripted::new(
            &saga_id,
            vec![(
                Src::Placed { item: 7 },
                vec![
                    Reaction::Command(Command::new(
                        StreamRef::new(LEDGER, &ledger_stream),
                        ExpectedVersion::Any,
                        Cmd::Reserved { item: 7 },
                    )),
                    Reaction::EffectRequest(EffectRequest::new(Fx::Export { item: 7 })),
                ],
            )],
        );
        let runner = rig.runner(saga);

        // The replay: react re-runs, the batch re-appends, the intent
        // insert hits the uniqueness index, and the classification
        // re-read finds the key present.
        let step = runner
            .step(limit(10))
            .await
            .expect("the redelivery acks as a no-op");
        assert_eq!(step, RunnerStep::Advanced { acked_to: position });

        // Nothing doubled: each stream holds exactly its one record.
        let ledger = rig.ledger_records(&ledger_stream).await;
        assert_eq!(
            ledger.len(),
            1,
            "the rolled-back re-append left no second row"
        );
        assert!(is_command_record(
            &ledger[0],
            &Cmd::Reserved { item: 7 },
            &key0
        ));
        let outbox = rig.outbox_records(&saga_id).await;
        assert_eq!(outbox.len(), 1, "exactly the pre-committed intent stands");
        assert!(is_intent_record(&outbox[0], &key1, &Fx::Export { item: 7 }));

        assert_eq!(
            runner.step(limit(10)).await.expect("second step"),
            RunnerStep::Idle
        );
    })
    .await;
}

/// The panel's expectation-sensitive scenario: the same crash replay,
/// but the command arm carries `ExpectedVersion::NoStream`. The
/// manual pre-commit creates the command stream, so the replay's
/// batch aborts as a Conflict - the expectation check runs before the
/// inserts ever reach the index - and the classification finds the
/// intent key present and acks the no-op.
#[tokio::test(flavor = "multi_thread")]
async fn replayed_no_stream_command_acks_through_the_conflict_path() {
    under_deadline(async {
        let rig = Rig::new().await;
        let saga_id = unique("e20-nostr-saga");
        let ledger_stream = unique("e20-ledger");
        let position = rig.place_on("o-1", Src::Placed { item: 11 }).await;

        let key0 = rendered(&saga_id, &rig.source_category, "o-1", position.get(), 0);
        let key1 = rendered(&saga_id, &rig.source_category, "o-1", position.get(), 1);

        // The crashed attempt's committed state, by hand: the NoStream
        // command creates its stream, the intent lands, the ack does
        // not.
        let mut builder = rig.db.batch();
        builder
            .write(
                LEDGER,
                &ledger_stream,
                ExpectedVersion::NoStream,
                &EventBatch::from_records(vec![RecordedEvent::keyed(
                    Cmd::Reserved { item: 11 },
                    intent_envelope(&key0),
                )])
                .expect("one record is nonempty"),
            )
            .expect("the command write encodes");
        builder
            .write(
                OUTBOX_CATEGORY,
                &saga_id,
                ExpectedVersion::Any,
                &EventBatch::from_records(vec![RecordedEvent::keyed(
                    OutboxEvent::Intent {
                        intent: key_of(key1.clone()),
                        request: Fx::Export { item: 11 },
                    },
                    intent_envelope(&key1),
                )])
                .expect("one record is nonempty"),
            )
            .expect("the intent write encodes");
        rig.db
            .transact(builder.build().expect("the batch has writes"))
            .await
            .expect("the crashed attempt's batch commits");

        let saga = Scripted::new(
            &saga_id,
            vec![(
                Src::Placed { item: 11 },
                vec![
                    Reaction::Command(Command::new(
                        StreamRef::new(LEDGER, &ledger_stream),
                        ExpectedVersion::NoStream,
                        Cmd::Reserved { item: 11 },
                    )),
                    Reaction::EffectRequest(EffectRequest::new(Fx::Export { item: 11 })),
                ],
            )],
        );
        let runner = rig.runner(saga);

        // The replay: the expectation check aborts the batch before
        // any insert, and the re-read finds the intent key present.
        let step = runner
            .step(limit(10))
            .await
            .expect("the redelivery acks as a no-op");
        assert_eq!(step, RunnerStep::Advanced { acked_to: position });

        // Nothing doubled: each stream still holds exactly its one
        // record.
        let ledger = rig.ledger_records(&ledger_stream).await;
        assert_eq!(ledger.len(), 1, "nothing doubled");
        assert!(is_command_record(
            &ledger[0],
            &Cmd::Reserved { item: 11 },
            &key0
        ));
        let outbox = rig.outbox_records(&saga_id).await;
        assert_eq!(outbox.len(), 1, "nothing doubled");
        assert!(is_intent_record(
            &outbox[0],
            &key1,
            &Fx::Export { item: 11 }
        ));

        assert_eq!(
            runner.step(limit(10)).await.expect("second step"),
            RunnerStep::Idle
        );
    })
    .await;
}

/// A command arm that really conflicts - the ledger stream moved past
/// the arm's `NoStream` expectation before the source event landed,
/// and the minted keys are nowhere on the outbox stream (no manual
/// pre-commit, no intents anywhere). The conflict surfaces as an
/// error, stores nothing, and the failing entry is not acked.
#[tokio::test(flavor = "multi_thread")]
async fn a_real_conflict_on_postgres_surfaces() {
    under_deadline(async {
        let rig = Rig::new().await;
        let saga_id = unique("e20-conflict-saga");
        let ledger_stream = unique("e20-ledger");

        // The ledger stream already moved: the saga's NoStream arm can
        // never land.
        rig.seed_ledger(&ledger_stream, Cmd::Reserved { item: 12 })
            .await;
        let position = rig.place_on("o-1", Src::Placed { item: 12 }).await;

        let saga = Scripted::new(
            &saga_id,
            vec![(
                Src::Placed { item: 12 },
                vec![
                    Reaction::Command(Command::new(
                        StreamRef::new(LEDGER, &ledger_stream),
                        ExpectedVersion::NoStream,
                        Cmd::Charged { item: 12 },
                    )),
                    Reaction::EffectRequest(EffectRequest::new(Fx::Export { item: 12 })),
                ],
            )],
        );
        let error = rig
            .runner(saga)
            .step(limit(10))
            .await
            .expect_err("the conflict surfaces");
        assert!(matches!(error, SagaError::Conflict(_)));

        assert!(
            rig.outbox_records(&saga_id).await.is_empty(),
            "the batch rolled back whole: no intent records"
        );

        // The failing entry is not acked: a fresh poll of the runner's
        // group still delivers its position.
        let redelivered = rig
            .source_feed
            .poll(
                &ConsumerGroup::new(saga_id.clone()).expect("a saga id is a nonempty group"),
                limit(10),
            )
            .await
            .expect("probe poll succeeds");
        assert_eq!(
            redelivered
                .iter()
                .map(|entry| entry.position())
                .collect::<Vec<_>>(),
            vec![position],
            "the failing entry is not acked"
        );
    })
    .await;
}

/// The executor's crash window: the port performs the external effect
/// and the process dies before the `Done` append. The poisoned port
/// stages the crash - apply, then `panic_any` - inside a spawned
/// step, so the executor "crashes" with the effect already applied.
/// A fresh executor on a healthy port (sharing the external system)
/// redelivers the intent: the port is called a SECOND time, the
/// port-side dedupe absorbs the replay, `Done` lands, and the cursor
/// advances. At-least-once delivery, idempotency at the port.
#[tokio::test(flavor = "multi_thread")]
async fn executor_crash_between_perform_and_done_replays_to_a_dedupe() {
    under_deadline(async {
        let rig = Rig::new().await;
        let saga_id = unique("e20-exec-saga");
        let position = rig.place_on("o-1", Src::Placed { item: 30 }).await;

        // One intent, committed by a real runner step.
        let saga = Scripted::new(
            &saga_id,
            vec![(
                Src::Placed { item: 30 },
                vec![Reaction::EffectRequest(EffectRequest::new(Fx::Export {
                    item: 30,
                }))],
            )],
        );
        let step = rig
            .runner(saga)
            .step(limit(10))
            .await
            .expect("the intent commits");
        assert_eq!(step, RunnerStep::Advanced { acked_to: position });

        let key = rendered(&saga_id, &rig.source_category, "o-1", position.get(), 0);

        // The poisoned port: its FIRST perform for the key applies the
        // external effect and then panics - the crash between the
        // external call and the outcome append. The step runs inside
        // tokio::spawn, so the crash is a JoinError, the
        // process-death shape.
        let port = Port::default();
        port.crash_on_apply.lock().unwrap().push(key.clone());
        let executor = rig.executor(&saga_id, port.clone(), Hook::default(), 5);

        let crashed = tokio::spawn(async move {
            for _ in 0..STEP_BOUND {
                executor
                    .step(limit(POLL_LIMIT))
                    .await
                    .expect("the executor steps succeed until the simulated crash");
            }
            panic!("the poisoned port never reached the intent within the step bound");
        });
        let outcome = tokio::time::timeout(STEP_DEADLINE, crashed)
            .await
            .expect("the crash lands inside the deadline");
        match outcome {
            Err(join_error) => {
                assert!(
                    join_error.is_panic(),
                    "the executor task failed without panicking: {join_error}"
                );
                assert!(
                    join_error
                        .into_panic()
                        .downcast_ref::<SimulatedCrash>()
                        .is_some(),
                    "the crash was not the port's simulated perform crash"
                );
            }
            Ok(_) => panic!("the executor survived the poisoned port"),
        }
        assert_eq!(
            &*port.calls.lock().unwrap(),
            &[(key.clone(), Fx::Export { item: 30 })],
            "the crash recorded the first perform"
        );
        assert_eq!(
            &*port.applied.lock().unwrap(),
            &[key.clone()],
            "the external effect was applied before the crash"
        );

        // A fresh executor on a healthy port over the same external
        // system: the intent redelivers, the port is called a second
        // time, the dedupe absorbs it, Done appends, the cursor
        // advances.
        let fresh = rig.executor(&saga_id, port.healthy_sibling(), Hook::default(), 5);
        let mut steps = 0;
        let records = loop {
            let outcome = tokio::time::timeout(STEP_DEADLINE, fresh.step(limit(POLL_LIMIT)))
                .await
                .expect("the fresh step lands inside the deadline")
                .expect("the fresh executor steps succeed");
            assert!(
                matches!(outcome, ExecutorStep::Advanced { .. } | ExecutorStep::Idle),
                "unexpected executor outcome: {outcome:?}"
            );
            steps += 1;
            assert!(
                steps <= STEP_BOUND,
                "the fresh executor never reached the intent"
            );
            let records = rig.outbox_records(&saga_id).await;
            if records.len() == 2 && is_intent_record(&records[0], &key, &Fx::Export { item: 30 }) {
                break records;
            }
        };
        assert!(
            is_done_record(&records[1], &key),
            "exactly one Done record lands after the intent: {records:?}"
        );
        assert_eq!(
            &*port.applied.lock().unwrap(),
            &[key.clone()],
            "the external effect was applied exactly once"
        );
        assert_eq!(
            port.calls.lock().unwrap().len(),
            2,
            "at-least-once: the port was called twice, the dedupe absorbed the replay"
        );
    })
    .await;
}

/// Budget exhaustion on postgres: with a budget of 2 and a port that
/// always fails, the first perform appends `Failed` and HOLDS the
/// cursor, the second parks the intent - `Parked` with the durable
/// count and the port error's Display text - and fires the hook
/// exactly once. After the park, one more step skips the outcome
/// records and the final state is stable.
#[tokio::test(flavor = "multi_thread")]
async fn repeated_failure_parks_and_fires_the_hook_on_postgres() {
    under_deadline(async {
        let rig = Rig::new().await;
        let saga_id = unique("e20-park-saga");
        let position = rig.place_on("o-1", Src::Placed { item: 60 }).await;

        // One intent, committed by a real runner step.
        let saga = Scripted::new(
            &saga_id,
            vec![(
                Src::Placed { item: 60 },
                vec![Reaction::EffectRequest(EffectRequest::new(Fx::Export {
                    item: 60,
                }))],
            )],
        );
        let step = rig
            .runner(saga)
            .step(limit(10))
            .await
            .expect("the intent commits");
        assert_eq!(step, RunnerStep::Advanced { acked_to: position });

        let key = rendered(&saga_id, &rig.source_category, "o-1", position.get(), 0);
        let error_text = PortErr::flaky().text();

        let port = Port::default();
        port.fail_for.lock().unwrap().insert(key.clone(), u32::MAX);
        let hook = Hook::default();
        let executor = rig.executor(&saga_id, port.clone(), hook.clone(), 2);

        // Each step appends Failed or parks; bounded loop until the
        // park lands (the first steps may chew foreign backlog).
        let mut steps = 0;
        let records = loop {
            let outcome = tokio::time::timeout(STEP_DEADLINE, executor.step(limit(POLL_LIMIT)))
                .await
                .expect("the step lands inside the deadline")
                .expect("the executor steps succeed until the park");
            steps += 1;
            assert!(
                steps <= STEP_BOUND,
                "the park never landed within the step bound"
            );
            let records = rig.outbox_records(&saga_id).await;
            if records
                .iter()
                .any(|record| is_parked_record(record, &key, 2, &error_text))
            {
                assert!(
                    matches!(outcome, ExecutorStep::Advanced { .. }),
                    "the park acks past the intent permanently, got {outcome:?}"
                );
                break records;
            }
            match &outcome {
                ExecutorStep::Advanced { .. } => {}
                ExecutorStep::Holding { intent } => {
                    assert_eq!(intent.as_str(), key, "the hold names the failing intent");
                }
                ExecutorStep::Idle => panic!("the executor went idle before the park"),
            }
        };

        // The whole stream, in order: Intent, Failed, Failed, Parked.
        assert_eq!(records.len(), 4, "intent, two failed, parked: {records:?}");
        assert!(is_intent_record(
            &records[0],
            &key,
            &Fx::Export { item: 60 }
        ));
        assert!(is_failed_record(&records[1], &key, &error_text));
        assert!(is_failed_record(&records[2], &key, &error_text));
        assert!(is_parked_record(&records[3], &key, 2, &error_text));
        assert_eq!(
            &*hook.fires.lock().unwrap(),
            &[(key.clone(), 2, error_text.clone())],
            "the hook fires once, at park time, with the durable count"
        );
        assert_eq!(
            port.calls.lock().unwrap().len(),
            2,
            "the budget allowed exactly two performs"
        );

        // After the park: one more step skips the outcome records
        // (ack past) and the final state is stable.
        let outcome = tokio::time::timeout(STEP_DEADLINE, executor.step(limit(POLL_LIMIT)))
            .await
            .expect("the post-park step lands inside the deadline")
            .expect("the post-park step succeeds");
        assert!(
            matches!(outcome, ExecutorStep::Advanced { .. } | ExecutorStep::Idle),
            "unexpected post-park outcome: {outcome:?}"
        );
        assert_eq!(
            rig.outbox_records(&saga_id).await.len(),
            4,
            "no more appends after the park"
        );
        assert_eq!(hook.fires.lock().unwrap().len(), 1);
        assert_eq!(port.calls.lock().unwrap().len(), 2);
    })
    .await;
}
