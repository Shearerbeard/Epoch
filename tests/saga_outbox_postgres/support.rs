//! Consumer types and fixtures shared by the PostgreSQL replay tests.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use epoch::decider::Event;
use epoch::streams::feed::{ConsumerGroup, EventFeed, FeedPosition, PollLimit};
use epoch::streams::outbox::{
    CompensationHook, EffectPort, Executor, ParkedNotice, Record, INTENT_METADATA_KEY,
    OUTBOX_CATEGORY,
};
use epoch::streams::postgres::{
    pool_from_conn_str, PgBatchBuilder, PgDatabase, PgEventFeed, PgEventStreams,
};
use epoch::streams::saga::{CommandGroup, IntentGroup, Reaction, ReactionFold, Runner, Saga};
use epoch::streams::{
    BackoffSchedule, EventBatch, EventMetadata, EventStreams, ExpectedVersion, RecordedEvent,
    RenderedIntentKey, RetryBudget, RetryPolicy, SagaId, StreamState,
};

/// The ledger category commands land in (the golden's name; the
/// stream keys are per-run unique, so the shared category never
/// collides).
pub(super) const LEDGER: &str = "ledger";

/// How many entries one executor poll may deliver: generous, because
/// the shared outbox category carries every prior run's rows.
pub(super) const POLL_LIMIT: usize = 200;

/// The bound on step loops that chew through the shared outbox
/// backlog before THIS scenario's entry is reached.
pub(super) const STEP_BOUND: usize = 200;

/// One runner/executor step's own deadline: a hang fails loudly, not
/// slowly.
pub(super) const STEP_DEADLINE: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// The consumer's types
// ---------------------------------------------------------------------------

/// A source event the saga reacts to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(super) enum Src {
    Placed {
        item: u32,
    },
    #[allow(dead_code)] // also exercises the variant's wire shape
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Fx {
    Export {
        item: u32,
    },
    #[allow(dead_code)] // wire-format parity covers this variant
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

/// Replays the same reactions for the same source event.
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

/// The fold cannot fail in these scenarios; the error type exists
/// because the seam requires one.
#[derive(Debug)]
pub(super) struct FoldErr;

impl fmt::Display for FoldErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the fold rejected a group")
    }
}

impl std::error::Error for FoldErr {}

/// Exercises the consumer's obligation to preserve each envelope and
/// copy each intent key into its payload.
pub(super) struct PgFold;

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
            .map_err(|_| FoldErr)
    }
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

/// The simulated crash's panic payload: distinguishes the port's
/// staged crash from any other panic the spawned step could raise.
#[derive(Debug)]
pub(super) struct SimulatedCrash;

/// Simulates an external effect with key-based deduplication. Failure
/// and crash scripts exercise retries and the perform-to-append window.
#[derive(Clone, Default)]
pub(super) struct Port {
    pub(super) calls: Arc<Mutex<Vec<(String, Fx)>>>,
    pub(super) applied: Arc<Mutex<Vec<String>>>,
    pub(super) fail_for: Arc<Mutex<HashMap<String, u32>>>,
    pub(super) crash_on_apply: Arc<Mutex<Vec<String>>>,
}

