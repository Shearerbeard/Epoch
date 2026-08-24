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
//! itself. The row is locked for the statement's life, which is also
//! the v1 one-poller-per-group contract's backstop: a second poller
//! contends on the row lock rather than racing the watermark.

use std::fmt::Debug;
use std::marker::PhantomData;

use serde::de::DeserializeOwned;

use crate::decider::Event;
use crate::streams::batch::StreamRef;
use crate::streams::feed::{
    AckError, ConsumerGroup, DeliveredWatermark, EventFeed, FeedCursor, FeedEntry, FeedPosition,
    PollLimit, Regression, Undelivered,
};
use crate::streams::RecordedEvent;

use super::{PgPool, PgStreamsError};

/// A feed over one category of a PostgreSQL database.
pub struct PgEventFeed<E> {
    pool: PgPool,
    category: String,
    _marker: PhantomData<fn() -> E>,
}

impl<E> PgEventFeed<E> {
    /// `category` is the feed's namespace, the same string its
    /// writers append under.
    pub fn new(pool: PgPool, category: &str) -> Self {
        Self {
            pool,
            category: category.to_owned(),
            _marker: PhantomData,
        }
    }
}

/// Shares the pool, exactly as the stores do (ADR 0005).
impl<E> Clone for PgEventFeed<E> {
    fn clone(&self) -> Self {
        Self {
            pool: self.pool.clone(),
            category: self.category.clone(),
            _marker: PhantomData,
        }
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
        // The migration's check constraint keeps both non-negative
        // and the watermark at or above the cursor, so the u64 casts
        // are storage-corruption assertions rather than branches.
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
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;

        let progress = tx
            .query_opt(
                "SELECT cursor, delivered_to FROM epoch_feed_cursors \
                 WHERE category = $1 AND group_name = $2 FOR UPDATE",
                &[&self.category, &group.as_str().to_owned()],
            )
            .await?
            .map(|row| Progress::from_row(row.get("cursor"), row.get("delivered_to")))
            .unwrap_or(Progress {
                cursor: 0,
                delivered_to: 0,
            });

        let lower = i64::try_from(progress.cursor).unwrap_or(i64::MAX);
        let limit = i64::try_from(limit.get()).unwrap_or(i64::MAX);
        let rows = tx
            .query(
                "SELECT global_sequence, stream_key, event_data, event_metadata \
                 FROM stream_events \
                 WHERE category = $1 AND global_sequence > $2 \
                 ORDER BY global_sequence ASC LIMIT $3",
                &[&self.category, &lower, &limit],
            )
            .await?;

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
                &[&self.category, &group.as_str().to_owned(), &tip],
            )
            .await?;
        }

        tx.commit().await?;
        Ok(entries)
    }

    async fn ack(
        &self,
        group: &ConsumerGroup,
        position: FeedPosition,
    ) -> Result<(), AckError<Self::Error>> {
        let mut conn = self
            .pool
            .get()
            .await
            .map_err(|e| AckError::Backend(PgStreamsError::from(e)))?;
        let tx = conn
            .transaction()
            .await
            .map_err(|e| AckError::Backend(PgStreamsError::from(e)))?;

        let progress = tx
            .query_opt(
                "SELECT cursor, delivered_to FROM epoch_feed_cursors \
                 WHERE category = $1 AND group_name = $2 FOR UPDATE",
                &[&self.category, &group.as_str().to_owned()],
            )
            .await
            .map_err(|e| AckError::Backend(PgStreamsError::from(e)))?
            .map(|row| Progress::from_row(row.get("cursor"), row.get("delivered_to")))
            .unwrap_or(Progress {
                cursor: 0,
                delivered_to: 0,
            });

        let attempted = position.get();
        if attempted == progress.cursor {
            tx.commit()
                .await
                .map_err(|e| AckError::Backend(PgStreamsError::from(e)))?;
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
        tx.execute(
            "INSERT INTO epoch_feed_cursors (category, group_name, cursor, delivered_to) \
             VALUES ($1, $2, $3, $3) \
             ON CONFLICT (category, group_name) \
             DO UPDATE SET cursor = $3",
            &[&self.category, &group.as_str().to_owned(), &to],
        )
        .await
        .map_err(|e| AckError::Backend(PgStreamsError::from(e)))?;

        tx.commit()
            .await
            .map_err(|e| AckError::Backend(PgStreamsError::from(e)))?;
        Ok(())
    }
}
