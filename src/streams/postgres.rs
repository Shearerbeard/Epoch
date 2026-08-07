//! PostgreSQL [`EventStreams`] backend on the unified 1-based sequence
//! semantics (ADR 0003) and the shared-store `Clone` contract
//! (ADR 0005): clones share one pool and one category, so concurrent
//! writers contend on the same stored streams.
//!
//! Append atomicity. One transaction takes a per-stream
//! `pg_advisory_xact_lock`, then reads the stream's position, then
//! writes. Rivals on one stream serialize on that lock, and the position
//! read is a statement that cannot start until the lock is held: under
//! READ COMMITTED it therefore takes its snapshot after the winner
//! committed and observes the winner's rows, so the loser conflicts
//! instead of overwriting. The lock is transaction-scoped, so a
//! cancelled future releases it at rollback rather than leaking a
//! session-level lock. The unique position constraint in `schema.sql`
//! backstops the same invariant at the storage layer, alongside a check
//! constraint that keeps positions 1-based.
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

const SCHEMA: &str = include_str!("postgres/schema.sql");

/// The connection pool every store in this module runs on.
pub type PgPool = Pool<PostgresConnectionManager<NoTls>>;

/// One category of streams in PostgreSQL, addressed by a typed id.
pub struct PgEventStreams<Id, E> {
    pool: PgPool,
    category: String,
    _marker: PhantomData<fn() -> (Id, E)>,
}

impl<Id, E> PgEventStreams<Id, E> {
    /// `category` is this repository's namespace; ids render only their
    /// own identity into it (ADR 0004).
    pub fn new(pool: PgPool, category: &str) -> Self {
        Self {
            pool,
            category: category.to_owned(),
            _marker: PhantomData,
        }
    }

    /// Apply `schema.sql`. Idempotent and safe under concurrent callers:
    /// a transaction-scoped advisory lock serializes the CREATE
    /// statements and releases automatically at commit.
    pub async fn migrate(&self) -> Result<(), PgStreamsError> {
        const MIGRATION_LOCK: i64 = 0x6570_6f63_685f_7374; // "epoch_st"
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        tx.execute("SELECT pg_advisory_xact_lock($1)", &[&MIGRATION_LOCK])
            .await?;
        tx.batch_execute(SCHEMA).await?;
        tx.commit().await?;
        Ok(())
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

    #[error("invalid connection config: {0}")]
    Config(String),
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
            // `EventBatch::new` is an E1 hole outside this card's fill
            // bound; crate-internal construction is valid here because
            // the nonempty invariant was just checked.
            Ok(StreamState::Present(EventBatch(events)))
        }
    }

    async fn load_stream_from(
        &self,
        id: &Self::Id,
        from: Option<StreamSequence>,
    ) -> Result<StreamSlice<E>, Self::Error> {
        let key = id.stream_key();

        // One statement, so the observed position and the events in
        // range derive from one snapshot: a rival append committing
        // mid-read cannot produce a slice whose events disagree with
        // its observation.
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT sequence, event_data FROM stream_events \
                 WHERE category = $1 AND stream_key = $2 \
                 ORDER BY sequence ASC",
                &[&self.category, &key],
            )
            .await?;

        let at = rows.last().map_or(StreamVersion::NoStream, |row| {
            version_of(stored_position(row.get("sequence")))
        });
        // `from` is 1-based and inclusive, and the comparison is in
        // `u64`: a cursor past the tail - any tail, including one beyond
        // what postgres can store - reads nothing.
        let cursor = from.map_or(1, StreamSequence::get);
        let in_range = rows
            .iter()
            .filter(|row| stored_position(row.get("sequence")) >= cursor);

        // `StreamSlice::new` is an E1 hole outside this card's fill
        // bound; the pair is consistent by construction, since rows only
        // exist for a stream whose observation is `Exact`.
        Ok(StreamSlice {
            events: decode_events(in_range)?,
            at,
        })
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

        // Serialize this stream's writers for the life of the
        // transaction; see the module docs for why this rules out the
        // lost update.
        tx.execute(
            "SELECT pg_advisory_xact_lock(hashtext($1), hashtext($2))",
            &[&self.category, &key],
        )
        .await
        .map_err(PgStreamsError::Connection)?;

        let position: i64 = tx
            .query_one(
                "SELECT COALESCE(MAX(sequence), 0) AS position FROM stream_events \
                 WHERE category = $1 AND stream_key = $2",
                &[&self.category, &key],
            )
            .await
            .map_err(PgStreamsError::Connection)?
            .get("position");
        let observed = version_of(stored_position(position));

        let satisfied = match expected {
            ExpectedVersion::Any => true,
            ExpectedVersion::NoStream => matches!(observed, StreamVersion::NoStream),
            ExpectedVersion::StreamExists => matches!(observed, StreamVersion::Exact(_)),
            ExpectedVersion::Exact(sequence) => observed == StreamVersion::Exact(sequence),
        };
        if !satisfied {
            // `VersionConflict::new` is an E1 hole outside this card's
            // fill bound; the pair is a genuine conflict because it is
            // only built when the check just failed.
            return Err(AppendError::Conflict(VersionConflict {
                expected,
                actual: observed,
            }));
        }

        // `EventBatch::as_slice` is an E1 hole outside this card's fill
        // bound; the batch's own invariant makes this nonempty.
        let mut next = position;
        for event in &events.0 {
            next += 1;
            let event_type = event.event_type();
            let event_data = serde_json::to_value(event).map_err(PgStreamsError::Serialization)?;
            tx.execute(
                "INSERT INTO stream_events \
                 (category, stream_key, event_type, sequence, event_data) \
                 VALUES ($1, $2, $3, $4, $5)",
                &[&self.category, &key, &event_type, &next, &event_data],
            )
            .await
            .map_err(PgStreamsError::Connection)?;
        }

        tx.commit().await.map_err(PgStreamsError::Connection)?;
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
}