impl Port {
    /// A port that shares this port's external system - the call log
    /// and the applied set - but carries none of its scripting: the
    /// fresh-process-after-a-crash shape.
    pub(super) fn healthy_sibling(&self) -> Port {
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
pub(super) struct Hook {
    pub(super) fires: Arc<Mutex<Vec<(String, u32, String)>>>,
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
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The rig
// ---------------------------------------------------------------------------

/// Shared PostgreSQL pool and per-run source category. Outbox entries
/// from earlier runs remain visible to the executor feed.
pub(super) struct Rig {
    pub(super) source_category: String,
    pub(super) db: PgDatabase,
    source: PgEventStreams<String, Src>,
    pub(super) source_feed: PgEventFeed<Src>,
    outbox: PgEventStreams<String, Record<Fx>>,
    outbox_feed: PgEventFeed<Record<Fx>>,
    ledger: PgEventStreams<String, Cmd>,
}

type PgRunner =
    Runner<PgDatabase, PgEventFeed<Src>, PgEventStreams<String, Record<Fx>>, Scripted, PgFold>;

type PgExecutor =
    Executor<PgEventFeed<Record<Fx>>, PgEventStreams<String, Record<Fx>>, Port, Hook, Fx>;

impl Rig {
    pub(super) async fn new() -> Self {
        let pool = pool_from_conn_str(&conn_str())
            .await
            .expect("pg pool from EPOCH_PG_TEST_URL");
        let source_category = unique("saga-src");
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

    pub(super) fn runner(&self, saga: Scripted) -> PgRunner {
        Runner::new(
            saga,
            self.db.clone(),
            self.source_feed.clone(),
            self.outbox.clone(),
            PgFold,
            RetryPolicy::default(),
        )
    }

    pub(super) fn executor(&self, saga: &str, port: Port, hook: Hook, budget: u32) -> PgExecutor {
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

    /// Append one bare source event to `stream`; returns its committed
    /// position in the shared log.
    pub(super) async fn place_on(&self, stream: &str, event: Src) -> FeedPosition {
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
    pub(super) async fn outbox_records(&self, saga: &str) -> Vec<(Record<Fx>, EventMetadata)> {
        stream_records(&self.outbox, saga).await
    }

    /// One ledger stream, whole: payload and envelope per record.
    pub(super) async fn ledger_records(&self, stream: &str) -> Vec<(Cmd, EventMetadata)> {
        stream_records(&self.ledger, stream).await
    }

    /// A bare event on a ledger stream, for pre-seeding conflicts.
    pub(super) async fn seed_ledger(&self, stream: &str, event: Cmd) {
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
pub(super) fn intent_envelope(key: &str) -> EventMetadata {
    let mut metadata = EventMetadata::new();
    metadata.insert(INTENT_METADATA_KEY, key);
    metadata
}

/// One ledger record: the payload with its intent-key envelope.
pub(super) fn is_command_record(record: &(Cmd, EventMetadata), payload: &Cmd, key: &str) -> bool {
    record.0 == *payload && record.1.get(INTENT_METADATA_KEY) == Some(key)
}

/// One intent record: the key in payload AND envelope, the request
/// the saga recorded.
pub(super) fn is_intent_record(
    record: &(Record<Fx>, EventMetadata),
    key: &str,
    payload: &Fx,
) -> bool {
    match record {
        (Record::Intent { intent, request }, metadata) => {
            intent.as_str() == key && request == payload && metadata == &intent_envelope(key)
        }
        _ => false,
    }
}

/// One Done record: the key in the payload, no envelope.
pub(super) fn is_done_record(record: &(Record<Fx>, EventMetadata), key: &str) -> bool {
    match record {
        (Record::Done { intent }, metadata) => intent.as_str() == key && metadata.is_empty(),
        _ => false,
    }
}

/// One Failed record: the key and the rendered port error, no
/// envelope.
pub(super) fn is_failed_record(
    record: &(Record<Fx>, EventMetadata),
    key: &str,
    error: &str,
) -> bool {
    match record {
        (
            Record::Failed {
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
pub(super) fn is_parked_record(
    record: &(Record<Fx>, EventMetadata),
    key: &str,
    attempts: u32,
    error: &str,
) -> bool {
    match record {
        (
            Record::Parked {
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

/// `RenderedIntentKey::from_rendered` is crate-internal, so an
/// integration test reaches the same constructor through the type's
/// public serde form: a rendered key is a transparent newtype over
/// its text.
pub(super) fn key_of(text: String) -> RenderedIntentKey {
    serde_json::from_value(serde_json::Value::String(text))
        .expect("a rendered key is a JSON string")
}

fn conn_str() -> String {
    let _ = dotenv::dotenv();
    std::env::var("EPOCH_PG_TEST_URL").expect("EPOCH_PG_TEST_URL must be set (see .env.example)")
}

/// Stream ids carry a ULID nonce, so runs never collide and storage
/// is never cleared.
pub(super) fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", rusty_ulid::generate_ulid_string())
}

pub(super) fn limit(n: usize) -> PollLimit {
    PollLimit::new(n).expect("a test limit is nonzero")
}
