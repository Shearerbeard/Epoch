//! PostgreSQL [`EventStreams`] backend on the unified 1-based sequence
//! semantics (ADR 0003) and the shared-store `Clone` contract
//! (ADR 0005): clones share one pool and one category, so concurrent
//! writers contend on the same stored streams.
//!
//! Append atomicity, single-writer funnel (ADR 0010 post-pivot).
//! Every event transaction - single append or atomic batch - first
//! takes one global transaction-scoped advisory lock, so exactly one
//! writer is ever in flight. That makes insert order commit order by
//! construction, which is the property the feed's plain-maximum
//! cursor rests on: no event can become visible below an already
//! committed value. The version check inside the funnel decides
//! conflicts exactly as before; the funnel only serializes who may
//! run one. The lock is transaction-scoped, so a cancelled future
//! releases it at rollback rather than leaking. The unique position
//! constraint in the first migration step backstops the same
//! invariant at the storage layer, alongside a check constraint that
//! keeps positions 1-based.
//!
//! The funnel's ordering theorem rests on operating assumptions the
//! schema cannot enforce; every writer of `stream_events` must honor
//! them, forever. Every writer goes through the funnel above - raw
//! SQL, backfills, second services, and pre-funnel binaries from a
//! mixed-version deploy included. The backing sequence keeps its
//! default `CACHE 1`: a larger cache preallocates per session, so two
//! sessions can commit cached values out of draw order even while
//! obeying the lock. Nobody rewinds the sequence (`setval`,
//! `RESTART`): a re-issued value can land below an already-visible
//! one. Transactions run at the default READ COMMITTED: a pool
//! configured for a stronger isolation can stale the head read that
//! backs the version check. A violator's damage is scoped to the
//! categories it writes, and a late out-of-order commit below an
//! advanced feed cursor is silent - no assertion, constraint, or
//! poll invariant can detect it after the fact.
//!
//! Read consistency. Each load is a single statement, which is its own
//! snapshot: a rival append committing mid-read cannot split a read
//! across two views and hand back events that disagree with the
//! position reported with them.

use std::fmt::Debug;
use std::marker::PhantomData;

use bb8::Pool;
use bb8_postgres::PostgresConnectionManager;
use serde::{de::DeserializeOwned, Serialize};
use thiserror::Error;
use tokio_postgres::{NoTls, Row};

use crate::decider::Event;

use super::{
    AppendError, CategoryEvent, EventBatch, EventStreams, ExpectedVersion, LoadError, StreamId,
    StreamSequence, StreamSlice, StreamState, StreamVersion, VersionConflict,
};

mod batch;
mod feed;
mod migrations;

pub use batch::{EncodedEvent, PgBatch, PgBatchBuilder, PgDatabase, PgWriteError};
pub use feed::PgEventFeed;

/// The connection pool every store in this module runs on.
pub type PgPool = Pool<PostgresConnectionManager<NoTls>>;

/// The single-writer funnel: one advisory lock every event
/// transaction takes before anything else. Distinct from the
/// migration lock and from every two-int stream lock; spells
/// "epwriter".
pub(crate) const WRITER_LOCK: i64 = 0x6570_7772_6974_6572;

/// The default funnel wait bound for both write paths (single append
/// and atomic batch). Long enough that a writer queued behind
/// ordinary work still commits, short enough that a wedged holder
/// surfaces as a retryable timeout rather than a hang.
pub(crate) const DEFAULT_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The default idle-in-transaction bound for the write paths: how
/// long an event transaction may sit without an active statement
/// before the server terminates it, evicting a wedged holder.
/// Active SQL, commit, and total client duration remain outside this
/// bound.
pub(crate) const DEFAULT_IDLE_TRANSACTION_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(30);

/// A `Duration` as the `lock_timeout` GUC's integer milliseconds.
/// The value is a count, so there is nothing a caller can inject;
/// `.max(1)` keeps zero ("wait forever") unsettable - the one bound
/// these paths must not set.
pub(crate) fn lock_timeout_ms(timeout: std::time::Duration) -> u64 {
    u64::try_from(timeout.as_millis())
        .unwrap_or(u64::MAX)
        .max(1)
}

