//! The SQLite [`AtomicStreams`] implementation (ADR 0006, under ADR
//! 0010's post-pivot single-writer funnel).
//!
//! One transaction does the whole batch under SQLite's database-wide
//! write lock, which every write path shares, so insert order is commit
//! order by construction. Every check runs before the first insert, so
//! a rejected batch leaves no partial rows.

use std::collections::HashMap;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use tokio_rusqlite::{rusqlite, Connection};

use crate::decider::Event;
use crate::streams::batch::{
    AtomicStreams, Batch, BatchBuilder, BatchSource, DuplicateWrite, StreamRef, TransactError,
};
use crate::streams::{EventBatch, ExpectedVersion, StreamId, StreamVersion};

use crate::streams::batch::{
    expectation_satisfied, BatchConflict, ConstraintViolation, DuplicateIntent,
};

use super::{SqliteEventFeed, SqliteEventStreams, SqliteStreamsError, DEFAULT_LOCK_TIMEOUT};

/// An event erased to the backend's wire form at push (ADR 0006):
/// encoding is fallible and happens before any transaction starts. The
/// envelope rides as its JSON form, the same as the payload.
#[derive(Debug, Clone, PartialEq)]
pub struct SqliteEncodedEvent {
    event_type: String,
    data: Value,
    metadata: Value,
}

/// A batch the SQLite database can commit.
pub type SqliteBatch = Batch<SqliteEncodedEvent>;

/// The builder for one.
pub type SqliteBatchBuilder = BatchBuilder<SqliteEncodedEvent>;

/// The unique index the saga outbox's intent key rides (migration
/// `0003-outbox-intent-index.sql`): unique on the keyed envelope, partial
/// on the outbox category, so only a duplicate outbox intent write can
/// name it.
const OUTBOX_INTENT_INDEX: &str = "stream_events_outbox_intent";

/// The most `(category, stream_key)` pairs one head-read statement binds.
/// SQLite's variable limit is 32766 (3.32+, the bundled default) and each
/// pair spends two variables, so this chunk stays inside it. The chunk
/// results merge in Rust, inside the one transaction.
const HEAD_READ_CHUNK: usize = 16_383;

/// The database handle: the value that owns the connection, and so the
/// only value whose scope covers a cross-category batch (ADR 0006's
/// ownership seam). Per-category [`super::SqliteEventStreams`] stores
/// stay independently constructible for the append path; a batch write
/// carries a category, a rendered stream key and its events, never a
/// store, so the only connection a transact can reach is this handle's.
#[derive(Clone)]
pub struct SqliteDatabase {
    connection: Connection,
    lock_timeout: Duration,
}

impl SqliteDatabase {
    /// The handle over one SQLite connection. Clones share it, so a
    /// clone's batch commits against the same database (ADR 0005).
    pub fn new(connection: Connection) -> Self {
        Self {
            connection,
            lock_timeout: DEFAULT_LOCK_TIMEOUT,
        }
    }

    /// Set the write-lock wait bound. A wedged writer surfaces as the
    /// retryable [`TransactError::LockTimeout`] when the wait expires,
    /// never a version conflict (ADR 0010).
    pub fn with_lock_timeout(self, lock_timeout: Duration) -> Self {
        Self {
            lock_timeout,
            ..self
        }
    }

    /// A builder for a batch this handle can commit.
    pub fn batch(&self) -> SqliteBatchBuilder {
        SqliteBatchBuilder::new()
    }

    /// A per-category store over this handle's connection, mirroring
    /// `InMemoryDatabase::category` in name and role. Clones of the
    /// connection share one actor and one database (ADR 0005), so the
    /// store contends on the same stored streams as every other child
    /// of the handle: a handle built once at an application root
    /// serves every surface - store, batch, feed - over the one
    /// connection. Infallible: the backend has no category registry to
    /// claim, so there is nothing `Result`-shaped to return, the same
    /// unvalidated `&str` posture [`SqliteEventStreams::new`] takes.
    pub fn category<Id, E>(&self, category: &str) -> SqliteEventStreams<Id, E> {
        SqliteEventStreams::new(self.connection.clone(), category)
            .with_lock_timeout(self.lock_timeout)
    }

