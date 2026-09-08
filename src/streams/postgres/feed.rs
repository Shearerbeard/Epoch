//! The PostgreSQL [`EventFeed`] implementation (ADR 0010, post-pivot).
//!
//! The write path's single-writer funnel makes `global_sequence`
//! commit-ordered, so the cursor is a plain maximum over it: a poll
//! reads the category's events past the group's cursor in
//! `global_sequence` order and can neither skip a committed event nor
//! wait on a hole (a rolled-back append leaves no row, and under one
//! writer nothing may be in flight below a committed value).
//!
//! Per-group progress lives in `epoch_feed_cursors` (migration step
//! 4). Both poll and ack run as one transaction each - the entries a
//! poll returns and the watermark it records derive from one
//! snapshot, and an ack's check-then-advance cannot interleave with
//! itself. The row is locked for the transaction's life, which is
//! also the v1 one-poller-per-group contract's backstop: a second
//! poller contends on the row lock rather than racing the watermark.
//!
//! Poll and ack bound their lock waits: each transaction sets a
//! local `lock_timeout` before taking the cursor row, and an expiry
//! surfaces as the retryable [`PgStreamsError::LockTimeout`], never a
//! generic backend fault (ADR 0010). The feed bounds lock waits only;
//! no idle-holder bound exists on this path.

use std::fmt::Debug;
use std::marker::PhantomData;
use std::time::Duration;

use serde::de::DeserializeOwned;

use crate::decider::Event;
use crate::streams::batch::StreamRef;
use crate::streams::feed::{
    AckError, ConsumerGroup, DeliveredWatermark, EventFeed, FeedCursor, FeedEntry, FeedPosition,
    PollLimit, Regression, Undelivered,
};
use crate::streams::RecordedEvent;

use super::{lock_timeout_ms, PgPool, PgStreamsError, DEFAULT_LOCK_TIMEOUT};

/// A feed over one category of a PostgreSQL database.
pub struct PgEventFeed<E> {
    pool: PgPool,
    category: String,
    lock_timeout: Duration,
    _marker: PhantomData<fn() -> E>,
}

impl<E> PgEventFeed<E> {
    /// `category` is the feed's namespace, the same string its
    /// writers append under.
    pub fn new(pool: PgPool, category: &str) -> Self {
        Self {
            pool,
            category: category.to_owned(),
            lock_timeout: DEFAULT_LOCK_TIMEOUT,
            _marker: PhantomData,
        }
    }

    /// Set the poll/ack lock-wait bound. A cursor row or index lock
    /// held past it surfaces as the retryable
    /// [`PgStreamsError::LockTimeout`] when it expires (ADR 0010).
    pub fn with_lock_timeout(self, lock_timeout: Duration) -> Self {
        Self {
            lock_timeout,
            ..self
        }
    }
}

/// Shares the pool, exactly as the stores do (ADR 0005).
impl<E> Clone for PgEventFeed<E> {
    fn clone(&self) -> Self {
        Self {
            pool: self.pool.clone(),
            category: self.category.clone(),
            lock_timeout: self.lock_timeout,
            _marker: PhantomData,
        }
    }
}

/// Apply the feed's lock-wait bound to one poll/ack transaction
/// before lock-taking work: `SET LOCAL lock_timeout` bounding the
/// cursor row's `FOR UPDATE` and every index lock the statements
/// take. Converted by [`lock_timeout_ms`], so a zero or
/// sub-millisecond bound normalizes to the 1ms floor - the one
/// "wait forever" value these paths must not set. The feed-side,
/// lock-only counterpart of the write path's
/// `configure_writer_timeouts`: the feed has no idle-in-transaction
/// bound.
async fn configure_feed_timeout(
    tx: &tokio_postgres::Transaction<'_>,
    bound_ms: u64,
) -> Result<(), tokio_postgres::Error> {
    tx.batch_execute(&format!("SET LOCAL lock_timeout = '{bound_ms}ms';"))
        .await
}