/// Apply both writer timeout bounds to one event transaction before
/// the funnel acquisition, shared by the single append and the atomic
/// batch paths: `SET LOCAL lock_timeout` bounding the funnel wait
/// (converted by [`lock_timeout_ms`], so zero or a sub-millisecond
/// bound normalizes to the 1ms floor) and `SET LOCAL
/// idle_in_transaction_session_timeout` bounding how long the
/// transaction may sit without an active statement, both in one
/// single command. A bound the server rejects as out of range fails
/// loudly rather than saturating to the server maximum.
#[expect(
    unused_variables,
    reason = "timeout configuration body awaits implementation"
)]
async fn configure_writer_timeouts(
    tx: &tokio_postgres::Transaction<'_>,
    lock_timeout: std::time::Duration,
    idle_transaction_timeout: std::time::Duration,
) -> Result<(), tokio_postgres::Error> {
    todo!("set both local GUCs in one command")
}

/// One category of streams in PostgreSQL, addressed by a typed id.
pub struct PgEventStreams<Id, E> {
    pool: PgPool,
    category: String,
    lock_timeout: std::time::Duration,
    idle_transaction_timeout: std::time::Duration,
    _marker: PhantomData<fn() -> (Id, E)>,
}

impl<Id, E> PgEventStreams<Id, E> {
    /// `category` is this repository's namespace; ids render only their
    /// own identity into it (ADR 0004).
    pub fn new(pool: PgPool, category: &str) -> Self {
        Self {
            pool,
            category: category.to_owned(),
            lock_timeout: DEFAULT_LOCK_TIMEOUT,
            idle_transaction_timeout: DEFAULT_IDLE_TRANSACTION_TIMEOUT,
            _marker: PhantomData,
        }
    }

    /// Set the funnel wait bound. A wedged writer surfaces as the
    /// retryable [`AppendError::LockTimeout`] when it expires, never
    /// a hang and never a conflict (ADR 0010).
    pub fn with_lock_timeout(self, lock_timeout: std::time::Duration) -> Self {
        Self {
            lock_timeout,
            ..self
        }
    }

    /// Set the idle-in-transaction bound: how long an event
    /// transaction may sit without an active statement before the
    /// server terminates it, evicting a wedged writer.
    pub fn with_idle_transaction_timeout(
        self,
        idle_transaction_timeout: std::time::Duration,
    ) -> Self {
        Self {
            idle_transaction_timeout,
            ..self
        }
    }

    /// Apply pending schema migrations in order. Idempotent and safe
    /// under concurrent callers: the whole run is one transaction
    /// behind a transaction-scoped advisory lock, and later callers
    /// find nothing pending.
    pub async fn migrate(&self) -> Result<(), PgStreamsError> {
        let mut conn = self.pool.get().await?;
        migrations::apply(&mut conn).await
    }

    async fn category_rows(&self) -> Result<Vec<Row>, PgStreamsError> {
        let conn = self.pool.get().await?;
        Ok(conn
            .query(
                "SELECT stream_key, event_data FROM stream_events \
                 WHERE category = $1 \
                 ORDER BY global_sequence ASC",
                &[&self.category],
            )
            .await?)
    }
}

/// Shares the store (ADR 0005); never forks it. Hand-written so the
/// derive's spurious `Id: Clone, E: Clone` bounds are not required of
/// callers.
impl<Id, E> Clone for PgEventStreams<Id, E> {
    fn clone(&self) -> Self {
        Self {
            pool: self.pool.clone(),
            category: self.category.clone(),
            lock_timeout: self.lock_timeout,
            idle_transaction_timeout: self.idle_transaction_timeout,
            _marker: PhantomData,
        }
    }
}

/// Build a pool from a postgres connection string, e.g.
/// `postgres://user:pass@host:5432/dbname`.
pub async fn pool_from_conn_str(conn_str: &str) -> Result<PgPool, PgStreamsError> {
    let config: tokio_postgres::Config = conn_str
        .parse()
        .map_err(|error: tokio_postgres::Error| PgStreamsError::Config(error.to_string()))?;
    Ok(Pool::builder()
        .build(PostgresConnectionManager::new(config, NoTls))
        .await?)
}

