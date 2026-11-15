//! Test consumer implementations and file-backed SQLite fixture wiring.
//! The saga writes `ReserveStock` as a command request; it does not reserve
//! inventory. Fake ports and hooks retain external deduplication state across
//! database reopenings so tests can distinguish calls from applied effects.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio_rusqlite::Connection;

use epoch::decider::Event;
use epoch::streams::feed::{ConsumerGroup, EventFeed, FeedPosition, PollLimit};
use epoch::streams::outbox::{
    CompensationHook, EffectPort, Executor, ParkedNotice, Record, OUTBOX_CATEGORY,
};
use epoch::streams::saga::{
    Command, CommandGroup, EffectRequest, IntentGroup, Reaction, ReactionFold, Runner, Saga,
};
use epoch::streams::sqlite::{
    SqliteBatchBuilder, SqliteDatabase, SqliteEventFeed, SqliteEventStreams, SqliteWriteError,
};
use epoch::streams::{
    BackoffSchedule, EventBatch, EventMetadata, EventStreams, ExpectedVersion, RecordedEvent,
    RenderedIntentKey, RetryBudget, RetryPolicy, SagaId, StreamRef, StreamState,
};

pub(super) const SOURCE: &str = "orders";
pub(super) const SOURCE_KEY: &str = "order-1";
pub(super) const LEDGER: &str = "ledger";
pub(super) const COMMAND_KEY: &str = "command-1";
pub(super) const SAGA: &str = "sqlite-saga";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Source {
    Placed { item: u32 },
}

impl Event for Source {
    type EntityId = ();

    fn event_type(&self) -> String {
        "Placed".to_owned()
    }

    fn get_id(&self) -> Self::EntityId {}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum CommandEvent {
    ReserveStock { item: u32 },
}

impl Event for CommandEvent {
    type EntityId = ();

    fn event_type(&self) -> String {
        "ReserveStock".to_owned()
    }

    fn get_id(&self) -> Self::EntityId {}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Effect {
    Export { item: u32 },
}

impl Event for Effect {
    type EntityId = ();

    fn event_type(&self) -> String {
        "Export".to_owned()
    }

    fn get_id(&self) -> Self::EntityId {}
}

pub(super) struct TestSaga {
    id: SagaId,
    expected: ExpectedVersion,
}

impl Saga for TestSaga {
    type Event = Source;
    type Command = CommandEvent;
    type Effect = Effect;

    fn id(&self) -> &SagaId {
        &self.id
    }

    fn react(&self, event: &Source) -> Vec<Reaction<CommandEvent, Effect>> {
        match event {
            Source::Placed { item } => vec![
                Reaction::Command(Command::new(
                    StreamRef::new(LEDGER, &COMMAND_KEY.to_owned()),
                    self.expected,
                    CommandEvent::ReserveStock { item: *item },
                )),
                Reaction::EffectRequest(EffectRequest::new(Effect::Export { item: *item })),
            ],
        }
    }
}

pub(super) struct SqliteFold;

impl ReactionFold<SqliteBatchBuilder> for SqliteFold {
    type Command = CommandEvent;
    type Effect = Effect;
    type Error = SqliteWriteError;

    fn push_command_group(
        &self,
        builder: &mut SqliteBatchBuilder,
        group: CommandGroup<'_, CommandEvent>,
    ) -> Result<(), SqliteWriteError> {
        let records: Vec<RecordedEvent<CommandEvent>> = group
            .records()
            .iter()
            .map(|record| RecordedEvent::keyed(record.payload().clone(), record.metadata().clone()))
            .collect();
        let batch =
            EventBatch::from_records(records).expect("a command group holds at least one record");
        builder.write(
            group.stream().category(),
            &group.stream().key().to_owned(),
            group.expected(),
            &batch,
        )
    }