/// Lock-timeout expiry inside a poll or ack transaction is a distinct
/// retryable outcome (`PgStreamsError::LockTimeout`), never a generic
/// backend fault (ADR 0010, the feed-side twin of the append and
/// batch statement classifiers). Every statement after the GUC is in
/// effect routes its failure through here - the cursor row's
/// `FOR UPDATE`, the head read, the watermark upsert, the cursor
/// UPDATE, and the one commit each transaction makes can all wait on
/// locks. Ack's two branches - the no-op at the cursor and the
/// advance - each commit through this classifier. Pre-bound failures
/// are not classified: the GUC is not yet in effect, so a pool
/// checkout stays `PgStreamsError::Pool` and a BEGIN or config
/// failure stays an ordinary `Connection` error.
fn feed_statement_error(error: tokio_postgres::Error, bound: Duration) -> PgStreamsError {
    if error.code() == Some(&tokio_postgres::error::SqlState::LOCK_NOT_AVAILABLE) {
        PgStreamsError::LockTimeout(bound)
    } else {
        PgStreamsError::Connection(error)
    }
}

/// One group's progress row as poll and ack read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Progress {
    cursor: u64,
    delivered_to: u64,
}

impl Progress {
    fn from_row(cursor: i64, delivered_to: i64) -> Self {
        // Migration steps 0004 and 0005 keep the watermark at or
        // above the cursor and both values non-negative, so the u64
        // casts are storage-corruption assertions rather than
        // branches.
        Self {
            cursor: u64::try_from(cursor).expect("stored cursors are non-negative"),
            delivered_to: u64::try_from(delivered_to).expect("stored watermarks are non-negative"),
        }
    }
}