/// Backend failure surface wrapped by [`AppendError::Backend`] and
/// [`LoadError::Backend`].
#[derive(Debug, Error)]
pub enum PgStreamsError {
    #[error("connection error: {0}")]
    Connection(#[from] tokio_postgres::Error),

    #[error("connection pool error: {0}")]
    Pool(#[from] bb8::RunError<tokio_postgres::Error>),

    #[error("event payload serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// A server-side lock wait expired: the bounded operation made no
    /// progress and may be retried (the feed-side counterpart of
    /// [`AppendError::LockTimeout`]).
    #[error("lock wait timed out after {0:?}")]
    LockTimeout(std::time::Duration),

    #[error("invalid connection config: {0}")]
    Config(String),
}

/// Lock-timeout expiry inside an append transaction is a distinct
/// retryable outcome, never a generic backend fault (ADR 0010's
/// funnel contract, the append-side twin of the batch path's
/// statement classifier). Every statement after the funnel
/// acquisition routes its failure through here so an expiry is
/// classified the same wherever it arises - the funnel lock itself,
/// the head read, the inserts, and the commit can all wait on locks.
fn append_statement_error(
    error: tokio_postgres::Error,
    lock_timeout: std::time::Duration,
) -> AppendError<PgStreamsError> {
    if error.code() == Some(&tokio_postgres::error::SqlState::LOCK_NOT_AVAILABLE) {
        AppendError::LockTimeout(lock_timeout)
    } else {
        AppendError::Backend(PgStreamsError::Connection(error))
    }
}

/// A stream's highest stored position as the observed version: no rows
/// is `NoStream`, otherwise sequence N.
fn version_of(position: u64) -> StreamVersion {
    match StreamSequence::new(position) {
        Ok(sequence) => StreamVersion::Exact(sequence),
        Err(super::ZeroSequence) => StreamVersion::NoStream,
    }
}

/// Stored positions are written from a 1-based counter, so this is a
/// storage-corruption assertion rather than a runtime branch.
fn stored_position(raw: i64) -> u64 {
    u64::try_from(raw).expect("stored sequences are non-negative")
}

fn decode_events<'a, E>(
    rows: impl IntoIterator<Item = &'a Row>,
) -> Result<Vec<E>, serde_json::Error>
where
    E: DeserializeOwned,
{
    rows.into_iter()
        .map(|row| serde_json::from_value(row.get("event_data")))
        .collect()
}

impl<Id, E> EventStreams<E> for PgEventStreams<Id, E>
where
    Id: StreamId,
    E: Event + Serialize + DeserializeOwned + Send + Sync + Debug,
{
    type Id = Id;
    type Error = PgStreamsError;

    async fn load_stream(&self, id: &Self::Id) -> Result<StreamState<E>, Self::Error> {
        let key = id.stream_key();
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT event_data FROM stream_events \
                 WHERE category = $1 AND stream_key = $2 \
                 ORDER BY sequence ASC",
                &[&self.category, &key],
            )
            .await?;

        let events = decode_events(&rows)?;
        if events.is_empty() {
            Ok(StreamState::Missing)
        } else {
            // The nonempty invariant was just checked above, so the
            // empty-batch error is unreachable.
            Ok(StreamState::Present(
                EventBatch::new(events).expect("events were checked nonempty"),
            ))
        }
    }

    async fn load_stream_from(
        &self,
        id: &Self::Id,
        from: Option<StreamSequence>,
    ) -> Result<StreamSlice<E>, Self::Error> {
        let key = id.stream_key();
        // `from` is 1-based and inclusive, and the decisive comparison
        // is in `u64`: a cursor past the tail - any tail, including one
        // beyond what postgres can store - reads nothing.
        let cursor = from.map_or(1, StreamSequence::get);
        let lower_bound = i64::try_from(cursor).unwrap_or(i64::MAX);

        // One statement, so the head and the joined rows derive from one
        // snapshot: a rival append committing mid-read cannot produce a
        // slice whose events disagree with its observation. Only the
        // in-range rows cross the wire; the outer join carries the head
        // on a row with null event columns when nothing is in range.
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT s.head, e.sequence, e.event_data \
                 FROM (SELECT COALESCE(MAX(sequence), 0) AS head \
                         FROM stream_events \
                        WHERE category = $1 AND stream_key = $2) s \
                 LEFT JOIN stream_events e \
                   ON e.category = $1 AND e.stream_key = $2 AND e.sequence >= $3 \
                 ORDER BY e.sequence ASC",
                &[&self.category, &key, &lower_bound],
            )
            .await?;

