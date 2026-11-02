//! SQLite [`EventStreams`] backend on the unified 1-based sequence
//! semantics (ADR 0003) and the shared-store `Clone` contract
//! (ADR 0005): clones share one actor and one database, so concurrent
//! writers contend on the same stored streams.
//!
//! The single-writer funnel ADR 0010 requires is structural here, not
//! conventional. SQLite serializes writers database-wide, and every
//! write path opens its transaction with `BEGIN IMMEDIATE` - the write
//! lock is taken before the transaction's first read - so exactly one
//! event transaction is ever in flight. Insert order is therefore
//! commit order by construction, which is the property the feed's
//! plain-maximum cursor rests on. The lock is coarser than
//! PostgreSQL's single global advisory key: database-wide rather than
//! key-scoped, never weaker, with no lock order to get wrong and no
//! deadlock cycle to form.
//!
//! Every write path opens with `BEGIN IMMEDIATE`, never a deferred
//! `BEGIN`. A deferred transaction that reads and then writes fails
//! with `database is locked` when another connection committed in
//! between, so the read-then-write shape is not safe here. The
//! read-only paths need no transaction at all: each load is a single
//! statement, and a statement is its own snapshot, so a rival append
//! committing mid-read cannot split a read across two views.

use std::fmt::Debug;
use std::marker::PhantomData;
use std::time::Duration;

use serde::{de::DeserializeOwned, Serialize};
use thiserror::Error;
use tokio_rusqlite::{rusqlite, Connection};

use crate::decider::Event;

use super::{
    AppendError, CategoryEvent, EventBatch, EventStreams, ExpectedVersion, LoadError,
    RecordedEvent, StreamId, StreamSequence, StreamSlice, StreamState, StreamVersion, ZeroSequence,
};

use super::{batch::expectation_satisfied, EventMetadata, VersionConflict};

pub mod batch;
pub mod feed;
mod migrations;

#[cfg(test)]
mod tests;

pub use batch::{
    SqliteBatch, SqliteBatchBuilder, SqliteDatabase, SqliteEncodedEvent, SqliteWriteError,
};
pub use feed::SqliteEventFeed;

/// The default writer wait bound for every write path: the single
/// append, the atomic batch, and the feed's poll and ack. Long enough
/// that a writer queued behind ordinary work still commits, short
/// enough that a wedged holder surfaces as a retryable timeout rather
/// than a hang.
pub(crate) const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// A `Duration` as whole milliseconds for the busy timeout. The value
/// is a count, so there is nothing a caller can inject; the floor of
/// 1ms keeps zero unsettable - SQLite reads a zero timeout as "fail
/// immediately" - and the retryable payload then names a bound that
/// was really in effect. The ceiling is the driver's own: it takes the
/// timeout as a 32-bit millisecond count, so a longer bound could not
/// be installed at all.
pub(crate) fn lock_timeout_ms(timeout: Duration) -> u64 {
    u64::try_from(timeout.as_millis())
        .unwrap_or(u64::MAX)
        .clamp(1, i32::MAX as u64)
}

/// Install the effective busy timeout on the connection. The timeout
/// is a property of the connection, not of the operation, and cloned
/// handles share one connection - so it is installed inside each
/// operation's own closure, immediately before that operation takes
/// the write lock, and never once at connect. The bound in force is
/// then always the one the operation intends.
pub(crate) fn install_busy_timeout(
    conn: &rusqlite::Connection,
    bound: Duration,
) -> Result<(), rusqlite::Error> {
    conn.busy_timeout(Duration::from_millis(lock_timeout_ms(bound)))
}

