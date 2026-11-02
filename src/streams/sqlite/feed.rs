//! The SQLite [`EventFeed`] implementation (ADR 0010, post-pivot).
//!
//! The write path's single-writer funnel makes `global_sequence`
//! commit-ordered, so the cursor is a plain maximum over it: a poll
//! reads the category's events past the group's cursor in
//! `global_sequence` order and can neither skip a committed event nor
//! wait on a hole.
//!
//! Per-group progress lives in `epoch_feed_cursors` (migration step
//! 2). Both poll and ack run as one transaction each - the entries a
//! poll returns and the watermark it records derive from one snapshot,
//! and an ack's check-then-advance cannot interleave with itself.
//!
//! Poll and ack bound their lock waits: each transaction installs the
//! busy timeout before taking the write lock, and an expiry surfaces
//! as the retryable [`SqliteStreamsError::LockTimeout`], never a
//! generic backend fault (ADR 0010). On SQLite a poll is a writer - it
//! records the delivered watermark - so it contends with event writers
//! for the database write lock. PostgreSQL's poll takes only a row
//! lock and never does; that difference is recorded in the design
//! note.

#![allow(dead_code)]

use std::fmt::Debug;
use std::marker::PhantomData;
use std::time::Duration;

use serde::de::DeserializeOwned;
use tokio_rusqlite::Connection;

use crate::decider::Event;
use crate::streams::feed::{
    AckError, ConsumerGroup, EventFeed, FeedEntry, FeedPosition, PollLimit,
};

#[expect(unused_imports, reason = "todo!() bodies; filled by E28")]
use crate::streams::feed::{DeliveredWatermark, FeedCursor, Regression, Undelivered};

#[expect(unused_imports, reason = "todo!() bodies; filled by E28")]
use crate::streams::batch::StreamRef;

#[expect(unused_imports, reason = "todo!() bodies; filled by E28")]
use crate::streams::RecordedEvent;

use super::{SqliteStreamsError, DEFAULT_LOCK_TIMEOUT};

/// One group's progress row as poll and ack read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Progress {
    cursor: u64,
    delivered_to: u64,
}

/// A feed over one category of a SQLite database.
pub struct SqliteEventFeed<E> {
    connection: Connection,
    category: String,
    lock_timeout: Duration,
    _marker: PhantomData<fn() -> E>,
}

impl<E> SqliteEventFeed<E> {
    /// `category` is the feed's namespace, the same string its writers
    /// append under.
    pub fn new(connection: Connection, category: &str) -> Self {
        Self {
            connection,
            category: category.to_owned(),
            lock_timeout: DEFAULT_LOCK_TIMEOUT,
            _marker: PhantomData,
        }
    }

    /// Set the poll/ack lock-wait bound. A write lock held past it
    /// surfaces as the retryable [`SqliteStreamsError::LockTimeout`]
    /// when it expires (ADR 0010).
    pub fn with_lock_timeout(self, lock_timeout: Duration) -> Self {
        Self {
            lock_timeout,
            ..self
        }
    }
}

/// Shares the connection, exactly as the stores do (ADR 0005).
impl<E> Clone for SqliteEventFeed<E> {
    fn clone(&self) -> Self {
        Self {
            connection: self.connection.clone(),
            category: self.category.clone(),
            lock_timeout: self.lock_timeout,
            _marker: PhantomData,
        }
    }
}

impl<E> EventFeed<E> for SqliteEventFeed<E>
where
    E: Event + DeserializeOwned + Send + Sync + Debug,
{
    type Error = SqliteStreamsError;

    #[expect(unused_variables, reason = "todo!() body; filled by E28")]
    async fn poll(
        &self,
        group: &ConsumerGroup,
        limit: PollLimit,
    ) -> Result<Vec<FeedEntry<E>>, Self::Error> {
        todo!("E28: poll")
    }

    #[expect(unused_variables, reason = "todo!() body; filled by E28")]
    async fn ack(
        &self,
        group: &ConsumerGroup,
        position: FeedPosition,
    ) -> Result<(), AckError<Self::Error>> {
        todo!("E28: ack")
    }
}
