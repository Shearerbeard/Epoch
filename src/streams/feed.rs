//! The event feed (ADR 0010, post-pivot shape): consumer groups with
//! durable cursors over the committed log, at-least-once delivery,
//! and monotonic acknowledgement.
//!
//! Every event transaction funnels through one serialized writer, so
//! insert order is commit order by construction and the cursor is a
//! plain maximum over `global_sequence`. A committed event below the
//! log's maximum is always visible (nothing may be in flight below a
//! committed value under one writer), so a cursor that reads
//! `WHERE global_sequence > cursor` can neither skip a committed
//! event nor wait on a hole an abort burned permanently.
//!
//! Delivery is at-least-once: a crash between delivery and
//! acknowledgement replays, and idempotency is the consumer's
//! documented obligation. v1 assumes ONE active poller per group.

use std::num::NonZeroU64;
use std::num::NonZeroUsize;

use thiserror::Error;

use super::batch::StreamRef;
use super::RecordedEvent;

pub mod spec;

/// The position of one event in the committed log. Positions are the
/// `global_sequence` values, 1-based and nonzero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FeedPosition(NonZeroU64);

impl FeedPosition {
    /// Parse a raw committed position; zero is not a position.
    pub fn new(raw: u64) -> Result<Self, ZeroPosition> {
        NonZeroU64::new(raw).map(Self).ok_or(ZeroPosition)
    }

    /// The raw 1-based position.
    pub fn get(self) -> u64 {
        self.0.get()
    }
}

/// Zero arrived where a 1-based committed position was required.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("zero is not a 1-based feed position")]
pub struct ZeroPosition;

/// A consumer group's durable watermark: the highest position the
/// group has acknowledged. `START` (zero) is the group's state before
/// it acknowledges anything. Monotonicity is a store-enforced rule,
/// not a property of the value: an ack that would move the cursor
/// backwards is rejected by the feed, never represented as a new
/// cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FeedCursor(u64);

impl FeedCursor {
    /// The cursor of a group that has acknowledged nothing.
    pub const START: Self = Self(0);

    /// A watermark value. Any u64 is a legal cursor value; the
    /// monotonic rule is enforced where cursors move (the feed's
    /// ack), not where they are read.
    pub fn at(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw watermark; zero means nothing acknowledged yet.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// The highest position a group has been delivered. A different
/// watermark from the ack cursor: the two diverge exactly in the
/// at-least-once window between delivery and acknowledgement, which
/// is why they are different types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeliveredWatermark(u64);

impl DeliveredWatermark {
    /// The watermark of a group that has been delivered nothing.
    pub const NONE: Self = Self(0);

    /// A watermark value. Any u64 is legal; the delivery rule is
    /// enforced where the watermark moves (the feed's poll), not
    /// where it is read.
    pub fn at(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw watermark; zero means nothing delivered yet.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// A consumer group's identity. Non-empty, because a group with no
/// name cannot be distinguished from a forgotten argument.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConsumerGroup(String);

impl ConsumerGroup {
    /// Parse a group name; the empty string is not a group.
    pub fn new(name: impl Into<String>) -> Result<Self, EmptyGroupName> {
        let name = name.into();
        if name.is_empty() {
            Err(EmptyGroupName)
        } else {
            Ok(Self(name))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A consumer group was constructed with the empty string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("a consumer group name must not be empty")]
pub struct EmptyGroupName;

/// How many entries one poll may deliver. A poll of zero delivers
/// nothing and is rejected at construction rather than reaching the
/// backend as a no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PollLimit(NonZeroUsize);

impl PollLimit {
    /// Parse a poll limit; zero is not a limit.
    pub fn new(raw: usize) -> Result<Self, ZeroPollLimit> {
        NonZeroUsize::new(raw).map(Self).ok_or(ZeroPollLimit)
    }

    /// The raw limit.
    pub fn get(self) -> usize {
        self.0.get()
    }
}

/// Zero arrived where a nonzero poll limit was required.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("a poll limit must deliver at least one entry")]
pub struct ZeroPollLimit;

/// One delivered event: where it sits in the committed log, the
/// stream it belongs to, and the record itself - event plus the
/// envelope riding it end to end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedEntry<E> {
    position: FeedPosition,
    stream: StreamRef,
    record: RecordedEvent<E>,
}

impl<E> FeedEntry<E> {
    pub fn new(position: FeedPosition, stream: StreamRef, record: RecordedEvent<E>) -> Self {
        Self {
            position,
            stream,
            record,
        }
    }

    /// The entry's committed position.
    pub fn position(&self) -> FeedPosition {
        self.position
    }

    /// The stream the event was appended to.
    pub fn stream(&self) -> &StreamRef {
        &self.stream
    }

    /// The delivered record.
    pub fn record(&self) -> &RecordedEvent<E> {
        &self.record
    }

    pub fn into_parts(self) -> (FeedPosition, StreamRef, RecordedEvent<E>) {
        (self.position, self.stream, self.record)
    }
}

/// A rejected backwards ack. Constructible only when the attempted
/// position sits strictly below the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Regression {
    cursor: FeedCursor,
    attempted: FeedPosition,
}

impl Regression {
    /// Build the rejection; refuses pairs that are not regressions -
    /// the cursor's own position is a silent no-op, and anything
    /// above it is not a regression.
    pub fn new(cursor: FeedCursor, attempted: FeedPosition) -> Result<Self, NotARegression> {
        if attempted.get() < cursor.get() {
            Ok(Self { cursor, attempted })
        } else {
            Err(NotARegression)
        }
    }

    /// The group's watermark the ack would have regressed.
    pub fn cursor(&self) -> FeedCursor {
        self.cursor
    }

    /// The position the caller tried to acknowledge.
    pub fn attempted(&self) -> FeedPosition {
        self.attempted
    }
}

/// An ack rejection was requested for a pair that is not a
/// regression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("the attempted position is not below the cursor; not a regression")]
pub struct NotARegression;

impl std::fmt::Display for Regression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ack at {} would regress the cursor from {}",
            self.attempted.get(),
            self.cursor.get()
        )
    }
}

/// A rejected ack naming a position past the group's delivered
/// watermark. Constructible only when the attempted position was
/// never delivered that far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Undelivered {
    delivered_to: DeliveredWatermark,
    attempted: FeedPosition,
}

