//! The event feed (ADR 0010, post-pivot shape): consumer groups with
//! durable cursors over the committed log, at-least-once delivery,
//! and monotonic acknowledgement.
//!
//! The gate-S spike measured the allocation ledger's cost at 4.444x
//! p99 on the single-append path against a pre-registered 2x limit,
//! so the wave pivoted to the single-writer fallback the record names:
//! every event transaction funnels through one serialized writer,
//! which makes insert order commit order by construction. The cursor
//! is therefore a plain maximum over `global_sequence` - no ledger
//! ranges, no reaper, no prefix machinery. A committed event below
//! the log's maximum is always visible (nothing may be in flight
//! below a committed value under one writer), so a cursor that reads
//! `WHERE global_sequence > cursor` can neither skip a committed
//! event nor wait on a hole an abort burned permanently.
//!
//! Delivery is at-least-once: a crash between delivery and
//! acknowledgement replays, and idempotency is the consumer's
//! documented obligation. v1 pins ONE active poller per group - the
//! Axon tracking-processor model; concurrent pollers per group are a
//! later, separately chartered extension.

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

    /// The position one past this watermark: the first position a
    /// poll delivers. `START` yields position 1, which is nonzero by
    /// the log's own numbering.
    pub fn next_position(self) -> FeedPosition {
        // The cursor is a count of acknowledged log positions, so the
        // next position is cursor + 1 and can never be zero.
        FeedPosition::new(self.0 + 1).expect("cursor + 1 is nonzero")
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

    /// The group's name.
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
    /// Assemble an entry.
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

    /// Consume the entry into its parts.
    pub fn into_parts(self) -> (FeedPosition, StreamRef, RecordedEvent<E>) {
        (self.position, self.stream, self.record)
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
    #[error("ack at {attempted:?} would regress the cursor from {cursor:?}")]
    Regression {
        /// The group's current watermark.
        cursor: FeedCursor,
        /// The position the caller tried to acknowledge.
        attempted: FeedPosition,
    },
    /// The acked position has not been delivered to this group, so
    /// acknowledging it would advance the cursor past unread entries.
    #[error("ack at {attempted:?} precedes the group's delivered watermark {delivered_to:?}")]
    NotDelivered {
        /// The highest position this group has been delivered.
        delivered_to: FeedCursor,
        /// The position the caller tried to acknowledge.
        attempted: FeedPosition,
    },
    /// The backend failed before the protocol check could decide.
    #[error(transparent)]
    Backend(#[from] E),
}

/// Delivery over the committed log for consumer groups (ADR 0010).
/// Native async-fn-in-trait under the same `Send` rewrite the streams
/// trait uses.
///
/// v1 pins ONE active poller per group: an implementation may assume
/// it, and a deployment that violates it gets at-least-once delivery
/// at worst - the cursor still never advances past acknowledged
/// positions. Concurrent pollers per group are a later, separately
/// chartered extension.
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

    /// Advance the group's cursor to `position`. The position must
    /// have been delivered to this group, and the cursor never moves
    /// backwards; an ack of an already-acknowledged position is a
    /// silent no-op.
    async fn ack(
        &self,
        group: &ConsumerGroup,
        position: FeedPosition,
    ) -> Result<(), AckError<Self::Error>>;
}