        let head: i64 = rows
            .first()
            .expect("the outer join always reports the head")
            .get("head");
        let in_range = rows.iter().filter(|row| {
            row.get::<_, Option<i64>>("sequence")
                .is_some_and(|sequence| stored_position(sequence) >= cursor)
        });

        // The pair is consistent by construction: rows only exist for a
        // stream whose observation is `Exact`, so the misshapen-slice
        // error is unreachable.
        Ok(
            StreamSlice::new(decode_events(in_range)?, version_of(stored_position(head)))
                .expect("in-range rows imply an Exact observation"),
        )
    }

    async fn load_category(
        &self,
    ) -> Result<
        Vec<CategoryEvent<Self::Id, E>>,
        LoadError<Self::Error, <Self::Id as StreamId>::ParseError>,
    > {
        let rows = self.category_rows().await.map_err(LoadError::Backend)?;
        rows.iter()
            .map(|row| {
                let id =
                    Self::Id::parse_key(row.get("stream_key")).map_err(LoadError::InvalidKey)?;
                let event = serde_json::from_value(row.get("event_data"))
                    .map_err(PgStreamsError::Serialization)
                    .map_err(LoadError::Backend)?;
                Ok(CategoryEvent { id, event })
            })
            .collect()
    }

    async fn append(
        &self,
        expected: ExpectedVersion,
        stream: &Self::Id,
        events: &EventBatch<E>,
    ) -> Result<StreamSequence, AppendError<Self::Error>> {
        let key = stream.stream_key();
        let mut conn = self.pool.get().await.map_err(PgStreamsError::Pool)?;
        let tx = conn
            .transaction()
            .await
            .map_err(PgStreamsError::Connection)?;

        configure_writer_timeouts(&tx, self.lock_timeout, self.idle_transaction_timeout)
            .await
            .map_err(PgStreamsError::Connection)?;

        // The funnel: exactly one event transaction runs at a time,
        // so the head read below observes a store no rival can be
        // concurrently appending to.
        tx.execute("SELECT pg_advisory_xact_lock($1)", &[&WRITER_LOCK])
            .await
            .map_err(|error| append_statement_error(error, self.lock_timeout))?;

        let position: i64 = tx
            .query_one(
                "SELECT COALESCE(MAX(sequence), 0) AS position FROM stream_events \
                 WHERE category = $1 AND stream_key = $2",
                &[&self.category, &key],
            )
            .await
            .map_err(|error| append_statement_error(error, self.lock_timeout))?
            .get("position");
        let observed = version_of(stored_position(position));

        let satisfied = match expected {
            ExpectedVersion::Any => true,
            ExpectedVersion::NoStream => matches!(observed, StreamVersion::NoStream),
            ExpectedVersion::StreamExists => matches!(observed, StreamVersion::Exact(_)),
            ExpectedVersion::Exact(sequence) => observed == StreamVersion::Exact(sequence),
        };
        if !satisfied {
            // The pair is a genuine conflict because it is only built
            // when the check just failed, so the not-a-conflict error
            // is unreachable.
            return Err(AppendError::Conflict(
                VersionConflict::new(expected, observed)
                    .expect("the expectation just failed against the observation"),
            ));
        }

        let mut next = position;
        for record in events.records() {
            next += 1;
            let event_type = record.event().event_type();
            let event_data =
                serde_json::to_value(record.event()).map_err(PgStreamsError::Serialization)?;
            let event_metadata =
                serde_json::to_value(record.metadata()).map_err(PgStreamsError::Serialization)?;
            tx.execute(
                "INSERT INTO stream_events \
                 (category, stream_key, event_type, sequence, event_data, event_metadata) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
                &[
                    &self.category,
                    &key,
                    &event_type,
                    &next,
                    &event_data,
                    &event_metadata,
                ],
            )
            .await
            .map_err(|error| append_statement_error(error, self.lock_timeout))?;
        }

        tx.commit()
            .await
            .map_err(|error| append_statement_error(error, self.lock_timeout))?;
        Ok(StreamSequence::new(stored_position(next)).expect("a batch holds at least one event"))
    }
}

#[cfg(all(test, feature = "postgres"))]
mod tests {
    use serde::Deserialize;

