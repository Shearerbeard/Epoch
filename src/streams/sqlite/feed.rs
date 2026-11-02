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

use std::fmt::Debug;
use std::marker::PhantomData;
use std::time::Duration;

use serde::de::DeserializeOwned;
use tokio_rusqlite::Connection;

use crate::decider::Event;
use crate::streams::feed::{
    AckError, ConsumerGroup, EventFeed, FeedEntry, FeedPosition, PollLimit,
};

use crate::streams::feed::{DeliveredWatermark, FeedCursor, Regression, Undelivered};

use crate::streams::batch::StreamRef;

use crate::streams::RecordedEvent;

use super::{SqliteStreamsError, DEFAULT_LOCK_TIMEOUT};

/// One group's progress row as poll and ack read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Progress {
    cursor: u64,
    delivered_to: u64,
}

impl Progress {
    /// One group's progress row, read from the cursor table. The two
    /// values move together: the schema's checks keep the watermark at
    /// or above the cursor and both non-negative, so the u64 casts
    /// are storage-corruption assertions rather than branches.
    fn from_row(cursor: i64, delivered_to: i64) -> Self {
        Self {
            cursor: u64::try_from(cursor).expect("stored cursors are non-negative"),
            delivered_to: u64::try_from(delivered_to).expect("stored watermarks are non-negative"),
        }
    }
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

