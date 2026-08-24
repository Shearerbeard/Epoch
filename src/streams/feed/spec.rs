//! Generic feed conformance cases over any [`EventFeed`]
//! implementation paired with an [`EventStreams`] writer into the same
//! store (the E11 pattern: backends wire their own test modules
//! through these cases unchanged).
//!
//! Every case generates a unique consumer group per iteration (a ULID
//! nonce in the group name), so preconditions hold on any backend
//! without assuming storage can be cleared. Cases pair a writer and a
//! feed view over one store: the writer appends through the ordinary
//! streams surface, the feed delivers through the polling surface,
//! and the cases pin the at-least-once and monotonic-ack contract
//! ADR 0010 states.

use std::fmt::Debug;

use crate::decider::Event;

use super::super::spec::under_deadline;
use super::super::{EventBatch, EventMetadata, EventStreams, ExpectedVersion, RecordedEvent};
use super::{AckError, ConsumerGroup, EventFeed, FeedCursor, FeedPosition, PollLimit};

/// A unique group name per call, so runs never collide.
fn unique_group(prefix: &str) -> ConsumerGroup {
    // Group names are non-empty by construction here, so the parse
    // error is unreachable.
    ConsumerGroup::new(format!("{prefix}-{}", rusty_ulid::generate_ulid_string()))
        .expect("a ULID nonce names a non-empty group")
}

/// A unique stream id per call, so a wiring that shares a store with
/// other cases never has its version expectations perturbed.
fn unique_stream_id<Id>(prefix: &str, make_id: impl Fn(&str) -> Id) -> Id {
    make_id(&format!("{prefix}-{}", rusty_ulid::generate_ulid_string()))
}

fn limit(raw: usize) -> PollLimit {
    // The case constants are nonzero, so the zero-limit error is
    // unreachable.
    PollLimit::new(raw).expect("case limits are nonzero")
}

/// Undelivered entries reappear until acknowledged, and an
/// acknowledged position never reappears: at-least-once delivery with
/// a monotonic cursor, the whole contract in one case.
pub async fn poll_redelivers_until_acked<S, F, E>(
    writer: S,
    feed: F,
    make_id: impl Fn(&str) -> S::Id,
    make_event: impl Fn() -> E,
) where
    S: EventStreams<E> + Clone + Send + Sync + 'static,
    S::Id: Clone + Send + Sync + 'static,
    F: EventFeed<E> + Clone + Send + Sync + 'static,
    E: Event + Clone + PartialEq + Send + Sync + Debug + 'static,
{
    under_deadline(async {
        let group = unique_group("redeliver");
        let stream = unique_stream_id("feed-spec-redeliver", &make_id);
        writer
            .append(
                ExpectedVersion::NoStream,
                &stream,
                &EventBatch::new(vec![make_event(), make_event()])
                    .expect("two events are nonempty"),
            )
            .await
            .expect("seed append");

        let first = feed
            .poll(&group, limit(10))
            .await
            .expect("first poll delivers");
        assert_eq!(first.len(), 2, "a fresh group reads the whole log tail");

        let again = feed
            .poll(&group, limit(10))
            .await
            .expect("second poll delivers");
        assert_eq!(
            again, first,
            "without an ack the same entries redeliver: at-least-once"
        );

        let highest = first.last().expect("entries are nonempty").position();
        feed.ack(&group, highest)
            .await
            .expect("ack the delivered tip");

        let caught_up = feed
            .poll(&group, limit(10))
            .await
            .expect("third poll delivers");
        assert!(
            caught_up.is_empty(),
            "an acked position never reappears: {caught_up:?}"
        );
    })
    .await;
}

/// A poll limit smaller than the backlog delivers a prefix, and the
/// remainder follows on the next poll: pagination inside delivery.
pub async fn poll_pages_the_backlog<S, F, E>(
    writer: S,
    feed: F,
    make_id: impl Fn(&str) -> S::Id,
    make_event: impl Fn() -> E,
) where
    S: EventStreams<E> + Clone + Send + Sync + 'static,
    S::Id: Clone + Send + Sync + 'static,
    F: EventFeed<E> + Clone + Send + Sync + 'static,
    E: Event + Clone + PartialEq + Send + Sync + Debug + 'static,
{
    under_deadline(async {
        let group = unique_group("pages");
        let stream = unique_stream_id("feed-spec-pages", &make_id);
        writer
            .append(
                ExpectedVersion::NoStream,
                &stream,
                &EventBatch::new(vec![make_event(), make_event(), make_event(), make_event()])
                    .expect("four events are nonempty"),
            )
            .await
            .expect("seed append");

        let page_one = feed
            .poll(&group, limit(2))
            .await
            .expect("first page delivers");
        assert_eq!(page_one.len(), 2, "the limit bounds the page");

        let tip = page_one.last().expect("the page is nonempty").position();
        feed.ack(&group, tip).await.expect("ack the page tip");

        let page_two = feed
            .poll(&group, limit(2))
            .await
            .expect("second page delivers");
        assert_eq!(page_two.len(), 2, "the remainder follows");
        assert!(
            page_two.first().expect("the page is nonempty").position() > tip,
            "pages continue after the acked tip"
        );
    })
    .await;
}