    /// A per-category feed over this handle's connection, mirroring
    /// `InMemoryDatabase::feed`. This is how a handle over a private
    /// in-memory database hands its connection to a feed, which the
    /// raw constructors cannot express:
    /// [`SqliteEventStreams::open_in_memory`] keeps the connection to
    /// itself. Clones of the connection share one actor and one
    /// database (ADR 0005), so the feed observes the same stored
    /// streams the handle's stores and batches write.
    pub fn feed<E>(&self, category: &str) -> SqliteEventFeed<E> {
        SqliteEventFeed::new(self.connection.clone(), category).with_lock_timeout(self.lock_timeout)
    }

    /// Apply pending schema migrations over this handle's connection:
    /// the same migration [`SqliteEventStreams::migrate`] makes, both
    /// delegating to `super::migrate_connection` so the two cannot
    /// drift. Run once at the application root, it prepares the one
    /// database every child of the handle - store, batch, feed - then
    /// shares (ADR 0005).
    pub async fn migrate(&self) -> Result<(), SqliteStreamsError> {
        super::migrate_connection(&self.connection, self.lock_timeout).await
    }
}

impl BatchSource for SqliteDatabase {
    type Wire = SqliteEncodedEvent;

    fn builder(&self) -> SqliteBatchBuilder {
        self.batch()
    }
}