    async fn poll(
        &self,
        group: &ConsumerGroup,
        limit: PollLimit,
    ) -> Result<Vec<FeedEntry<E>>, Self::Error> {
        let connection = self.connection.clone();
        let category = self.category.clone();
        let group = group.as_str().to_owned();
        let lock_timeout = self.lock_timeout;
        // A limit beyond i64::MAX means "no limit"; the cursor came from
        // an i64 column, so the conversion back cannot fail.
        let limit = i64::try_from(limit.get()).unwrap_or(i64::MAX);

        let outcome = connection
            .call(move |conn| {
                // The bound goes in before the BEGIN: the write lock is
                // taken by the BEGIN itself, so a bound installed after
                // it would bound nothing.
                super::install_busy_timeout(conn, lock_timeout)
                    .map_err(|error| super::classify_lock_failure(error, lock_timeout))?;
                let transaction = conn
                    .transaction_with_behavior(
                        tokio_rusqlite::rusqlite::TransactionBehavior::Immediate,
                    )
                    .map_err(|error| super::classify_lock_failure(error, lock_timeout))?;

                // A group with no row starts at cursor 0 and delivered 0.
                let progress = match transaction.query_row(
                    "SELECT cursor, delivered_to FROM epoch_feed_cursors \
                     WHERE category = ?1 AND group_name = ?2",
                    tokio_rusqlite::rusqlite::params![&category, &group],
                    |row| Ok(Progress::from_row(row.get(0)?, row.get(1)?)),
                ) {
                    Ok(progress) => progress,
                    Err(tokio_rusqlite::rusqlite::Error::QueryReturnedNoRows) => {
                        Progress::from_row(0, 0)
                    }
                    Err(error) => return Err(super::classify_lock_failure(error, lock_timeout)),
                };

                let lower = i64::try_from(progress.cursor).expect("a stored cursor fits i64");
                // The rows leave the closure as owned strings: a rusqlite
                // `Row` borrows the statement that produced it.
                let mut statement = transaction
                    .prepare(
                        "SELECT global_sequence, stream_key, event_data, event_metadata \
                         FROM stream_events \
                         WHERE category = ?1 AND global_sequence > ?2 \
                         ORDER BY global_sequence ASC LIMIT ?3",
                    )
                    .map_err(|error| super::classify_lock_failure(error, lock_timeout))?;
                let rows = statement
                    .query_map(
                        tokio_rusqlite::rusqlite::params![&category, lower, limit],
                        |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                            ))
                        },
                    )
                    .map_err(|error| super::classify_lock_failure(error, lock_timeout))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| super::classify_lock_failure(error, lock_timeout))?;
                drop(statement);

                if let Some((tip, ..)) = rows.last() {
                    // The `max` keeps the delivered watermark monotonic.
                    transaction
                        .execute(
                            "INSERT INTO epoch_feed_cursors (category, group_name, cursor, delivered_to) \
                             VALUES (?1, ?2, 0, ?3) \
                             ON CONFLICT (category, group_name) \
                             DO UPDATE SET delivered_to = max(epoch_feed_cursors.delivered_to, excluded.delivered_to)",
                            tokio_rusqlite::rusqlite::params![&category, &group, *tip],
                        )
                        .map_err(|error| super::classify_lock_failure(error, lock_timeout))?;
                }

                transaction
                    .commit()
                    .map_err(|error| super::classify_lock_failure(error, lock_timeout))?;
                Ok(rows)
            })
            .await;

        let rows = super::actor_outcome(outcome)?;
        let mut entries = Vec::with_capacity(rows.len());
        for (global_sequence, stream_key, event_data, event_metadata) in rows {
            let position = FeedPosition::new(
                u64::try_from(global_sequence).expect("global_sequence is non-negative"),
            )
            .expect("a stored global_sequence of a committed event is nonzero");
            let event: E = serde_json::from_str(&event_data)?;
            let metadata: crate::streams::EventMetadata = serde_json::from_str(&event_metadata)?;
            entries.push(FeedEntry::new(
                position,
                StreamRef::new(&self.category, &stream_key),
                RecordedEvent::keyed(event, metadata),
            ));
        }
        Ok(entries)
    }

    async fn ack(
        &self,
        group: &ConsumerGroup,
        position: FeedPosition,
    ) -> Result<(), AckError<Self::Error>> {
        let connection = self.connection.clone();
        let category = self.category.clone();
        let group = group.as_str().to_owned();
        let lock_timeout = self.lock_timeout;
        let attempted = position.get();

        let outcome = connection
            .call(move |conn| {
                super::install_busy_timeout(conn, lock_timeout)
                    .map_err(|error| super::classify_lock_failure(error, lock_timeout))?;
                let transaction = conn
                    .transaction_with_behavior(
                        tokio_rusqlite::rusqlite::TransactionBehavior::Immediate,
                    )
                    .map_err(|error| super::classify_lock_failure(error, lock_timeout))?;

                let progress = match transaction.query_row(
                    "SELECT cursor, delivered_to FROM epoch_feed_cursors \
                     WHERE category = ?1 AND group_name = ?2",
                    tokio_rusqlite::rusqlite::params![&category, &group],
                    |row| Ok(Progress::from_row(row.get(0)?, row.get(1)?)),
                ) {
                    Ok(progress) => progress,
                    Err(tokio_rusqlite::rusqlite::Error::QueryReturnedNoRows) => {
                        Progress::from_row(0, 0)
                    }
                    Err(error) => {
                        return Err(AckError::Backend(super::classify_lock_failure(
                            error,
                            lock_timeout,
                        )))
                    }
                };

                if attempted == progress.cursor {
                    transaction
                        .commit()
                        .map_err(|error| super::classify_lock_failure(error, lock_timeout))?;
                    return Ok(());
                }
                if attempted < progress.cursor {
                    return Err(AckError::Regression(
                        Regression::new(FeedCursor::at(progress.cursor), position)
                            .expect("the position was just checked below the cursor"),
                    ));
                }
                if attempted > progress.delivered_to {
                    return Err(AckError::NotDelivered(
                        Undelivered::new(DeliveredWatermark::at(progress.delivered_to), position)
                            .expect("the position was just checked past delivery"),
                    ));
                }

                let to = i64::try_from(attempted).expect("a position fits i64");
                transaction
                    .execute(
                        "UPDATE epoch_feed_cursors SET cursor = ?3 \
                         WHERE category = ?1 AND group_name = ?2",
                        tokio_rusqlite::rusqlite::params![&category, &group, to],
                    )
                    .map_err(|error| super::classify_lock_failure(error, lock_timeout))?;

                transaction
                    .commit()
                    .map_err(|error| super::classify_lock_failure(error, lock_timeout))?;
                Ok(())
            })
            .await;

        match outcome {
            Ok(()) => Ok(()),
            Err(tokio_rusqlite::Error::Error(rejection)) => Err(rejection),
            Err(tokio_rusqlite::Error::Close((_, error))) => {
                Err(AckError::Backend(SqliteStreamsError::Connection(error)))
            }
            Err(_) => Err(AckError::Backend(SqliteStreamsError::Handle(
                tokio_rusqlite::Error::ConnectionClosed,
            ))),
        }
    }
}