impl<E> EventFeed<E> for PgEventFeed<E>
where
    E: Event + DeserializeOwned + Send + Sync + Debug,
{
    type Error = PgStreamsError;

    async fn poll(
        &self,
        group: &ConsumerGroup,
        limit: PollLimit,
    ) -> Result<Vec<FeedEntry<E>>, Self::Error> {
        let bound_ms = lock_timeout_ms(self.lock_timeout);
        let effective_bound = Duration::from_millis(bound_ms);
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;

        // The bound is in effect before the cursor row's FOR UPDATE,
        // the transaction's first lock-taking statement.
        configure_feed_timeout(&tx, bound_ms).await?;

        let progress = tx
            .query_opt(
                "SELECT cursor, delivered_to FROM epoch_feed_cursors \
                 WHERE category = $1 AND group_name = $2 FOR UPDATE",
                &[&self.category, &group.as_str()],
            )
            .await
            .map_err(|error| feed_statement_error(error, effective_bound))?
            .map(|row| Progress::from_row(row.get("cursor"), row.get("delivered_to")))
            .unwrap_or(Progress {
                cursor: 0,
                delivered_to: 0,
            });

        // The cursor came from an i64 column, so the conversion back
        // cannot fail; a limit beyond i64::MAX means "no limit".
        let lower = i64::try_from(progress.cursor).expect("a stored cursor fits i64");
        let limit = i64::try_from(limit.get()).unwrap_or(i64::MAX);
        let rows = tx
            .query(
                "SELECT global_sequence, stream_key, event_data, event_metadata \
                 FROM stream_events \
                 WHERE category = $1 AND global_sequence > $2 \
                 ORDER BY global_sequence ASC LIMIT $3",
                &[&self.category, &lower, &limit],
            )
            .await
            .map_err(|error| feed_statement_error(error, effective_bound))?;

        let mut entries = Vec::with_capacity(rows.len());
        for row in &rows {
            let position = FeedPosition::new(
                u64::try_from(row.get::<_, i64>("global_sequence"))
                    .expect("global_sequence is non-negative"),
            )
            .expect("a stored global_sequence of a committed event is nonzero");
            let event: E = serde_json::from_value(row.get("event_data"))?;
            let metadata: crate::streams::EventMetadata =
                serde_json::from_value(row.get("event_metadata"))?;
            let key: String = row.get("stream_key");
            entries.push(FeedEntry::new(
                position,
                StreamRef::new(&self.category, &key),
                RecordedEvent::keyed(event, metadata),
            ));
        }

        if let Some(tip) = entries.last() {
            let tip = i64::try_from(tip.position().get()).expect("a position fits i64");
            tx.execute(
                "INSERT INTO epoch_feed_cursors (category, group_name, cursor, delivered_to) \
                 VALUES ($1, $2, 0, $3) \
                 ON CONFLICT (category, group_name) \
                 DO UPDATE SET delivered_to = GREATEST(epoch_feed_cursors.delivered_to, $3)",
                &[&self.category, &group.as_str(), &tip],
            )
            .await
            .map_err(|error| feed_statement_error(error, effective_bound))?;
        }

        tx.commit()
            .await
            .map_err(|error| feed_statement_error(error, effective_bound))?;
        Ok(entries)
    }

    async fn ack(
        &self,
        group: &ConsumerGroup,
        position: FeedPosition,
    ) -> Result<(), AckError<Self::Error>> {
        let bound_ms = lock_timeout_ms(self.lock_timeout);
        let effective_bound = Duration::from_millis(bound_ms);
        let mut conn = self
            .pool
            .get()
            .await
            .map_err(|e| AckError::Backend(PgStreamsError::from(e)))?;
        let tx = conn
            .transaction()
            .await
            .map_err(|e| AckError::Backend(PgStreamsError::from(e)))?;

        // The bound is in effect before the cursor row's FOR UPDATE,
        // the transaction's first lock-taking statement. A config
        // failure is pre-bound: an ordinary backend error, not a
        // classified timeout.
        configure_feed_timeout(&tx, bound_ms)
            .await
            .map_err(|e| AckError::Backend(PgStreamsError::from(e)))?;

        let progress = tx
            .query_opt(
                "SELECT cursor, delivered_to FROM epoch_feed_cursors \
                 WHERE category = $1 AND group_name = $2 FOR UPDATE",
                &[&self.category, &group.as_str()],
            )
            .await
            .map_err(|error| feed_statement_error(error, effective_bound))?
            .map(|row| Progress::from_row(row.get("cursor"), row.get("delivered_to")))
            .unwrap_or(Progress {
                cursor: 0,
                delivered_to: 0,
            });

        let attempted = position.get();
        if attempted == progress.cursor {
            tx.commit()
                .await
                .map_err(|error| feed_statement_error(error, effective_bound))?;
            return Ok(());
        }
        if attempted < progress.cursor {
            // The guard just failed, so the pair is a genuine
            // regression and the constructor cannot reject it.
            return Err(AckError::Regression(
                Regression::new(FeedCursor::at(progress.cursor), position)
                    .expect("the position was just checked below the cursor"),
            ));
        }
        if attempted > progress.delivered_to {
            // The guard just failed, so the position is genuinely past
            // delivery and the constructor cannot reject it.
            return Err(AckError::NotDelivered(
                Undelivered::new(DeliveredWatermark::at(progress.delivered_to), position)
                    .expect("the position was just checked past delivery"),
            ));
        }

        let to = i64::try_from(attempted).expect("a position fits i64");
        // The guards above imply the row exists: with no row the
        // delivered watermark reads zero and every (nonzero) position
        // is rejected as undelivered. A plain UPDATE states the real
        // transition, and the row count asserts it.
        let advanced = tx
            .execute(
                "UPDATE epoch_feed_cursors SET cursor = $3 \
                 WHERE category = $1 AND group_name = $2",
                &[&self.category, &group.as_str(), &to],
            )
            .await
            .map_err(|error| feed_statement_error(error, effective_bound))?;
        assert_eq!(
            advanced, 1,
            "the cursor row a poll inserted was just locked FOR UPDATE"
        );

        tx.commit()
            .await
            .map_err(|error| feed_statement_error(error, effective_bound))?;
        Ok(())
    }
}