impl SqliteBatchBuilder {
    /// Add this stream's write to the batch, encoding its events to the
    /// backend's JSON wire form now: a failed encoding is a build-time
    /// error, before any transaction starts. `expected` is checked
    /// against the stream's pre-batch head when the batch commits, the
    /// same check single append makes.
    pub fn write<Id, E>(
        &mut self,
        category: &str,
        id: &Id,
        expected: ExpectedVersion,
        events: &EventBatch<E>,
    ) -> Result<(), SqliteWriteError>
    where
        Id: StreamId,
        E: Event + Serialize,
    {
        let encoded = events
            .records()
            .iter()
            .map(|record| {
                Ok(SqliteEncodedEvent {
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
#[derive(Debug, Error)]
pub enum SqliteWriteError {
    /// The batch already writes this stream.
    #[error(transparent)]
    Duplicate(#[from] DuplicateWrite),
    /// The events do not encode to the backend's JSON wire form.
    #[error("event payload serialization error: {0}")]
    Encoding(#[from] serde_json::Error),
}

/// A statement failure inside a transact. Lock-timeout expiry is a
/// distinct retryable outcome, never a version conflict (ADR 0006).
fn statement_error(error: rusqlite::Error, bound: Duration) -> TransactError<SqliteStreamsError> {
    if crate::streams::sqlite::is_lock_timeout(&error) {
        // The payload names the bound the operation really installed,
        // with `lock_timeout_ms`'s 1ms floor, not the raw configuration.
        TransactError::LockTimeout(Duration::from_millis(
            crate::streams::sqlite::lock_timeout_ms(bound),
        ))
    } else {
        TransactError::Backend(SqliteStreamsError::Connection(error))
    }
}

/// A per-event insert's failure. The intent-key index rejecting the
/// row is the saga runner's typed redelivery signal on the write's
/// stream; every other failure - a non-unique constraint, another
/// index's unique violation, anything else - falls through to the
/// shared statement classification unchanged. `pub(super)` so the
/// conformance tests can drive it with the synthetic error pair real
/// SQLite cannot produce.
pub(super) fn insert_error(
    error: rusqlite::Error,
    stream: &StreamRef,
    bound: Duration,
) -> TransactError<SqliteStreamsError> {
    // SQLite reports a constraint failure as a primary code, an
    // extended code naming the violated constraint's kind, and a
    // message, with no structured field naming the index. The primary
    // code alone cannot separate a unique violation from a CHECK
    // violation - both report `ConstraintViolation` - so the extended
    // code is tested before the message text and only a
    // `SQLITE_CONSTRAINT_UNIQUE` reaches it; the index name is matched
    // inside the message because that is the only place the driver
    // reports it. The message text remains the one fragile dependency,
    // deliberately isolated here.
    let rejected_intent = matches!(
        &error,
        rusqlite::Error::SqliteFailure(failure, Some(message))
            if failure.code == rusqlite::ErrorCode::ConstraintViolation
                && failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                && message.contains(OUTBOX_INTENT_INDEX)
    );
    if rejected_intent {
        TransactError::DuplicateIntent(DuplicateIntent::new(stream.clone()))
    } else {
        statement_error(error, bound)
    }
}

/// Every addressed stream's head inside the write lock, in one grouped
/// read.
fn locked_heads(
    conn: &rusqlite::Connection,
    streams: &[StreamRef],
) -> Result<HashMap<(String, String), StreamVersion>, TransactError<SqliteStreamsError>> {
    // A read here runs under the write lock the `BEGIN IMMEDIATE` already
    // holds, so it cannot time out; a failure is a backend fault.
    let failure =
        |error: rusqlite::Error| TransactError::Backend(SqliteStreamsError::Connection(error));

    let mut heads = HashMap::with_capacity(streams.len());
    for chunk in streams.chunks(HEAD_READ_CHUNK) {
        let placeholders = (0..chunk.len())
            .map(|index| format!("(?{}, ?{})", index * 2 + 1, index * 2 + 2))
            .collect::<Vec<_>>()
            .join(", ");
        // The correlated scalar subquery is pinned: unlike a `LEFT JOIN`
        // plus `GROUP BY`, it costs no temp B-tree and returns a row with
        // head 0 for a stream with no events, which `head_of` needs.
        let sql = format!(
            "WITH heads(category, stream_key) AS (VALUES {placeholders}) \
             SELECT h.category, h.stream_key, \
                    (SELECT COALESCE(MAX(sequence), 0) FROM stream_events e \
                      WHERE e.category = h.category AND e.stream_key = h.stream_key) AS head \
               FROM heads h"
        );

        let mut values = Vec::with_capacity(chunk.len() * 2);
        for stream in chunk {
            values.push(stream.category());
            values.push(stream.key());
        }

        let mut statement = conn.prepare(&sql).map_err(failure)?;
        let mut rows = statement
            .query(rusqlite::params_from_iter(values))
            .map_err(failure)?;
        while let Some(row) = rows.next().map_err(failure)? {
            let category: String = row.get(0).map_err(failure)?;
            let key: String = row.get(1).map_err(failure)?;
            let head: i64 = row.get(2).map_err(failure)?;
            heads.insert(
                (category, key),
                super::version_of(super::stored_position(head)),
            );
        }
    }
    Ok(heads)
}

/// The head of a stream the batch observed inside the write lock.
fn head_of(heads: &HashMap<(String, String), StreamVersion>, stream: &StreamRef) -> StreamVersion {
    *heads
        .get(&(stream.category().to_owned(), stream.key().to_owned()))
        .expect("every addressed stream was observed under its lock")
}

impl AtomicStreams for SqliteDatabase {
    type Batch = SqliteBatch;
    type Error = SqliteStreamsError;

    async fn transact(&self, batch: Self::Batch) -> Result<(), TransactError<Self::Error>> {
        let connection = self.connection.clone();
        let bound = self.lock_timeout;
        let outcome = connection
            .call(move |conn| {
                // The effective bound goes in inside the closure, before
                // the write lock is taken: the busy timeout is a property
                // of the connection, and clones share one, so the bound in
                // force must be the one this operation installs.
                crate::streams::sqlite::install_busy_timeout(conn, bound)
                    .map_err(|error| statement_error(error, bound))?;
                let transaction = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(|error| statement_error(error, bound))?;

                // All checks precede all writes (ADR 0006). Every head is
                // read under the write lock, then every constraint, then
                // every write expectation; a failure returns before the
                // first insert, so the dropped transaction rolls back
                // whole.
                let streams: Vec<StreamRef> =
                    batch.locked_streams().into_iter().cloned().collect();
                let heads = locked_heads(&transaction, &streams)?;

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
                        let data = serde_json::to_string(&event.data)
                            .map_err(SqliteStreamsError::Serialization)?;
                        let metadata = serde_json::to_string(&event.metadata)
                            .map_err(SqliteStreamsError::Serialization)?;
                        transaction
                            .execute(
                                "INSERT INTO stream_events \
                                 (category, stream_key, event_type, sequence, event_data, event_metadata) \
                                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                                rusqlite::params![
                                    stream.category(),
                                    stream.key(),
                                    event.event_type,
                                    sequence,
                                    data,
                                    metadata,
                                ],
                            )
                            .map_err(|error| insert_error(error, stream, bound))?;
                    }
                }

                transaction
                    .commit()
                    .map_err(|error| statement_error(error, bound))?;
                Ok(())
            })
            .await;

        match outcome {
            Ok(()) => Ok(()),
            Err(tokio_rusqlite::Error::Error(error)) => Err(error),
            Err(tokio_rusqlite::Error::Close((_, error))) => Err(TransactError::Backend(
                SqliteStreamsError::Connection(error),
            )),
            Err(_) => Err(TransactError::Backend(SqliteStreamsError::Handle(
                tokio_rusqlite::Error::ConnectionClosed,
            ))),
        }
    }
}
