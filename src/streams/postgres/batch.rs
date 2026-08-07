//! The PostgreSQL [`AtomicStreams`] implementation (ADR 0006).
//!
//! One transaction does the whole batch. It takes the same two-key
//! `pg_advisory_xact_lock(hashtext(category), hashtext(stream_key))`
//! that single append takes, over every stream the batch writes or
//! constrains, so batches and ordinary appends serialize against each
//! other on every stream they share. Locks are acquired in one total
//! order - lexicographic over the deduplicated two-integer hash tuples -
//! which is what makes a cross-category deadlock cycle impossible.
//!
//! Deriving that order needs the `hashtext` values, which only postgres
//! can compute, so the transaction spends one round trip hashing every
//! pair, sorts and deduplicates the tuples in the client, and then takes
//! the locks one statement at a time in that order. The alternative -
//! a single `SELECT pg_advisory_xact_lock(...) FROM (... ORDER BY ...)`
//! statement - rests acquisition order on the evaluation order of a
//! plan's target list, which postgres does not promise; ordering in the
//! client instead makes the order both guaranteed and testable
//! ([`lock_order`] is pinned by its own case below).
//!
//! Waiting is bounded per acquisition by a transaction-scoped
//! `lock_timeout`. Expiry is its own retryable outcome
//! ([`TransactError::LockTimeout`]), never a version conflict: the
//! transaction rolls back whole and its xact-scoped locks release with
//! it. Every check runs before the first insert and the transaction is
//! only committed once they all pass, so a rejected batch leaves no
//! partial rows.

use std::collections::HashMap;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tokio_postgres::error::SqlState;
use tokio_postgres::Transaction;

use crate::decider::Event;
use crate::streams::batch::{
    expectation_satisfied, AtomicStreams, Batch, BatchBuilder, BatchConflict, ConstraintViolation,
    DuplicateWrite, StreamRef, TransactError,
};
use crate::streams::{EventBatch, ExpectedVersion, StreamId, StreamVersion};

use super::{stored_position, version_of, PgPool, PgStreamsError};

/// The default per-acquisition wait bound. Long enough that a batch
/// queued behind ordinary work still commits, short enough that a
/// wedged writer surfaces as a retryable timeout rather than a hang.
const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// An event erased to the backend's wire form at push (ADR 0006):
/// encoding is fallible and happens before any transaction starts.
#[derive(Debug, Clone, PartialEq)]
pub struct EncodedEvent {
    event_type: String,
    data: Value,
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
}

impl PgDatabase {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            lock_timeout: DEFAULT_LOCK_TIMEOUT,
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

