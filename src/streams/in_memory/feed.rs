//! The in-memory [`EventFeed`] implementation (ADR 0010, post-pivot).
//!
//! The root's mutex is the single writer: appends and batches commit
//! under one lock, so the log's arrival order is its commit order and
//! a log index is a feed position (1-based). No abort burns a value,
//! so positions are contiguous - the honest in-memory analogue of the
//! single-writer funnel postgres now enforces with one advisory lock.
//!
//! Per-group progress (ack cursor and delivered watermark) lives in
//! the root beside the log, keyed by the feed's category and the
//! group's name: a group reading two categories holds two
//! independent cursors, each legal watermark movement over the
//! shared position space.

use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::Mutex;

use crate::decider::Event;
use crate::streams::batch::StreamRef;
use crate::streams::feed::{
    AckError, ConsumerGroup, DeliveredWatermark, EventFeed, FeedCursor, FeedEntry, FeedPosition,
    PollLimit, Regression, Undelivered,
};
use crate::streams::RecordedEvent;

use super::{InMemoryDatabase, Root};

/// One group's progress on one feed: what it acknowledged, and what
/// it was delivered. Zero means nothing yet for either watermark.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct GroupProgress {
    pub(crate) cursor: u64,
    pub(crate) delivered_to: u64,
}

/// A feed over one category of an [`InMemoryDatabase`].
pub struct InMemoryEventFeed<E> {
    root: Arc<Mutex<Root>>,
    category: String,
    _marker: PhantomData<fn() -> E>,
}

impl InMemoryDatabase {
    /// A feed view over this root's `category`, fixing the category's
    /// event type the same way [`InMemoryDatabase::category`] does.
    pub fn feed<E>(
        &self,
        category: &str,
    ) -> Result<InMemoryEventFeed<E>, super::CategoryTypeMismatch>
    where
        E: Send + Sync + 'static,
    {
        self.root
            .lock()
            .expect("event store lock poisoned")
            .claim(category, std::any::TypeId::of::<E>())?;
        Ok(InMemoryEventFeed {
            root: Arc::clone(&self.root),
            category: category.to_owned(),
            _marker: PhantomData,
        })
    }
}

/// Shares the root, exactly as the stores do (ADR 0005).
impl<E> Clone for InMemoryEventFeed<E> {
    fn clone(&self) -> Self {
        Self {
            root: Arc::clone(&self.root),
            category: self.category.clone(),
            _marker: PhantomData,
        }
    }
}

impl<E> InMemoryEventFeed<E>
where
    E: Event + Clone + Send + Sync + std::fmt::Debug + 'static,
{
    /// The category's entries after `cursor`, oldest first, at most
    /// `limit` of them. Entries are owned clones: delivery does not
    /// borrow the log.
    fn entries_after(&self, root: &Root, cursor: u64, limit: PollLimit) -> Vec<FeedEntry<E>> {
        let mut entries = Vec::new();
        // Positions are 1-based log indexes; the cursor is a count of
        // acknowledged positions, so scanning starts at its offset. A
        // cursor that does not fit usize exceeds every log length the
        // target can hold, so starting at the end is the exact empty
        // page - a refinement over the old `as` cast, which could
        // wrap and skip.
        let mut scan = usize::try_from(cursor).unwrap_or(root.log.len());
        while entries.len() < limit.get() && scan < root.log.len() {
            let stored = &root.log[scan];
            if stored.category == self.category {
                let position =
                    FeedPosition::new(scan as u64 + 1).expect("a log index plus one is nonzero");
                let record = RecordedEvent::keyed(
                    super::stored_event(&stored.payload),
                    stored.metadata.clone(),
                );
                entries.push(FeedEntry::new(
                    position,
                    StreamRef::new(&self.category, &stored.key),
                    record,
                ));
            }
            scan += 1;
        }
        entries
    }
}

impl<E> EventFeed<E> for InMemoryEventFeed<E>
where
    E: Event + Clone + Send + Sync + std::fmt::Debug + 'static,
{
    type Error = super::InMemoryError;

    async fn poll(
        &self,
        group: &ConsumerGroup,
        limit: PollLimit,
    ) -> Result<Vec<FeedEntry<E>>, Self::Error> {
        let mut root = self.root.lock().expect("event store lock poisoned");
        let progress = root.feed_progress(&self.category, group.as_str());
        let entries = self.entries_after(&root, progress.cursor, limit);
        if let Some(tip) = entries.last() {
            root.record_delivery(&self.category, group.as_str(), tip.position().get());
        }
        Ok(entries)
    }

    async fn ack(
        &self,
        group: &ConsumerGroup,
        position: FeedPosition,
    ) -> Result<(), AckError<Self::Error>> {
        let mut root = self.root.lock().expect("event store lock poisoned");
        let progress = root.feed_progress(&self.category, group.as_str());
        if position.get() == progress.cursor {
            return Ok(());
        }
        if position.get() < progress.cursor {
            // The guard just failed, so the pair is a genuine
            // regression and the constructor cannot reject it.
            return Err(AckError::Regression(
                Regression::new(FeedCursor::at(progress.cursor), position)
                    .expect("the position was just checked below the cursor"),
            ));
        }
        if position.get() > progress.delivered_to {
            // The guard just failed, so the position is genuinely past
            // delivery and the constructor cannot reject it.
            return Err(AckError::NotDelivered(
                Undelivered::new(DeliveredWatermark::at(progress.delivered_to), position)
                    .expect("the position was just checked past delivery"),
            ));
        }
        root.advance_cursor(&self.category, group.as_str(), position.get());
        Ok(())
    }
}

#[cfg(all(test, feature = "in_memory"))]
mod tests {
    use super::*;
    use crate::streams::feed::spec::{
        ack_is_monotonic, ack_rejects_undelivered, delivery_carries_the_envelope,
        poll_pages_the_backlog, poll_redelivers_until_acked,
    };

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Noted;

    impl Event for Noted {
        type EntityId = ();

        fn event_type(&self) -> String {
            "Noted".to_owned()
        }

        fn get_id(&self) -> Self::EntityId {}
    }

    /// A fresh database per case, per the suite's fresh-store
    /// assumption: count assertions read the whole category tail.
    fn pair() -> (
        super::super::InMemoryEventStreams<Noted>,
        InMemoryEventFeed<Noted>,
    ) {
        let db = InMemoryDatabase::new();
        let writer = db.category::<Noted>("feed").expect("fresh category");
        let feed = db.feed::<Noted>("feed").expect("claimed category");
        (writer, feed)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_poll_redelivers_until_acked() {
        let (writer, feed) = pair();
        poll_redelivers_until_acked(writer, feed, str::to_owned, || Noted).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_poll_pages_the_backlog() {
        let (writer, feed) = pair();
        poll_pages_the_backlog(writer, feed, str::to_owned, || Noted).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_ack_is_monotonic() {
        let (writer, feed) = pair();
        ack_is_monotonic(writer, feed, str::to_owned, || Noted).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_ack_rejects_undelivered() {
        let (writer, feed) = pair();
        ack_rejects_undelivered(writer, feed, str::to_owned, || Noted).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_delivery_carries_the_envelope() {
        let (writer, feed) = pair();
        delivery_carries_the_envelope(writer, feed, str::to_owned, || Noted).await;
    }
}