/// An ack below the cursor is rejected with the cursor it would have
/// regressed; an ack of the position the cursor already sits at is a
/// silent no-op.
pub async fn ack_is_monotonic<S, F, E>(
    writer: S,
    feed: F,
    make_id: impl Fn(&str) -> S::Id,
    make_event: impl Fn() -> E,
) where
    S: EventStreams<E> + Clone + Send + Sync + 'static,
    S::Id: Clone + Send + Sync + 'static,
    F: EventFeed<E> + Clone + Send + Sync + 'static,
    E: Event + Clone + PartialEq + Send + Sync + Debug + 'static,
{
    under_deadline(async {
        let group = unique_group("monotonic");
        let stream = unique_stream_id("feed-spec-monotonic", &make_id);
        writer
            .append(
                ExpectedVersion::NoStream,
                &stream,
                &EventBatch::new(vec![make_event(), make_event()])
                    .expect("two events are nonempty"),
            )
            .await
            .expect("seed append");

        let entries = feed.poll(&group, limit(10)).await.expect("poll delivers");
        let low = entries.first().expect("entries are nonempty").position();
        let high = entries.last().expect("entries are nonempty").position();

        feed.ack(&group, high).await.expect("ack the tip");
        feed.ack(&group, high)
            .await
            .expect("re-acking the cursor position is a no-op");

        match feed.ack(&group, low).await {
            Err(AckError::Regression { cursor, attempted }) => {
                assert_eq!(cursor, FeedCursor::at(high.get()));
                assert_eq!(attempted, low);
            }
            other => panic!("expected a regression rejection, got {other:?}"),
        }
    })
    .await;
}

/// An ack naming a position the group was never delivered is
/// rejected: the cursor cannot advance past unread entries.
pub async fn ack_rejects_undelivered<S, F, E>(
    writer: S,
    feed: F,
    make_id: impl Fn(&str) -> S::Id,
    make_event: impl Fn() -> E,
) where
    S: EventStreams<E> + Clone + Send + Sync + 'static,
    S::Id: Clone + Send + Sync + 'static,
    F: EventFeed<E> + Clone + Send + Sync + 'static,
    E: Event + Clone + PartialEq + Send + Sync + Debug + 'static,
{
    under_deadline(async {
        let group = unique_group("undelivered");
        let stream = unique_stream_id("feed-spec-undelivered", &make_id);
        writer
            .append(
                ExpectedVersion::NoStream,
                &stream,
                &EventBatch::new(vec![make_event(), make_event(), make_event()])
                    .expect("three events are nonempty"),
            )
            .await
            .expect("seed append");

        let page = feed
            .poll(&group, limit(2))
            .await
            .expect("a bounded poll delivers");
        assert_eq!(page.len(), 2, "one entry stays undelivered");

        // Ack something this group never saw. Any position past the
        // delivered page is undelivered; position arithmetic keeps the
        // case independent of the log's tail.
        let undelivered =
            FeedPosition::new(page.last().expect("page is nonempty").position().get() + 1)
                .expect("the position after the page is nonzero");
        match feed.ack(&group, undelivered).await {
            Err(AckError::NotDelivered { delivered_to, .. }) => {
                assert_eq!(
                    delivered_to,
                    FeedCursor::START,
                    "nothing was acked, so the delivered watermark stands at start"
                );
            }
            other => panic!("expected a not-delivered rejection, got {other:?}"),
        }
    })
    .await;
}

/// The envelope written with an event is the envelope delivered with
/// it, and positions ascend across deliveries: the keyed seam's read
/// side.
pub async fn delivery_carries_the_envelope<S, F, E>(
    writer: S,
    feed: F,
    make_id: impl Fn(&str) -> S::Id,
    make_event: impl Fn() -> E,
) where
    S: EventStreams<E> + Clone + Send + Sync + 'static,
    S::Id: Clone + Send + Sync + 'static,
    F: EventFeed<E> + Clone + Send + Sync + 'static,
    E: Event + Clone + PartialEq + Send + Sync + Debug + 'static,
{
    under_deadline(async {
        let group = unique_group("envelope");
        let mut envelope = EventMetadata::new();
        envelope.insert("intent", "saga-1/order-5/2");
        let stream = unique_stream_id("feed-spec-envelope", &make_id);
        writer
            .append(
                ExpectedVersion::NoStream,
                &stream,
                &EventBatch::from_records(vec![
                    RecordedEvent::new(make_event()),
                    RecordedEvent::keyed(make_event(), envelope),
                ])
                .expect("two records are nonempty"),
            )
            .await
            .expect("seed append");

        let entries = feed.poll(&group, limit(10)).await.expect("poll delivers");
        assert_eq!(entries.len(), 2);
        assert!(
            entries[0].position() < entries[1].position(),
            "delivery order is commit order"
        );
        assert!(
            entries[0].record().metadata().is_empty(),
            "a bare event delivers with the empty envelope"
        );
        assert_eq!(
            entries[1].record().metadata().get("intent"),
            Some("saga-1/order-5/2"),
            "the keyed envelope rides delivery"
        );
    })
    .await;
}