impl Undelivered {
    /// Build the rejection; refuses pairs at or below the delivered
    /// watermark, which are legal watermark movement (positions of
    /// other categories included).
    pub fn new(
        delivered_to: DeliveredWatermark,
        attempted: FeedPosition,
    ) -> Result<Self, AlreadyDelivered> {
        if attempted.get() > delivered_to.get() {
            Ok(Self {
                delivered_to,
                attempted,
            })
        } else {
            Err(AlreadyDelivered)
        }
    }

    /// The highest position this group was delivered.
    pub fn delivered_to(&self) -> DeliveredWatermark {
        self.delivered_to
    }

    /// The position the caller tried to acknowledge.
    pub fn attempted(&self) -> FeedPosition {
        self.attempted
    }
}

/// An ack rejection was requested for a position within the group's
/// delivered watermark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("the attempted position is within the delivered watermark; not an undelivered ack")]
pub struct AlreadyDelivered;

impl std::fmt::Display for Undelivered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ack at {} exceeds the group's delivered watermark {}",
            self.attempted.get(),
            self.delivered_to.get()
        )
    }
}

/// Why an acknowledgement was rejected. Both rejections are protocol
/// violations a caller fixes, not backend faults it retries.
#[derive(Debug, Error)]
pub enum AckError<E>
where
    E: std::error::Error,
{
    /// The acked position is below the group's cursor. Cursors are
    /// monotonic: an ack of the position the cursor already sits at
    /// is a silent no-op, and a regression below it is rejected.
    #[error("{0}")]
    Regression(Regression),
    /// The acked position is past the group's delivered watermark, so
    /// acknowledging it would advance the cursor past unread entries.
    #[error("{0}")]
    NotDelivered(Undelivered),
    /// The backend failed before the protocol check could decide.
    #[error(transparent)]
    Backend(#[from] E),
}

/// Delivery over the committed log for consumer groups (ADR 0010).
///
/// v1 assumes ONE active poller per group: an implementation may rely
/// on it, and a deployment that violates it gets at-least-once
/// delivery at worst - the cursor still never advances past
/// acknowledged positions.
#[trait_variant::make(Send)]
pub trait EventFeed<E>
where
    E: Send + Sync + std::fmt::Debug,
{
    /// Backend failure type wrapped by [`AckError::Backend`].
    type Error: std::error::Error + Send + Sync;

    /// Deliver up to `limit` entries after the group's cursor, oldest
    /// first. Undelivered entries reappear on later polls until
    /// acknowledged: delivery is at-least-once, and idempotency is
    /// the consumer's obligation.
    async fn poll(
        &self,
        group: &ConsumerGroup,
        limit: PollLimit,
    ) -> Result<Vec<FeedEntry<E>>, Self::Error>;

    /// Advance the group's cursor to `position`. The cursor never
    /// moves backwards: an ack of the position the cursor already
    /// sits at is a silent no-op, and an ack below it is rejected as
    /// [`AckError::Regression`]. The position must not exceed the
    /// group's delivered watermark - [`AckError::NotDelivered`] -
    /// though positions of other categories at or below that
    /// watermark are legal watermark movement, not a skip.
    async fn ack(
        &self,
        group: &ConsumerGroup,
        position: FeedPosition,
    ) -> Result<(), AckError<Self::Error>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position(raw: u64) -> FeedPosition {
        FeedPosition::new(raw).expect("test positions are nonzero")
    }

    #[test]
    fn regression_requires_a_strictly_lower_attempt() {
        let cursor = FeedCursor::at(5);
        assert!(Regression::new(cursor, position(4)).is_ok());
        assert!(Regression::new(cursor, position(5)).is_err());
        assert!(Regression::new(cursor, position(6)).is_err());
    }

    #[test]
    fn undelivered_requires_an_attempt_past_the_watermark() {
        let delivered = DeliveredWatermark::at(5);
        assert!(Undelivered::new(delivered, position(6)).is_ok());
        assert!(Undelivered::new(delivered, position(5)).is_err());
        assert!(Undelivered::new(delivered, position(4)).is_err());
    }
}