    /// A builder for a batch this handle can commit.
    pub fn batch(&self) -> PgBatchBuilder {
        PgBatchBuilder::new()
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
        // `EventBatch::as_slice` is an E1 hole outside this card's fill
        // bound; the batch's own invariant makes this nonempty.
        let encoded = events
            .0
            .iter()
            .map(|event| {
                Ok(EncodedEvent {
                    event_type: event.event_type(),
                    data: serde_json::to_value(event)?,
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

/// The batch's advisory locks in the total order ADR 0006 pins:
/// lexicographic over the deduplicated `hashtext` tuples. The order is
/// total across categories, which is what rules out a cross-category
/// deadlock cycle; ordering stream keys as text within a category would
/// not be.
fn lock_order(mut tuples: Vec<(i32, i32)>) -> Vec<(i32, i32)> {
    tuples.sort_unstable();
    tuples.dedup();
    tuples
}

/// Lock-timeout expiry is a distinct retryable outcome, never a version
/// conflict (ADR 0006). Everything else from an acquisition is a
/// backend fault.
fn acquisition_error(
    error: tokio_postgres::Error,
    lock_timeout: Duration,
) -> TransactError<PgStreamsError> {
    if error.code() == Some(&SqlState::LOCK_NOT_AVAILABLE) {
        TransactError::LockTimeout(lock_timeout)
    } else {
        TransactError::Backend(PgStreamsError::Connection(error))
    }
}

fn backend_error(error: tokio_postgres::Error) -> TransactError<PgStreamsError> {
    TransactError::Backend(PgStreamsError::Connection(error))
}

/// Hash every addressed stream to its lock tuple in one round trip, so
/// the client can sort them into the acquisition order.
async fn lock_tuples(
    tx: &Transaction<'_>,
    categories: &[String],
    keys: &[String],
) -> Result<Vec<(i32, i32)>, TransactError<PgStreamsError>> {
    let rows = tx
        .query(
            "SELECT hashtext(t.category) AS category_hash, \
                    hashtext(t.stream_key) AS key_hash \
               FROM unnest($1::text[], $2::text[]) AS t(category, stream_key)",
            &[&categories, &keys],
        )
        .await
        .map_err(backend_error)?;
    Ok(lock_order(
        rows.iter()
            .map(|row| (row.get("category_hash"), row.get("key_hash")))
            .collect(),
    ))
}

/// Every addressed stream's head under the held locks, the same
/// `COALESCE(MAX(sequence), 0)` observation single append makes.
async fn locked_heads(
    tx: &Transaction<'_>,
    categories: &[String],
    keys: &[String],
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
        .map_err(backend_error)?;

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

/// The head of a stream the batch just locked and observed.
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
        let tx = conn.transaction().await.map_err(backend_error)?;

        // Bound every acquisition below. The value is milliseconds from
        // a `Duration`, so there is nothing here a caller can inject;
        // zero would mean "wait forever", which is the one bound this
        // path must not set.
        let bound_ms = u64::try_from(self.lock_timeout.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        tx.batch_execute(&format!("SET LOCAL lock_timeout = '{bound_ms}ms'"))
            .await
            .map_err(backend_error)?;

        for (category_hash, key_hash) in lock_tuples(&tx, &categories, &keys).await? {
            tx.execute(
                "SELECT pg_advisory_xact_lock($1, $2)",
                &[&category_hash, &key_hash],
            )
            .await
            .map_err(|error| acquisition_error(error, self.lock_timeout))?;
        }

        let heads = locked_heads(&tx, &categories, &keys).await?;

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
                     (category, stream_key, event_type, sequence, event_data) \
                     VALUES ($1, $2, $3, $4, $5)",
                    &[
                        &stream.category().to_owned(),
                        &stream.key().to_owned(),
                        &event.event_type,
                        &sequence,
                        &event.data,
                    ],
                )
                .await
                .map_err(backend_error)?;
            }
        }

        tx.commit().await.map_err(backend_error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_are_ordered_across_categories_and_deduplicated() {
        // Two streams of one category, one of another, one repeated:
        // the order is lexicographic over the whole tuple, so it does
        // not depend on which category a stream came from.
        let ordered = lock_order(vec![(7, 2), (-3, 9), (7, -1), (-3, 9), (0, 0)]);
        assert_eq!(ordered, [(-3, 9), (0, 0), (7, -1), (7, 2)]);
    }

    #[test]
    fn the_order_is_the_same_whichever_way_a_batch_declares_its_streams() {
        let one = lock_order(vec![(5, 5), (1, 9), (1, 2)]);
        let other = lock_order(vec![(1, 2), (5, 5), (1, 9)]);
        assert_eq!(one, other, "two batches cannot wait on each other");
    }
}

#[cfg(all(test, feature = "postgres"))]
mod postgres_tests {
    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::streams::batch::BatchConstraint;
    use crate::streams::postgres::{pool_from_conn_str, PgEventStreams};
    use crate::streams::spec::under_deadline;
    use crate::streams::{EventStreams, StreamState};

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
        // `EventBatch::new` is an E1 hole outside this card's fill
        // bound; crate-internal construction here is trivially nonempty.
        EventBatch(vec![KidHoldsCard])
    }

    fn chore_event() -> EventBatch<CardAssigned> {
        EventBatch(vec![CardAssigned])
    }

    /// A draw: one write in each of two categories, both expecting the
    /// streams to be untouched. `reversed` flips the order the streams
    /// are declared in, which the batch's own lock ordering must make
    /// irrelevant.
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
    /// streams and declare them in opposite orders: the sorted
    /// acquisition is what keeps them from deadlocking, and the shared
    /// pre-batch head is what makes exactly one of them lose.
    #[tokio::test(flavor = "multi_thread")]
    async fn overlapping_batches_leave_exactly_one_winner() {
        under_deadline(async {
            let fixture = fixture().await;
            for _ in 0..10 {
                let kid = unique("kid");
                let chore = unique("chore");
                let first = draw(&fixture.db, &kid, &chore, false);
                let second = draw(&fixture.db, &kid, &chore, true);

                let (a, b) = tokio::join!(fixture.db.transact(first), fixture.db.transact(second));
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
                    StreamState::Present(EventBatch(vec![KidHoldsCard]))
                );
                assert_eq!(
                    chore_state(&fixture, &chore).await,
                    StreamState::Present(EventBatch(vec![CardAssigned]))
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

    /// A satisfied constraint over a stream the batch does not write,
    /// and a write onto an existing stream: the batch commits at the
    /// next sequence, exactly as an append would have.
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
                StreamState::Present(EventBatch(vec![KidHoldsCard, KidHoldsCard]))
            );
            assert_eq!(
                chore_state(&fixture, &chore).await,
                StreamState::Present(EventBatch(vec![CardAssigned]))
            );
        })
        .await;
    }
}
