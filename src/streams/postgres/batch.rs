//! The PostgreSQL [`AtomicStreams`] implementation (ADR 0006, under
//! ADR 0010's post-pivot single-writer funnel).
//!
//! One transaction does the whole batch under the one global writer
//! lock every event transaction shares, so insert order is commit
//! order by construction (the module doc in `super` carries the
//! theorem and its operating assumptions). Every check runs before
//! the first insert, so a rejected batch leaves no partial rows.

use std::collections::HashMap;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tokio_postgres::error::{DbError, SqlState};
use tokio_postgres::Transaction;

use crate::decider::Event;
use crate::streams::batch::{
    expectation_satisfied, AtomicStreams, Batch, BatchBuilder, BatchConflict, BatchSource,
    ConstraintViolation, DuplicateIntent, DuplicateWrite, StreamRef, TransactError,
};
use crate::streams::{EventBatch, ExpectedVersion, StreamId, StreamVersion};

use super::{
    configure_writer_timeouts, stored_position, version_of, PgPool, PgStreamsError,
    DEFAULT_IDLE_TRANSACTION_TIMEOUT, DEFAULT_LOCK_TIMEOUT,
};

/// An event erased to the backend's wire form at push (ADR 0006):
/// encoding is fallible and happens before any transaction starts.
/// The envelope rides as its JSON form, the same as the payload.
#[derive(Debug, Clone, PartialEq)]
pub struct EncodedEvent {
    event_type: String,
    data: Value,
    metadata: Value,
}

/// A batch the postgres database can commit.
pub type PgBatch = Batch<EncodedEvent>;

/// The builder for one.
pub type PgBatchBuilder = BatchBuilder<EncodedEvent>;

/// The postgres database handle: the value that owns the pool, and so
/// the only value whose scope covers a cross-category batch (ADR 0006's
/// ownership seam). Per-category [`super::PgEventStreams`] stores stay
/// independently constructible for the append path; a batch write
/// carries a category, a rendered stream key and its events, never a
/// store, so the only pool a transact can reach is this handle's.
#[derive(Clone)]
pub struct PgDatabase {
    pool: PgPool,
    lock_timeout: Duration,
    idle_transaction_timeout: Duration,
}

impl PgDatabase {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            lock_timeout: DEFAULT_LOCK_TIMEOUT,
            idle_transaction_timeout: DEFAULT_IDLE_TRANSACTION_TIMEOUT,
        }
    }

    /// Set the per-acquisition wait bound. A batch's worst-case wait is
    /// this bound once per distinct lock it takes, not once per batch.
    pub fn with_lock_timeout(self, lock_timeout: Duration) -> Self {
        Self {
            lock_timeout,
            ..self
        }
    }

    /// Set the idle-in-transaction bound (the server's
    /// `idle_in_transaction_session_timeout` GUC): how long the batch's
    /// transaction may sit without an active statement before the
    /// server terminates it, evicting a wedged holder. Only idle open
    /// transactions are bounded; active statements, the commit, and
    /// total client duration are not.
    pub fn with_idle_transaction_timeout(self, idle_transaction_timeout: Duration) -> Self {
        Self {
            idle_transaction_timeout,
            ..self
        }
    }

    /// A builder for a batch this handle can commit.
    pub fn batch(&self) -> PgBatchBuilder {
        PgBatchBuilder::new()
    }
}

impl BatchSource for PgDatabase {
    type Wire = EncodedEvent;

    fn builder(&self) -> PgBatchBuilder {
        self.batch()
    }
}