    use super::*;
    use crate::streams::spec::{
        flash_sale_sells_exactly_the_stock, single_event_occ_race_on_empty_stream,
        single_event_occ_race_on_seeded_stream, under_deadline,
    };
    use crate::streams::{EventMetadata, RecordedEvent};

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct SomethingHappened;

    impl Event for SomethingHappened {
        type EntityId = ();

        fn event_type(&self) -> String {
            "SomethingHappened".to_owned()
        }

        fn get_id(&self) -> Self::EntityId {}
    }

    /// A migrated store over the compose postgres. Spec stream ids carry
    /// ULID nonces, so runs never collide and storage is never cleared.
    async fn store(category: &str) -> PgEventStreams<String, SomethingHappened> {
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
        let store = PgEventStreams::new(pool, category);
        store.migrate().await.expect("schema migrates");
        store
    }

    /// A writer and a feed over one fresh category, per the feed
    /// suite's fresh-store assumption: count assertions read the
    /// whole category tail.
    async fn writer_and_feed() -> (
        PgEventStreams<String, SomethingHappened>,
        PgEventFeed<SomethingHappened>,
    ) {
        let category = format!("feed-spec-{}", rusty_ulid::generate_ulid_string());
        let _ = dotenv::dotenv();
        let conn_str = std::env::var("EPOCH_PG_TEST_URL").expect("EPOCH_PG_TEST_URL must be set");
        let pool = pool_from_conn_str(&conn_str)
            .await
            .expect("pg pool from EPOCH_PG_TEST_URL");
        let store = PgEventStreams::new(pool.clone(), &category);
        store.migrate().await.expect("schema migrates");
        (store, PgEventFeed::new(pool, &category))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn postgres_poll_redelivers_until_acked() {
        let (writer, feed) = writer_and_feed().await;
        crate::streams::feed::spec::poll_redelivers_until_acked(
            writer,
            feed,
            str::to_owned,
            || SomethingHappened,
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn postgres_poll_pages_the_backlog() {
        let (writer, feed) = writer_and_feed().await;
        crate::streams::feed::spec::poll_pages_the_backlog(writer, feed, str::to_owned, || {
            SomethingHappened
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn postgres_ack_is_monotonic() {
        let (writer, feed) = writer_and_feed().await;
        crate::streams::feed::spec::ack_is_monotonic(writer, feed, str::to_owned, || {
            SomethingHappened
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn postgres_ack_rejects_undelivered() {
        let (writer, feed) = writer_and_feed().await;
        crate::streams::feed::spec::ack_rejects_undelivered(writer, feed, str::to_owned, || {
            SomethingHappened
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn postgres_delivery_carries_the_envelope() {
        let (writer, feed) = writer_and_feed().await;
        crate::streams::feed::spec::delivery_carries_the_envelope(
            writer,
            feed,
            str::to_owned,
            || SomethingHappened,
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn postgres_single_event_occ_race_on_empty_stream() {
        let store = store("spec-occ-empty").await;
        under_deadline(single_event_occ_race_on_empty_stream(
            store,
            str::to_owned,
            || SomethingHappened,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn postgres_single_event_occ_race_on_seeded_stream() {
        let store = store("spec-occ-seeded").await;
        under_deadline(single_event_occ_race_on_seeded_stream(
            store,
            str::to_owned,
            || SomethingHappened,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn postgres_flash_sale_sells_exactly_the_stock() {
        let store = store("spec-flash-sale").await;
        under_deadline(flash_sale_sells_exactly_the_stock(
            store,
            str::to_owned,
            || SomethingHappened,
        ))
        .await;
    }

    /// A wedged writer holding the funnel is a retryable timeout on
    /// the append path, never a hang and never a conflict: a second
    /// connection holds WRITER_LOCK in an open transaction, and the
    /// append's bounded wait expires against it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_wedged_funnel_is_a_retryable_append_timeout() {
        under_deadline(async {
            let _ = dotenv::dotenv();
            let conn_str =
                std::env::var("EPOCH_PG_TEST_URL").expect("EPOCH_PG_TEST_URL must be set");
            let pool = pool_from_conn_str(&conn_str)
                .await
                .expect("pg pool from EPOCH_PG_TEST_URL");
            let store =
                PgEventStreams::<String, SomethingHappened>::new(pool.clone(), "spec-funnel")
                    .with_lock_timeout(std::time::Duration::from_millis(100));
            store.migrate().await.expect("schema migrates");
            let key = format!("k-{}", rusty_ulid::generate_ulid_string());

            let mut holder = pool.get().await.expect("a second connection");
            let held = holder.transaction().await.expect("holder transaction");
            held.execute("SELECT pg_advisory_xact_lock($1)", &[&WRITER_LOCK])
                .await
                .expect("the holder takes the funnel lock");

            let bound = std::time::Duration::from_millis(100);
            let outcome = store
                .append(
                    ExpectedVersion::NoStream,
                    &key,
                    &EventBatch::new(vec![SomethingHappened]).expect("one event is nonempty"),
                )
                .await;
            assert!(
                matches!(outcome, Err(AppendError::LockTimeout(reported)) if reported == bound),
                "expected a retryable funnel timeout, got {outcome:?}"
            );

            held.rollback().await.expect("the holder releases");
            let state = store.load_stream(&key).await.expect("load succeeds");
            assert_eq!(
                state,
                StreamState::Missing,
                "a timed-out append stores nothing"
            );
        })
        .await;
    }

    /// The funnel's discriminating proof (pre-Gate-U review round,
    /// finding C1): a holder inside WRITER_LOCK with an uncommitted
    /// insert blocks a rival store append - the rival is observed
    /// waiting on the advisory lock in pg_locks, not merely
    /// unscheduled - and a poll in that window delivers nothing the
    /// holder or the rival wrote; after the holder rolls back, the
    /// rival commits above the holder's burned value and one poll
    /// delivers it. Under the pre-pivot per-stream locks the rival
    /// would not have waited on the holder at all - the wait IS the
    /// funnel's observable shadow.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_funnel_holder_blocks_a_rival_append_until_release() {
        use crate::streams::feed::{ConsumerGroup, EventFeed, PollLimit};

        under_deadline(async {
            let _ = dotenv::dotenv();
            let conn_str =
                std::env::var("EPOCH_PG_TEST_URL").expect("EPOCH_PG_TEST_URL must be set");
            let pool = pool_from_conn_str(&conn_str)
                .await
                .expect("pg pool from EPOCH_PG_TEST_URL");
            let category = format!("feed-funnel-{}", rusty_ulid::generate_ulid_string());
            let store = PgEventStreams::<String, SomethingHappened>::new(pool.clone(), &category);
            store.migrate().await.expect("schema migrates");
            let feed = PgEventFeed::<SomethingHappened>::new(pool.clone(), &category);
            let group = ConsumerGroup::new("funnel-testers").expect("non-empty group");

            let seed_key = format!("seed-{}", rusty_ulid::generate_ulid_string());
            store
                .append(
                    ExpectedVersion::NoStream,
                    &seed_key,
                    &EventBatch::new(vec![SomethingHappened]).expect("one event is nonempty"),
                )
                .await
                .expect("seed append");

            // The holder: the funnel lock plus one uncommitted
            // insert; its drawn sequence value burns at rollback.
            let mut holder_conn = pool.get().await.expect("a second connection");
            let held = holder_conn.transaction().await.expect("holder transaction");
            held.execute("SELECT pg_advisory_xact_lock($1)", &[&WRITER_LOCK])
                .await
                .expect("the holder takes the funnel");
            held.execute(
                "INSERT INTO stream_events \
                 (category, stream_key, event_type, sequence, event_data, event_metadata) \
                 VALUES ($1, $2, 'Held', 1, '\"{}\"'::jsonb, '{}'::jsonb)",
                &[
                    &category,
                    &format!("held-{}", rusty_ulid::generate_ulid_string()),
                ],
            )
            .await
            .expect("the uncommitted insert");
            // The value the holder drew and will burn at rollback,
            // read inside its own session (CURRVAL is session-local).
            let burned: i64 = held
                .query_one(
                    "SELECT CURRVAL('stream_events_global_sequence_seq') AS drawn",
                    &[],
                )
                .await
                .expect("read the holder's drawn value")
                .get("drawn");

            // A rival store append through the funnel must block on
            // the holder.
            let rival = {
                let store = store.clone();
                let key = format!("rival-{}", rusty_ulid::generate_ulid_string());
                tokio::spawn(async move {
                    store
                        .append(
                            ExpectedVersion::NoStream,
                            &key,
                            &EventBatch::new(vec![SomethingHappened])
                                .expect("one event is nonempty"),
                        )
                        .await
                })
            };

            // Establish that a writer actually waits on the funnel:
            // an ungranted advisory-lock row for WRITER_LOCK in
            // pg_locks. Our holder is the lock's only granted owner,
            // so any waiter queues behind it; under the pre-pivot
            // implementation no session takes this key at all, so the
            // probe cannot fire - which is what makes it
            // discriminating rather than decorative (the 300ms sleep
            // it replaces could only guess). Bounded: two seconds,
            // then the test fails.
            // pg_locks stores the halves as oid (unsigned), hence u32.
            let classid = u32::try_from(WRITER_LOCK >> 32).expect("the lock key's high half");
            let objid = u32::try_from(WRITER_LOCK & 0xffff_ffff).expect("the lock key's low half");
            let mut observed_wait = false;
            for _ in 0..40 {
                let waiting: bool = pool
                    .get()
                    .await
                    .expect("a monitor connection")
                    .query_one(
                        "SELECT EXISTS(\
                         SELECT 1 FROM pg_locks \
                         WHERE locktype = 'advisory' AND classid = $1 AND objid = $2 \
                         AND NOT granted)",
                        &[&classid, &objid],
                    )
                    .await
                    .expect("pg_locks probe")
                    .get(0);
                if waiting {
                    observed_wait = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            assert!(
                observed_wait,
                "no writer ever waited on the holder's funnel lock"
            );
            assert!(!rival.is_finished(), "the rival append waits on the funnel");

            // A poll in the window delivers only the seed: the
            // holder's row is uncommitted and invisible, the rival's
            // does not exist yet.
            let windowed = feed
                .poll(&group, PollLimit::new(10).expect("nonzero limit"))
                .await
                .expect("poll inside the holder's window");
            assert_eq!(windowed.len(), 1, "only the seed is visible in the window");
            let tip = windowed.last().expect("the seed entry").position();
            feed.ack(&group, tip).await.expect("ack the seed tip");

            held.rollback().await.expect("the holder releases");
            rival
                .await
                .expect("rival task")
                .expect("the rival commits once the funnel frees");

            // The rival's event delivers above the acked tip, and
            // above the value the holder burned: insert order is
            // commit order, and the burn never appears.
            let settled = feed
                .poll(&group, PollLimit::new(10).expect("nonzero limit"))
                .await
                .expect("poll after the release");
            assert_eq!(
                settled.len(),
                1,
                "the rival's event delivers after the release"
            );
            assert!(
                settled[0].position() > tip,
                "the rival committed above the acked tip"
            );
            let burned = u64::try_from(burned).expect("a sequence value is non-negative");
            assert!(
                settled[0].position().get() > burned,
                "the rival committed above the holder's burned value"
            );
        })
        .await;
    }

    /// The envelope a keyed append carries is the envelope the row
    /// stores; a bare append stores the empty object (ADR 0010's seam).
    #[tokio::test(flavor = "multi_thread")]
    async fn postgres_the_envelope_written_is_the_envelope_stored() {
        let store = store("spec-envelope").await;
        let key = format!("stream-{}", rusty_ulid::generate_ulid_string());

        let mut envelope = EventMetadata::new();
        envelope.insert("intent", "saga-1/order-5/2");
        let batch = EventBatch::from_records(vec![
            RecordedEvent::new(SomethingHappened),
            RecordedEvent::keyed(SomethingHappened, envelope),
        ])
        .expect("two events are nonempty");
        store
            .append(ExpectedVersion::NoStream, &key, &batch)
            .await
            .expect("append succeeds");

        let conn = store.pool.get().await.expect("assertion connection");
        let rows = conn
            .query(
                "SELECT event_metadata FROM stream_events \
                 WHERE category = $1 AND stream_key = $2 ORDER BY sequence",
                &[&"spec-envelope", &key],
            )
            .await
            .expect("envelope read");
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].get::<_, serde_json::Value>("event_metadata"),
            serde_json::json!({}),
            "a bare event stores the empty envelope"
        );
        assert_eq!(
            rows[1].get::<_, serde_json::Value>("event_metadata"),
            serde_json::json!({"intent": "saga-1/order-5/2"}),
            "a keyed event stores its envelope"
        );
    }
}