/// Whether a driver failure is a lock wait that ran out of time rather
/// than a fault. SQLite's `SQLITE_BUSY` and `SQLITE_LOCKED` are the
/// two codes a busy timeout produces, and both are retryable.
pub(crate) fn is_lock_timeout(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if matches!(
                failure.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

/// Backend failure surface wrapped by [`AppendError::Backend`] and
/// [`LoadError::Backend`].
#[derive(Debug, Error)]
pub enum SqliteStreamsError {
    /// The driver failed.
    #[error("connection error: {0}")]
    Connection(#[from] rusqlite::Error),

    /// The actor handle failed: the connection was closed.
    #[error("connection handle error: {0}")]
    Handle(#[from] tokio_rusqlite::Error),

    /// An event payload or envelope did not serialize.
    #[error("event payload serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// The write lock could not be taken inside the operation's wait
    /// bound; retryable, never a version conflict (ADR 0010).
    #[error("lock wait timed out after {0:?}")]
    LockTimeout(Duration),
}

/// Flatten the actor's error envelope: a unit-of-work failure passes
/// through, and an actor-lifecycle failure surfaces as a handle
/// error. Driver failures under the unit of work are already
/// classified by the operation that produced them.
fn actor_outcome<T>(
    outcome: Result<T, tokio_rusqlite::Error<SqliteStreamsError>>,
) -> Result<T, SqliteStreamsError> {
    match outcome {
        Ok(value) => Ok(value),
        Err(tokio_rusqlite::Error::Error(inner)) => Err(inner),
        Err(tokio_rusqlite::Error::Close((_, error))) => Err(SqliteStreamsError::Connection(error)),
        Err(_) => Err(SqliteStreamsError::Handle(
            tokio_rusqlite::Error::ConnectionClosed,
        )),
    }
}

/// Classify a driver failure: a lock wait that ran out of time is the
/// retryable outcome, and every other failure is a backend fault. The
/// payload names the effective bound, so a retry policy sizing its
/// backoff from it never reads a bound that was never in effect.
pub(crate) fn classify_lock_failure(error: rusqlite::Error, bound: Duration) -> SqliteStreamsError {
    if is_lock_timeout(&error) {
        SqliteStreamsError::LockTimeout(Duration::from_millis(lock_timeout_ms(bound)))
    } else {
        SqliteStreamsError::Connection(error)
    }
}

/// A stream's highest stored position as the observed version: no rows
/// is `NoStream`, otherwise sequence N.
fn version_of(position: u64) -> StreamVersion {
    match StreamSequence::new(position) {
        Ok(sequence) => StreamVersion::Exact(sequence),
        Err(ZeroSequence) => StreamVersion::NoStream,
    }
}

/// Stored positions are written from a 1-based counter, so this is a
/// storage-corruption assertion rather than a runtime branch.
fn stored_position(raw: i64) -> u64 {
    u64::try_from(raw).expect("stored sequences are non-negative")
}

/// Rows to records, each with the envelope stored beside it.
///
/// The actor hands back owned payload and envelope strings rather than
/// rows: a rusqlite `Row` borrows the statement that produced it, so
/// it cannot leave the actor's closure. Decoding happens on the
/// caller's side of that boundary, which also keeps the event type
/// off the closure's `'static` bound.
fn decode_records<E>(
    rows: impl IntoIterator<Item = (String, String)>,
) -> Result<Vec<RecordedEvent<E>>, SqliteStreamsError>
where
    E: DeserializeOwned,
{
    rows.into_iter()
        .map(|(payload, envelope)| {
            let event = serde_json::from_str(&payload)?;
            let metadata: EventMetadata = serde_json::from_str(&envelope)?;
            Ok(RecordedEvent::keyed(event, metadata))
        })
        .collect()
}

/// One category of streams in a SQLite database, addressed by a typed
/// id.
pub struct SqliteEventStreams<Id, E> {
    connection: Connection,
    category: String,
    lock_timeout: Duration,
    _marker: PhantomData<fn() -> (Id, E)>,
}

impl<Id, E> SqliteEventStreams<Id, E> {
    /// `category` is this repository's namespace; ids render only their
    /// own identity into it (ADR 0004).
    pub fn new(connection: Connection, category: &str) -> Self {
        Self {
            connection,
            category: category.to_owned(),
            lock_timeout: DEFAULT_LOCK_TIMEOUT,
            _marker: PhantomData,
        }
    }

    /// Set the write-lock wait bound. A wedged writer surfaces as the
    /// retryable [`AppendError::LockTimeout`] when the wait expires,
    /// never a version conflict (ADR 0010).
    pub fn with_lock_timeout(self, lock_timeout: Duration) -> Self {
        Self {
            lock_timeout,
            ..self
        }
    }

    /// Open a file-backed database, e.g. `epoch.db`. Cross-process
    /// writers serialize on the busy timeout the migration installs.
    pub async fn open(path: &str, category: &str) -> Result<Self, SqliteStreamsError> {
        Ok(Self::new(Connection::open(path).await?, category))
    }

    /// Open a private in-memory database. Every store built on the
    /// returned handle - clones share it - sees the same database;
    /// nothing else can.
    pub async fn open_in_memory(category: &str) -> Result<Self, SqliteStreamsError> {
        Ok(Self::new(Connection::open_in_memory().await?, category))
    }

    /// Apply pending schema migrations in order. Idempotent: the whole
    /// pending set runs in one transaction, so a failing step leaves
    /// nothing applied and a second call finds nothing pending.
    ///
    /// Opening a store does not create the schema; this call does
    /// (E12's rule, carried onto this backend).
    pub async fn migrate(&self) -> Result<(), SqliteStreamsError> {
        let connection = self.connection.clone();
        let lock_timeout = self.lock_timeout;
        let outcome = connection
            .call(move |conn| {
                // The effective bound goes in before any statement
                // that can contend for the write lock, the WAL pragma
                // included: switching journal mode needs a brief
                // exclusive lock, and it must wait under this
                // operation's own bound rather than under whatever the
                // connection happened to carry.
                install_busy_timeout(conn, lock_timeout)
                    .map_err(|error| classify_lock_failure(error, lock_timeout))?;
                // WAL lets file-backed readers proceed beside the
                // writer; on a `:memory:` database SQLite reports
                // "memory" back, which this query consumes
                // harmlessly.
                conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get::<_, String>(0))
                    .map_err(|error| classify_lock_failure(error, lock_timeout))?;
                migrations::apply(conn, lock_timeout)
            })
            .await;
        actor_outcome(outcome)
    }
}

/// Shares the store (ADR 0005); never forks it. Hand-written so the
/// derive's spurious `Id: Clone, E: Clone` bounds are not required of
/// callers.
impl<Id, E> Clone for SqliteEventStreams<Id, E> {
    fn clone(&self) -> Self {
        Self {
            connection: self.connection.clone(),
            category: self.category.clone(),
            lock_timeout: self.lock_timeout,
            _marker: PhantomData,
        }
    }
}

impl<Id, E> EventStreams<E> for SqliteEventStreams<Id, E>
where
    Id: StreamId,
    E: Event + Serialize + DeserializeOwned + Send + Sync + Debug,
{
    type Id = Id;
    type Error = SqliteStreamsError;

    async fn load_stream(&self, id: &Self::Id) -> Result<StreamState<E>, Self::Error> {
        let key = id.stream_key();
        let category = self.category.clone();
        let connection = self.connection.clone();
        let outcome = connection
            .call(move |conn| {
                let mut statement = conn.prepare(
                    "SELECT event_data, event_metadata FROM stream_events \
                      WHERE category = ?1 AND stream_key = ?2 \
                      ORDER BY sequence ASC",
                )?;
                let rows = statement
                    .query_map(rusqlite::params![category, key], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<Result<Vec<_>, rusqlite::Error>>()?;
                Ok::<_, SqliteStreamsError>(rows)
            })
            .await;
        let records = decode_records(actor_outcome(outcome)?)?;
        if records.is_empty() {
            Ok(StreamState::Missing)
        } else {
            Ok(StreamState::Present(
                EventBatch::from_records(records).expect("records were checked nonempty"),
            ))
        }
    }

    async fn load_stream_from(
        &self,
        id: &Self::Id,
        from: Option<StreamSequence>,
    ) -> Result<StreamSlice<E>, Self::Error> {
        let key = id.stream_key();
        let category = self.category.clone();
        // `from` is 1-based and inclusive; the decisive comparison is in
        // `u64`, so a cursor past any tail - including one beyond what
        // SQLite can store - reads nothing.
        let cursor = from.map_or(1, StreamSequence::get);
        let lower_bound = i64::try_from(cursor).unwrap_or(i64::MAX);
        let connection = self.connection.clone();
        let outcome = connection
            .call(move |conn| {
                // One statement, so the head and the joined rows derive
                // from one snapshot. The outer join carries the head on a
                // row with null event columns when nothing is in range.
                let mut statement = conn.prepare(
                    "SELECT s.head, e.sequence, e.event_data, e.event_metadata \
                       FROM (SELECT COALESCE(MAX(sequence), 0) AS head \
                               FROM stream_events \
                              WHERE category = ?1 AND stream_key = ?2) s \
                       LEFT JOIN stream_events e \
                         ON e.category = ?1 AND e.stream_key = ?2 AND e.sequence >= ?3 \
                      ORDER BY e.sequence ASC",
                )?;
                let rows = statement
                    .query_map(rusqlite::params![category, key, lower_bound], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, Option<i64>>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, Option<String>>(3)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, rusqlite::Error>>()?;
                let head = rows.first().map_or(0, |row| row.0);
                let in_range = rows
                    .into_iter()
                    .filter_map(|(_, sequence, payload, envelope)| {
                        let sequence = sequence?;
                        (stored_position(sequence) >= cursor).then_some((
                            payload.expect("an in-range row carries its payload"),
                            envelope.expect("an in-range row carries its envelope"),
                        ))
                    })
                    .collect::<Vec<_>>();
                Ok::<_, SqliteStreamsError>((head, in_range))
            })
            .await;
        let (head, rows) = actor_outcome(outcome)?;
        Ok(
            StreamSlice::new(decode_records(rows)?, version_of(stored_position(head)))
                .expect("in-range rows imply an Exact observation"),
        )
    }

    async fn load_category(
        &self,
    ) -> Result<
        Vec<CategoryEvent<Self::Id, E>>,
        LoadError<Self::Error, <Self::Id as StreamId>::ParseError>,
    > {
        let category = self.category.clone();
        let connection = self.connection.clone();
        let outcome = connection
            .call(move |conn| {
                let mut statement = conn.prepare(
                    "SELECT stream_key, event_data FROM stream_events \
                      WHERE category = ?1 \
                      ORDER BY global_sequence ASC",
                )?;
                let rows = statement
                    .query_map(rusqlite::params![category], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<Result<Vec<_>, rusqlite::Error>>()?;
                Ok::<_, SqliteStreamsError>(rows)
            })
            .await;
        actor_outcome(outcome)
            .map_err(LoadError::Backend)?
            .into_iter()
            .map(|(key, payload)| {
                let id = Self::Id::parse_key(&key).map_err(LoadError::InvalidKey)?;
                let event = serde_json::from_str(&payload)
                    .map_err(SqliteStreamsError::Serialization)
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
        let category = self.category.clone();
        let lock_timeout = self.lock_timeout;

        // Encode before the actor boundary: a failed encoding is a
        // serialization fault, and the owned wire strings keep the event
        // type off the closure's `'static` bound.
        let encoded = events
            .records()
            .iter()
            .map(|record| {
                let event_type = record.event().event_type();
                let payload = serde_json::to_value(record.event())
                    .and_then(|value| serde_json::to_string(&value))
                    .map_err(SqliteStreamsError::Serialization)?;
                let envelope = serde_json::to_value(record.metadata())
                    .and_then(|value| serde_json::to_string(&value))
                    .map_err(SqliteStreamsError::Serialization)?;
                Ok((event_type, payload, envelope))
            })
            .collect::<Result<Vec<_>, SqliteStreamsError>>()
            .map_err(AppendError::Backend)?;

        let connection = self.connection.clone();
        let outcome = connection
            .call(move |conn| {
                install_busy_timeout(conn, lock_timeout)
                    .map_err(|error| classify_lock_failure(error, lock_timeout))?;
                let transaction = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(|error| classify_lock_failure(error, lock_timeout))?;

                let position: i64 = transaction
                    .query_row(
                        "SELECT COALESCE(MAX(sequence), 0) AS position FROM stream_events \
                          WHERE category = ?1 AND stream_key = ?2",
                        rusqlite::params![category, key],
                        |row| row.get(0),
                    )
                    .map_err(|error| classify_lock_failure(error, lock_timeout))?;
                let observed = version_of(stored_position(position));

                if !expectation_satisfied(expected, observed) {
                    return Ok(Err(VersionConflict::new(expected, observed)
                        .expect("the expectation just failed against the observation")));
                }

                let mut next = position;
                for (event_type, payload, envelope) in encoded {
                    next += 1;
                    transaction
                        .execute(
                            "INSERT INTO stream_events \
                             (category, stream_key, event_type, sequence, event_data, event_metadata) \
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                            rusqlite::params![category, key, event_type, next, payload, envelope],
                        )
                        .map_err(|error| classify_lock_failure(error, lock_timeout))?;
                }

                transaction
                    .commit()
                    .map_err(|error| classify_lock_failure(error, lock_timeout))?;
                Ok(Ok(StreamSequence::new(stored_position(next))
                    .expect("a batch holds at least one event")))
            })
            .await;

        match actor_outcome(outcome) {
            Ok(Ok(sequence)) => Ok(sequence),
            Ok(Err(conflict)) => Err(AppendError::Conflict(conflict)),
            Err(SqliteStreamsError::LockTimeout(bound)) => Err(AppendError::LockTimeout(bound)),
            Err(error) => Err(AppendError::Backend(error)),
        }
    }
}