impl PgBatchBuilder {
    /// Add this stream's write to the batch, encoding its events to the
    /// backend's JSONB wire form now: a failed encoding is a build-time
    /// error, before any transaction starts. `expected` is checked
    /// against the stream's pre-batch head when the batch commits, the
    /// same check single append makes.
    pub fn write<Id, E>(
        &mut self,
        category: &str,
        id: &Id,
        expected: ExpectedVersion,
        events: &EventBatch<E>,
    ) -> Result<(), PgWriteError>
    where
        Id: StreamId,
        E: Event + Serialize,
    {
        let encoded = events
            .records()
            .iter()
            .map(|record| {
                Ok(EncodedEvent {
                    event_type: record.event().event_type(),
                    data: serde_json::to_value(record.event())?,
                    metadata: serde_json::to_value(record.metadata())?,
                })
            })
            .collect::<Result<Vec<_>, serde_json::Error>>()?;
        Ok(self.push(StreamRef::new(category, id), expected, encoded)?)
    }
}

/// Why a write could not join a batch.
#[derive(Debug, thiserror::Error)]
pub enum PgWriteError {
    /// The batch already writes this stream.
    #[error(transparent)]
    Duplicate(#[from] DuplicateWrite),
    /// The events do not encode to the backend's JSONB wire form.
    #[error("event payload serialization error: {0}")]
    Encoding(#[from] serde_json::Error),
}

/// Lock-timeout expiry is a distinct retryable outcome, never a version
/// conflict (ADR 0006), and the bound is transaction-scoped: it applies
/// to the funnel lock the batch takes and equally to the row
/// and index locks the inserts and the commit take on their own. Every
/// statement inside a transact routes its failure through here so an
/// expiry is classified the same wherever it arises.
fn statement_error(
    error: tokio_postgres::Error,
    lock_timeout: Duration,
) -> TransactError<PgStreamsError> {
    if error.code() == Some(&SqlState::LOCK_NOT_AVAILABLE) {
        // The GUC floors zero/sub-millisecond bounds to 1ms, so the
        // payload names the bound actually in effect.
        TransactError::LockTimeout(Duration::from_millis(super::lock_timeout_ms(lock_timeout)))
    } else {
        TransactError::Backend(PgStreamsError::Connection(error))
    }
}

/// The storage-level intent-key index (migration
/// `0004-feed-cursors-and-intent-key.sql`): unique on the keyed
/// envelope, partial on the outbox category, so only a duplicate
/// outbox intent write can ever name it.
const OUTBOX_INTENT_INDEX: &str = "stream_events_outbox_intent";

/// A per-event insert's failure. The intent-key index rejecting the
/// row is the saga runner's typed redelivery signal on the write's
/// stream; every other failure - including any other unique violation,
/// such as the primary key - falls through to the shared statement
/// classification unchanged.
fn insert_error(
    error: tokio_postgres::Error,
    stream: &StreamRef,
    lock_timeout: Duration,
) -> TransactError<PgStreamsError> {
    if error.code() == Some(&SqlState::UNIQUE_VIOLATION)
        && error.as_db_error().and_then(DbError::constraint) == Some(OUTBOX_INTENT_INDEX)
    {
        TransactError::DuplicateIntent(DuplicateIntent::new(stream.clone()))
    } else {
        statement_error(error, lock_timeout)
    }
}

/// Every addressed stream's head inside the funnel, in one grouped
/// query.
async fn locked_heads(
    tx: &Transaction<'_>,
    categories: &[String],
    keys: &[String],
    lock_timeout: Duration,
) -> Result<HashMap<(String, String), StreamVersion>, TransactError<PgStreamsError>> {
    let rows = tx
        .query(
            "SELECT t.category AS category, t.stream_key AS stream_key, \
                    COALESCE(MAX(e.sequence), 0) AS head \
               FROM unnest($1::text[], $2::text[]) AS t(category, stream_key) \
               LEFT JOIN stream_events e \
                 ON e.category = t.category AND e.stream_key = t.stream_key \
              GROUP BY t.category, t.stream_key",
            &[&categories, &keys],
        )
        .await
        .map_err(|error| statement_error(error, lock_timeout))?;

    Ok(rows
        .iter()
        .map(|row| {
            let head: i64 = row.get("head");
            (
                (row.get("category"), row.get("stream_key")),
                version_of(stored_position(head)),
            )
        })
        .collect())
}

/// The head of a stream the batch observed inside the funnel.
fn head_of(heads: &HashMap<(String, String), StreamVersion>, stream: &StreamRef) -> StreamVersion {
    *heads
        .get(&(stream.category().to_owned(), stream.key().to_owned()))
        .expect("every addressed stream was observed under its lock")
}

impl AtomicStreams for PgDatabase {
    type Batch = PgBatch;
    type Error = PgStreamsError;

    async fn transact(&self, batch: PgBatch) -> Result<(), TransactError<PgStreamsError>> {
        let (categories, keys): (Vec<String>, Vec<String>) = batch
            .locked_streams()
            .iter()
            .map(|stream| (stream.category().to_owned(), stream.key().to_owned()))
            .unzip();

        let mut conn = self.pool.get().await.map_err(PgStreamsError::Pool)?;
        // A BEGIN failure is pre-bound: a connection error, never a
        // lock-wait classification (the GUC below is not in effect yet).
        let tx = conn
            .transaction()
            .await
            .map_err(|error| TransactError::Backend(PgStreamsError::Connection(error)))?;

        // A setup failure here is a connection error, never a lock-wait
        // classification.
        configure_writer_timeouts(&tx, self.lock_timeout, self.idle_transaction_timeout)
            .await
            .map_err(|error| TransactError::Backend(PgStreamsError::Connection(error)))?;

        tx.execute("SELECT pg_advisory_xact_lock($1)", &[&super::WRITER_LOCK])
            .await
            .map_err(|error| statement_error(error, self.lock_timeout))?;

        let heads = locked_heads(&tx, &categories, &keys, self.lock_timeout).await?;

        for constrained in batch.constraints() {
            let observed = head_of(&heads, constrained.stream());
            if !constrained.constraint().satisfied_by(observed) {
                return Err(TransactError::ConstraintViolated(ConstraintViolation::new(
                    constrained.stream().clone(),
                    constrained.constraint(),
                    observed,
                )));
            }
        }

        for write in batch.writes() {
            let observed = head_of(&heads, write.stream());
            if !expectation_satisfied(write.expected(), observed) {
                return Err(TransactError::Conflict(BatchConflict::new(
                    write.stream().clone(),
                    write.expected(),
                    observed,
                )));
            }
        }

        for write in batch.writes() {
            let stream = write.stream();
            let head = match head_of(&heads, stream) {
                StreamVersion::NoStream => 0,
                StreamVersion::Exact(sequence) => sequence.get(),
            };
            for (offset, event) in write.events().iter().enumerate() {
                let sequence = i64::try_from(head + offset as u64 + 1)
                    .expect("a stream's length fits the stored sequence");
                tx.execute(
                    "INSERT INTO stream_events \
                     (category, stream_key, event_type, sequence, event_data, event_metadata) \
                     VALUES ($1, $2, $3, $4, $5, $6)",
                    &[
                        &stream.category().to_owned(),
                        &stream.key().to_owned(),
                        &event.event_type,
                        &sequence,
                        &event.data,
                        &event.metadata,
                    ],
                )
                .await
                .map_err(|error| insert_error(error, stream, self.lock_timeout))?;
            }
        }

        tx.commit()
            .await
            .map_err(|error| statement_error(error, self.lock_timeout))?;
        Ok(())
    }
}

#[cfg(all(test, feature = "postgres"))]
mod postgres_tests {
    use std::sync::Arc;

    use serde::{Deserialize, Serialize};
    use tokio::sync::Barrier;

    use super::*;
    use crate::streams::batch::BatchConstraint;
    use crate::streams::postgres::{pool_from_conn_str, PgEventStreams};
    use crate::streams::spec::under_deadline;
    use crate::streams::{AppendError, EventMetadata, EventStreams, RecordedEvent, StreamState};

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct KidHoldsCard;

    impl Event for KidHoldsCard {
        type EntityId = ();

        fn event_type(&self) -> String {
            "KidHoldsCard".to_owned()
        }

        fn get_id(&self) -> Self::EntityId {}
    }

    /// A second, unrelated event type: a batch spans categories whose
    /// event types differ, which is ADR 0006's mixed-type driver.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct CardAssigned;

    impl Event for CardAssigned {
        type EntityId = ();

        fn event_type(&self) -> String {
            "CardAssigned".to_owned()
        }

        fn get_id(&self) -> Self::EntityId {}
    }

    const KIDS: &str = "batch-kids";
    const CHORES: &str = "batch-chores";
    const POOL: &str = "batch-pool";

    /// The database handle plus the two category views the cases read
    /// through, all on one migrated pool.
    struct Fixture {
        db: PgDatabase,
        pool: PgPool,
        kids: PgEventStreams<String, KidHoldsCard>,
        chores: PgEventStreams<String, CardAssigned>,
    }

    async fn fixture() -> Fixture {
        // These tests migrate schema and write events into whatever
        // database they are pointed at, so there is no safe default to
        // fall back on: an unset variable must stop the run rather than
        // silently pick a database.
        let _ = dotenv::dotenv();
        let conn_str = std::env::var("EPOCH_PG_TEST_URL").expect(
            "EPOCH_PG_TEST_URL must be set (see .env.example; \
             `cp .env.example .env && docker compose up -d`)",
        );
        let pool = pool_from_conn_str(&conn_str)
            .await
            .expect("pg pool from EPOCH_PG_TEST_URL");
        let kids = PgEventStreams::new(pool.clone(), KIDS);
        kids.migrate().await.expect("schema migrates");
        Fixture {
            db: PgDatabase::new(pool.clone()),
            pool: pool.clone(),
            kids,
            chores: PgEventStreams::new(pool, CHORES),
        }
    }

    /// Stream ids carry a ULID nonce, so runs never collide and storage
    /// is never cleared.
    fn unique(prefix: &str) -> String {
        format!("{prefix}-{}", rusty_ulid::generate_ulid_string())
    }

    fn kid_event() -> EventBatch<KidHoldsCard> {
        EventBatch::new(vec![KidHoldsCard]).expect("a single event is nonempty")
    }

    fn chore_event() -> EventBatch<CardAssigned> {
        EventBatch::new(vec![CardAssigned]).expect("a single event is nonempty")
    }

    /// A draw: one write in each of two categories, both expecting the
    /// streams to be untouched. `reversed` flips the declaration
    /// order; the outcome must be the same either way.
    fn draw(db: &PgDatabase, kid: &str, chore: &str, reversed: bool) -> PgBatch {
        let mut builder = db.batch();
        let write_kid = |builder: &mut PgBatchBuilder| {
            builder
                .write(
                    KIDS,
                    &kid.to_owned(),
                    ExpectedVersion::NoStream,
                    &kid_event(),
                )
                .expect("one write per stream, encodable")
        };
        let write_chore = |builder: &mut PgBatchBuilder| {
            builder
                .write(
                    CHORES,
                    &chore.to_owned(),
                    ExpectedVersion::NoStream,
                    &chore_event(),
                )
                .expect("one write per stream, encodable")
        };
        if reversed {
            write_chore(&mut builder);
            write_kid(&mut builder);
        } else {
            write_kid(&mut builder);
            write_chore(&mut builder);
        }
        builder.build().expect("the batch has writes")
    }

    async fn kid_state(fixture: &Fixture, kid: &str) -> StreamState<KidHoldsCard> {
        fixture
            .kids
            .load_stream(&kid.to_owned())
            .await
            .expect("load succeeds")
    }

    async fn chore_state(fixture: &Fixture, chore: &str) -> StreamState<CardAssigned> {
        fixture
            .chores
            .load_stream(&chore.to_owned())
            .await
            .expect("load succeeds")
    }

    /// The consumer-shaped smoke test. Two batches want the same two
    /// streams and declare them in opposite orders: exactly one wins,
    /// the other conflicts on the shared pre-batch head, and no
    /// partial rows survive.
    ///
    /// Both batches are built before either transacts and the two tasks
    /// release together on a barrier, so the transactions genuinely
    /// contend on the funnel rather than one finishing before the
    /// other begins.
    #[tokio::test(flavor = "multi_thread")]
    async fn overlapping_batches_leave_exactly_one_winner() {
        under_deadline(async {
            let fixture = fixture().await;
            for _ in 0..10 {
                let kid = unique("kid");
                let chore = unique("chore");
                let first = draw(&fixture.db, &kid, &chore, false);
                let second = draw(&fixture.db, &kid, &chore, true);

                let barrier = Arc::new(Barrier::new(2));
                let racer = |batch: PgBatch| {
                    let db = fixture.db.clone();
                    let barrier = Arc::clone(&barrier);
                    tokio::spawn(async move {
                        barrier.wait().await;
                        db.transact(batch).await
                    })
                };

                let (a, b) = tokio::join!(racer(first), racer(second));
                let a = a.expect("racer task must not panic");
                let b = b.expect("racer task must not panic");
                match (a, b) {
                    (Ok(()), Err(TransactError::Conflict(_)))
                    | (Err(TransactError::Conflict(_)), Ok(())) => {}
                    (a, b) => {
                        panic!("expected exactly one winner and one conflict: {a:?}, {b:?}")
                    }
                }

                // One winner wrote each stream once; the loser wrote
                // neither, so no partial rows survived.
                assert_eq!(
                    kid_state(&fixture, &kid).await,
                    StreamState::Present(
                        EventBatch::new(vec![KidHoldsCard]).expect("a single event is nonempty")
                    )
                );
                assert_eq!(
                    chore_state(&fixture, &chore).await,
                    StreamState::Present(
                        EventBatch::new(vec![CardAssigned]).expect("a single event is nonempty")
                    )
                );
            }
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_violated_constraint_aborts_the_whole_batch() {
        under_deadline(async {
            let fixture = fixture().await;
            let (kid, chore, pinned) = (unique("kid"), unique("chore"), unique("pool"));

            let mut builder = fixture.db.batch();
            builder
                .write(KIDS, &kid, ExpectedVersion::NoStream, &kid_event())
                .expect("one write per stream");
            builder
                .write(CHORES, &chore, ExpectedVersion::NoStream, &chore_event())
                .expect("one write per stream");
            // A pin on a stream the batch does not write, and does not
            // hold: the whole batch must roll back.
            builder.require(POOL, &pinned, BatchConstraint::StreamExists);

            let outcome = fixture
                .db
                .transact(builder.build().expect("the batch has writes"))
                .await;
            let violation = match outcome {
                Err(TransactError::ConstraintViolated(violation)) => violation,
                other => panic!("expected a constraint violation, got {other:?}"),
            };
            assert_eq!(violation.stream().category(), POOL);
            assert_eq!(violation.observed(), StreamVersion::NoStream);

            assert_eq!(kid_state(&fixture, &kid).await, StreamState::Missing);
            assert_eq!(
                chore_state(&fixture, &chore).await,
                StreamState::Missing,
                "a violated constraint leaves no partial insert"
            );
        })
        .await;
    }

    /// Append parity: a batch and a single append take the same lock on
    /// the same stream and evaluate the same head, so the batch loses
    /// to an append that got there first - and takes its other write
    /// down with it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_batch_and_an_append_contend_on_one_stream() {
        under_deadline(async {
            let fixture = fixture().await;
            let (kid, chore) = (unique("kid"), unique("chore"));
            fixture
                .kids
                .append(ExpectedVersion::NoStream, &kid, &kid_event())
                .await
                .expect("the append lands first");

            let outcome = fixture
                .db
                .transact(draw(&fixture.db, &kid, &chore, false))
                .await;
            let conflict = match outcome {
                Err(TransactError::Conflict(conflict)) => conflict,
                other => panic!("expected a version conflict, got {other:?}"),
            };
            assert_eq!(conflict.stream().category(), KIDS);
            assert_eq!(
                chore_state(&fixture, &chore).await,
                StreamState::Missing,
                "the batch's other write rolled back with it"
            );
        })
        .await;
    }

    /// The same parity under a genuine race, which is what pins the
    /// lock identity rather than just the check: a batch and a plain
    /// append go for one stream released together on a barrier. Either
    /// may win - they are contending for the same physical advisory
    /// lock - but exactly one does, and the batch's second stream
    /// follows its own outcome.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_batch_and_an_append_race_for_one_stream() {
        under_deadline(async {
            let fixture = fixture().await;
            for _ in 0..10 {
                let (kid, chore) = (unique("kid"), unique("chore"));
                let batch = draw(&fixture.db, &kid, &chore, false);
                let barrier = Arc::new(Barrier::new(2));

                let batch_side = {
                    let db = fixture.db.clone();
                    let barrier = Arc::clone(&barrier);
                    tokio::spawn(async move {
                        barrier.wait().await;
                        db.transact(batch).await
                    })
                };
                let append_side = {
                    let kids = fixture.kids.clone();
                    let barrier = Arc::clone(&barrier);
                    let kid = kid.clone();
                    tokio::spawn(async move {
                        barrier.wait().await;
                        kids.append(ExpectedVersion::NoStream, &kid, &kid_event())
                            .await
                    })
                };

                let (batched, appended) = tokio::join!(batch_side, append_side);
                let batched = batched.expect("batch task must not panic");
                let appended = appended.expect("append task must not panic");

                match (batched, appended) {
                    (Ok(()), Err(AppendError::Conflict(_))) => assert_eq!(
                        chore_state(&fixture, &chore).await,
                        StreamState::Present(
                            EventBatch::new(vec![CardAssigned])
                                .expect("a single event is nonempty")
                        ),
                        "the batch won, so both its writes are stored"
                    ),
                    (Err(TransactError::Conflict(_)), Ok(_)) => assert_eq!(
                        chore_state(&fixture, &chore).await,
                        StreamState::Missing,
                        "the batch lost, so neither of its writes is stored"
                    ),
                    (batched, appended) => panic!(
                        "expected exactly one winner on the shared stream: \
                         {batched:?}, {appended:?}"
                    ),
                }

                // Whoever won, the contested stream holds one event.
                assert_eq!(
                    kid_state(&fixture, &kid).await,
                    StreamState::Present(
                        EventBatch::new(vec![KidHoldsCard]).expect("a single event is nonempty")
                    )
                );
            }
        })
        .await;
    }

    /// Run a two-stream draw with `bound` against a wedged funnel: a
    /// raw holder on a second connection already holds the global
    /// writer lock in an open transaction, released after the transact
    /// returns. The fixture and the kid key come back with the outcome
    /// so the caller can assert nothing was stored.
    async fn transact_against_wedged_funnel(
        bound: Duration,
    ) -> (Fixture, String, Result<(), TransactError<PgStreamsError>>) {
        let fixture = fixture().await;
        let (kid, chore) = (unique("kid"), unique("chore"));

        // The holder's connection borrows its pool, so the pool is a
        // local clone: nothing may borrow `fixture` when it is
        // returned below.
        let pool = fixture.pool.clone();
        let mut holder = pool.get().await.expect("a second connection");
        let held = holder.transaction().await.expect("holder transaction");
        held.execute(
            "SELECT pg_advisory_xact_lock($1)",
            &[&super::super::WRITER_LOCK],
        )
        .await
        .expect("the holder takes the funnel lock");

        let outcome = fixture
            .db
            .clone()
            .with_lock_timeout(bound)
            .transact(draw(&fixture.db, &kid, &chore, false))
            .await;
        held.rollback().await.expect("the holder releases");
        (fixture, kid, outcome)
    }

    /// A lock the batch cannot take inside its bound is a retryable
    /// timeout, not a conflict.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unavailable_lock_is_a_retryable_timeout() {
        under_deadline(async {
            let bound = Duration::from_millis(100);
            let (fixture, kid, outcome) = transact_against_wedged_funnel(bound).await;
            assert!(
                matches!(outcome, Err(TransactError::LockTimeout(reported)) if reported == bound),
                "expected a retryable lock timeout, got {outcome:?}"
            );
            assert_eq!(
                kid_state(&fixture, &kid).await,
                StreamState::Missing,
                "a timed-out batch stores nothing"
            );
        })
        .await;
    }

    /// A zero configured bound is floored to 1ms server-side, and the
    /// timeout payload reports that effective bound, not the zero the
    /// caller configured.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_zero_funnel_bound_times_out_reporting_the_server_floor() {
        under_deadline(async {
            let (fixture, kid, outcome) =
                transact_against_wedged_funnel(Duration::ZERO).await;
            assert!(
                matches!(outcome, Err(TransactError::LockTimeout(reported)) if reported == Duration::from_millis(1)),
                "expected the floored 1ms bound, got {outcome:?}"
            );
            assert_eq!(
                kid_state(&fixture, &kid).await,
                StreamState::Missing,
                "a timed-out batch stores nothing"
            );
        })
        .await;
    }

    /// A keyed batch write's envelope reaches the stored row (the
    /// runner's intent keys ride exactly this path).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_keyed_batch_write_stores_its_envelope() {
        under_deadline(async {
            let fixture = fixture().await;
            let kid = unique("kid");
            let mut envelope = EventMetadata::new();
            envelope.insert("intent", "saga-3/draw-7/1");
            let events =
                EventBatch::from_records(vec![RecordedEvent::keyed(KidHoldsCard, envelope)])
                    .expect("one event is nonempty");

            let mut builder = fixture.db.batch();
            builder
                .write(KIDS, &kid, ExpectedVersion::NoStream, &events)
                .expect("one write per stream");
            fixture
                .db
                .transact(builder.build().expect("the batch has writes"))
                .await
                .expect("the batch commits");

            let conn = fixture.pool.get().await.expect("assertion connection");
            let stored = conn
                .query_one(
                    "SELECT event_metadata FROM stream_events \
                     WHERE category = $1 AND stream_key = $2",
                    &[&KIDS, &kid],
                )
                .await
                .expect("envelope read");
            assert_eq!(
                stored.get::<_, serde_json::Value>("event_metadata"),
                serde_json::json!({"intent": "saga-3/draw-7/1"})
            );
        })
        .await;
    }