    fn push_intent_group(
        &self,
        builder: &mut SqliteBatchBuilder,
        group: IntentGroup<'_, Effect>,
    ) -> Result<(), SqliteWriteError> {
        let records: Vec<RecordedEvent<Record<Effect>>> = group
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
        builder.write(
            group.stream().category(),
            &group.stream().key().to_owned(),
            ExpectedVersion::Any,
            &batch,
        )
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum PortBehavior {
    Healthy,
    Fail,
    CrashAfterApply,
}

#[derive(Debug, thiserror::Error)]
#[error("external service rejected the request")]
pub(super) struct PortError;

#[derive(Debug)]
pub(super) struct SimulatedCrash;

#[derive(Clone)]
pub(super) struct Port {
    pub(super) calls: Arc<Mutex<Vec<(RenderedIntentKey, Effect)>>>,
    pub(super) applied: Arc<Mutex<HashSet<RenderedIntentKey>>>,
    behavior: PortBehavior,
}

impl Port {
    pub(super) fn new(behavior: PortBehavior) -> Self {
        Self {
            calls: Arc::default(),
            applied: Arc::default(),
            behavior,
        }
    }

    pub(super) fn with_behavior(&self, behavior: PortBehavior) -> Self {
        Self {
            calls: Arc::clone(&self.calls),
            applied: Arc::clone(&self.applied),
            behavior,
        }
    }
}

impl EffectPort<Effect> for Port {
    type Error = PortError;

    async fn perform(&self, intent: &RenderedIntentKey, request: &Effect) -> Result<(), PortError> {
        // Every delivery is recorded, applied or not: the tests read this
        // log to tell redelivery from a silently dropped intent.
        self.calls
            .lock()
            .unwrap()
            .push((intent.clone(), request.clone()));
        if matches!(self.behavior, PortBehavior::Fail) {
            // Rejected before the effect lands, so the applied set stays
            // empty and the executor's Failed record is the only outcome.
            return Err(PortError);
        }
        {
            // The applied set is the external system's idempotency memory,
            // keyed by intent so a redelivery applies nothing new.
            let mut applied = self.applied.lock().unwrap();
            applied.insert(intent.clone());
        }
        if matches!(self.behavior, PortBehavior::CrashAfterApply) {
            // The external call landed, the outcome append never runs. The
            // guard is already dropped: a poisoned applied set would hide
            // the effect the reopening is supposed to find.
            std::panic::panic_any(SimulatedCrash);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum HookBehavior {
    Healthy,
    FailAfterApply,
}

#[derive(Debug, thiserror::Error)]
#[error("hook failed after applying compensation")]
pub(super) struct HookError;

#[derive(Clone)]
pub(super) struct Hook {
    pub(super) calls: Arc<Mutex<Vec<(RenderedIntentKey, u32, String)>>>,
    pub(super) applied: Arc<Mutex<HashSet<RenderedIntentKey>>>,
    behavior: HookBehavior,
}

impl Hook {
    pub(super) fn new(behavior: HookBehavior) -> Self {
        Self {
            calls: Arc::default(),
            applied: Arc::default(),
            behavior,
        }
    }

    pub(super) fn with_behavior(&self, behavior: HookBehavior) -> Self {
        Self {
            calls: Arc::clone(&self.calls),
            applied: Arc::clone(&self.applied),
            behavior,
        }
    }
}

impl CompensationHook<Effect> for Hook {
    type Error = HookError;

    async fn on_parked(&self, notice: &ParkedNotice<Effect>) -> Result<(), HookError> {
        // At-least-once: every park and every park replay lands here, so the
        // log carries repeats the applied set has to absorb.
        self.calls.lock().unwrap().push((
            notice.key().clone(),
            notice.attempts().get(),
            notice.error().to_owned(),
        ));
        {
            // Compensation is applied once per key, before any failure below.
            let mut applied = self.applied.lock().unwrap();
            applied.insert(notice.key().clone());
        }
        if matches!(self.behavior, HookBehavior::FailAfterApply) {
            // The compensation landed; the rejection keeps the cursor from
            // acknowledging the parked intent so the hook re-fires.
            return Err(HookError);
        }
        Ok(())
    }
}

pub(super) struct Rig {
    connection: Connection,
    pub(super) db: SqliteDatabase,
}

type TestRunner = Runner<
    SqliteDatabase,
    SqliteEventFeed<Source>,
    SqliteEventStreams<String, Record<Effect>>,
    TestSaga,
    SqliteFold,
>;

type TestExecutor = Executor<
    SqliteEventFeed<Record<Effect>>,
    SqliteEventStreams<String, Record<Effect>>,
    Port,
    Hook,
    Effect,
>;

impl Rig {
    pub(super) async fn open(path: &Path) -> Self {
        let connection = Connection::open(path)
            .await
            .expect("the fixture opens its file-backed SQLite database");
        let db = SqliteDatabase::new(connection.clone());
        db.migrate()
            .await
            .expect("the fixture migrates the root schema");
        Self { connection, db }
    }

    pub(super) async fn close(self) {
        let Rig { connection, db } = self;
        drop(db);
        connection
            .close()
            .await
            .expect("the fixture's SQLite connection closes cleanly");
    }

    pub(super) fn runner(&self, expected: ExpectedVersion) -> TestRunner {
        Runner::new(
            TestSaga {
                id: SagaId::new(SAGA).expect("the fixture saga has a nonempty id"),
                expected,
            },
            self.db.clone(),
            self.db.feed::<Source>(SOURCE),
            self.db.category::<String, Record<Effect>>(OUTBOX_CATEGORY),
            SqliteFold,
            RetryPolicy::default(),
        )
    }

    pub(super) fn executor(&self, port: Port, hook: Hook, budget: u32) -> TestExecutor {
        Executor::new(
            self.db.feed::<Record<Effect>>(OUTBOX_CATEGORY),
            self.db.category::<String, Record<Effect>>(OUTBOX_CATEGORY),
            port,
            hook,
            SagaId::new(SAGA).expect("the fixture saga has a nonempty id"),
            RetryPolicy::new(
                RetryBudget::new(budget).expect("the fixture budget is nonzero"),
                BackoffSchedule::new(Duration::from_millis(1), Duration::from_millis(2))
                    .expect("the fixture backoff base does not exceed its cap"),
            ),
        )
    }

    pub(super) async fn place(&self, item: u32) -> FeedPosition {
        self.db
            .category::<String, Source>(SOURCE)
            .append(
                ExpectedVersion::Any,
                &SOURCE_KEY.to_owned(),
                &EventBatch::new(vec![Source::Placed { item }])
                    .expect("one source event is nonempty"),
            )
            .await
            .expect("the source append succeeds");
        let entries = self
            .db
            .feed::<Source>(SOURCE)
            .poll(
                &ConsumerGroup::new("probe").expect("the probe group is nonempty"),
                PollLimit::new(1000).expect("the probe limit is nonzero"),
            )
            .await
            .expect("the probe poll succeeds");
        entries
            .last()
            .expect("the append just landed in the source feed")
            .position()
    }

    pub(super) async fn records<E>(&self, category: &str, key: &str) -> Vec<(E, EventMetadata)>
    where
        E: Event + Serialize + DeserializeOwned + Send + Sync + std::fmt::Debug,
    {
        match self
            .db
            .category::<String, E>(category)
            .load_stream(&key.to_owned())
            .await
            .expect("the fixture reads a committed stream")
        {
            StreamState::Present(batch) => batch
                .into_records()
                .into_iter()
                .map(|record| record.into_parts())
                .collect(),
            StreamState::Missing => Vec::new(),
        }
    }
}
