//! The SQLite [`AtomicStreams`] implementation (ADR 0006, under ADR
//! 0010's post-pivot single-writer funnel).
//!
//! One transaction does the whole batch under SQLite's database-wide
//! write lock, which every write path shares, so insert order is commit
//! order by construction. Every check runs before the first insert, so
//! a rejected batch leaves no partial rows.

#![allow(dead_code)]

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

#[expect(unused_imports, reason = "todo!() bodies; filled by E28")]
use crate::streams::batch::{
    expectation_satisfied, BatchConflict, ConstraintViolation, DuplicateIntent,
};

use super::{SqliteStreamsError, DEFAULT_LOCK_TIMEOUT};

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
    #[expect(unused_variables, reason = "todo!() body; filled by E28")]
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
        todo!("E28: batch write")
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
#[expect(unused_variables, reason = "todo!() body; filled by E28")]
fn statement_error(error: rusqlite::Error, bound: Duration) -> TransactError<SqliteStreamsError> {
    todo!("E28: batch statement error")
}

/// A per-event insert's failure. The intent-key index rejecting the
/// row is the saga runner's typed redelivery signal on the write's
/// stream; every other failure - including any other unique violation -
/// falls through to the shared statement classification unchanged.
#[expect(unused_variables, reason = "todo!() body; filled by E28")]
fn insert_error(
    error: rusqlite::Error,
    stream: &StreamRef,
    bound: Duration,
) -> TransactError<SqliteStreamsError> {
    todo!("E28: batch insert error")
}

/// Every addressed stream's head inside the write lock, in one grouped
/// read.
#[expect(unused_variables, reason = "todo!() body; filled by E28")]
fn locked_heads(
    conn: &rusqlite::Connection,
    streams: &[StreamRef],
) -> Result<HashMap<(String, String), StreamVersion>, TransactError<SqliteStreamsError>> {
    todo!("E28: batch heads")
}

/// The head of a stream the batch observed inside the write lock.
#[expect(unused_variables, reason = "todo!() body; filled by E28")]
fn head_of(heads: &HashMap<(String, String), StreamVersion>, stream: &StreamRef) -> StreamVersion {
    todo!("E28: batch head")
}

impl AtomicStreams for SqliteDatabase {
    type Batch = SqliteBatch;
    type Error = SqliteStreamsError;

    #[expect(unused_variables, reason = "todo!() body; filled by E28")]
    async fn transact(&self, batch: Self::Batch) -> Result<(), TransactError<Self::Error>> {
        todo!("E28: transact")
    }
}