    /// A satisfied constraint and two writes, one onto an existing
    /// stream: the batch commits at the next sequence, exactly as an
    /// append would have.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_satisfied_batch_commits_every_write() {
        under_deadline(async {
            let fixture = fixture().await;
            let (kid, chore) = (unique("kid"), unique("chore"));
            fixture
                .kids
                .append(ExpectedVersion::NoStream, &kid, &kid_event())
                .await
                .expect("seed append");

            let mut builder = fixture.db.batch();
            builder
                .write(
                    KIDS,
                    &kid,
                    ExpectedVersion::Exact(
                        crate::streams::StreamSequence::new(1).expect("1 is a position"),
                    ),
                    &kid_event(),
                )
                .expect("one write per stream");
            builder
                .write(CHORES, &chore, ExpectedVersion::NoStream, &chore_event())
                .expect("one write per stream");
            builder.require(KIDS, &kid, BatchConstraint::StreamExists);

            fixture
                .db
                .transact(builder.build().expect("the batch has writes"))
                .await
                .expect("every check is satisfied");

            assert_eq!(
                kid_state(&fixture, &kid).await,
                StreamState::Present(
                    EventBatch::new(vec![KidHoldsCard, KidHoldsCard])
                        .expect("two events are nonempty")
                )
            );
            assert_eq!(
                chore_state(&fixture, &chore).await,
                StreamState::Present(
                    EventBatch::new(vec![CardAssigned]).expect("a single event is nonempty")
                )
            );
        })
        .await;
    }
}
