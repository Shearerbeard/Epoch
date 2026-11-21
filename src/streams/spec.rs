//! Generic spec suite over any [`EventStreams`] implementation
//! (ADR 0003). Backends wire their own test modules through these
//! cases unchanged; the suite is public so consumers can pin their own
//! wiring (chore-lottery's gate-M test) against the same contract.
//!
//! Every case generates a unique stream id per configuration and per
//! iteration (a ULID nonce in the id), so preconditions hold on any
//! backend without assuming storage can be cleared. Contenders run as
//! spawned tasks on a multi-thread runtime: each loads state,
//! synchronizes on a shared [`tokio::sync::Barrier`], then appends, so
//! the optimistic-concurrency collision is deterministic even for
//! backends with real I/O; the iteration loop is defense in depth,
//! never the mechanism. Cases take the implementation by value and
//! clone it per contender, per the shared-store `Clone` contract
//! (ADR 0005): clones contend on one store.

use std::fmt::Debug;
use std::sync::Arc;

use tokio::sync::Barrier;

use crate::decider::Event;

use super::{
    AppendError, EventBatch, EventMetadata, EventStreams, ExpectedVersion, RecordedEvent,
    StreamSequence, StreamState, StreamVersion,
};

const RACE_ITERATIONS: usize = 20;

/// Flash-sale contenders: at least 8, strictly more than the stock.
const FLASH_SALE_CONTENDERS: usize = 8;

/// Flash-sale stock: strictly less than the contender count.
const FLASH_SALE_STOCK: usize = 3;

/// Every case runs under a deadline so a hang - a contender parked
/// on the barrier after a rival panicked, or a retry loop that a
/// broken backend refuses to release - fails with a diagnosis
/// instead of stalling the suite.
const CASE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

/// Deadline wrapper for every backend's wiring of the cases below.
pub async fn under_deadline(case: impl std::future::Future<Output = ()>) {
    tokio::time::timeout(CASE_DEADLINE, case)
        .await
        .expect("spec case must finish under the deadline");
}

/// Two contenders observe the same never-used stream, release together,
/// and each appends one event expecting the empty stream: exactly one
/// wins at sequence 1 and one gets the version conflict. A post-race
/// load pins the stored state: exactly one event, so version 1.
pub async fn single_event_occ_race_on_empty_stream<S, E>(
    store: S,
    make_id: impl Fn(&str) -> S::Id,
    make_event: impl Fn() -> E + Clone + Send + Sync + 'static,
) where
    S: EventStreams<E> + Clone + Send + Sync + 'static,
    S::Id: Clone + Send + Sync + 'static,
    E: Event + Send + Sync + Debug + 'static,
{
    for iteration in 0..RACE_ITERATIONS {
        let id = make_id(&unique_stream_id("occ-empty", iteration));
        let barrier = Arc::new(Barrier::new(2));

        let contender = || {
            let store = store.clone();
            let id = id.clone();
            let barrier = Arc::clone(&barrier);
            let make_event = make_event.clone();
            tokio::spawn(async move {
                let state = store.load_stream(&id).await.expect("load succeeds");
                assert!(
                    matches!(state, StreamState::Missing),
                    "a never-used stream must load Missing"
                );
                barrier.wait().await;
                store
                    .append(
                        ExpectedVersion::NoStream,
                        &id,
                        &one_event_batch(make_event()),
                    )
                    .await
            })
        };

        let (a, b) = (contender(), contender());
        let a = a.await.expect("contender task must not panic");
        let b = b.await.expect("contender task must not panic");
        assert_exactly_one_winner(a, b, 1);
        assert_stored_event_count(&store, &id, 1).await;
    }
}

/// One event is appended first; two contenders observe the one-event
/// stream, release together, and each appends one event at the observed
/// version: exactly one wins at sequence 2 and one gets the version
/// conflict. This is the other side of the old backend's defect, where
/// an empty stream and a one-event stream shared version 0. A post-race
/// load pins the stored state: exactly two events, so version 2.
pub async fn single_event_occ_race_on_seeded_stream<S, E>(
    store: S,
    make_id: impl Fn(&str) -> S::Id,
    make_event: impl Fn() -> E + Clone + Send + Sync + 'static,
) where
    S: EventStreams<E> + Clone + Send + Sync + 'static,
    S::Id: Clone + Send + Sync + 'static,
    E: Event + Send + Sync + Debug + 'static,
{
    for iteration in 0..RACE_ITERATIONS {
        let id = make_id(&unique_stream_id("occ-seeded", iteration));
        let seeded = store
            .append(
                ExpectedVersion::NoStream,
                &id,
                &one_event_batch(make_event()),
            )
            .await
            .expect("seed append succeeds");
        assert_eq!(seeded.get(), 1, "the seed lands at sequence 1");

        let barrier = Arc::new(Barrier::new(2));

        let contender = || {
            let store = store.clone();
            let id = id.clone();
            let barrier = Arc::clone(&barrier);
            let make_event = make_event.clone();
            tokio::spawn(async move {
                let observed = observed_sequence(&store, &id).await;
                assert_eq!(observed.get(), 1, "contenders observe the one-event stream");
                barrier.wait().await;
                store
                    .append(
                        ExpectedVersion::Exact(observed),
                        &id,
                        &one_event_batch(make_event()),
                    )
                    .await
            })
        };

        let (a, b) = (contender(), contender());
        let a = a.await.expect("contender task must not panic");
        let b = b.await.expect("contender task must not panic");
        assert_exactly_one_winner(a, b, 2);
        assert_stored_event_count(&store, &id, 2).await;
    }
}

/// The flash-sale antagonistic case: N contenders race over stock K
/// through an explicit contend-retry protocol, since a single barrier
/// round permits exactly one OCC winner by design. The stream is seeded
/// with one non-sale event so the opening collision lands on the
/// one-event version the old defect conflated; contenders count sold as
/// events beyond the seed. Each contender loops: load, stop if sold has
/// reached the stock, otherwise append one sale at the observed version
/// and stop on `Ok`, reloading on version conflict. The first attempt
/// of every contender releases on a shared barrier so the opening round
/// collides deterministically; retries contend naturally, capped per
/// contender so a conflict-happy implementation fails loudly instead of
/// hanging the suite. Exactly K sale appends succeed and the final
/// stream holds exactly K + 1 events (the seed plus K sales): a
/// lost-update implementation overshoots, an append that stores its
/// event while reporting a conflict undershoots, and an implementation
/// that conflicts on a correct expectation exhausts the retry cap.
pub async fn flash_sale_sells_exactly_the_stock<S, E>(
    store: S,
    make_id: impl Fn(&str) -> S::Id,
    make_event: impl Fn() -> E + Clone + Send + Sync + 'static,
) where
    S: EventStreams<E> + Clone + Send + Sync + 'static,
    S::Id: Clone + Send + Sync + 'static,
    E: Event + Send + Sync + Debug + 'static,
{
    const {
        assert!(FLASH_SALE_CONTENDERS >= 8, "the case requires N >= 8");
        assert!(
            FLASH_SALE_STOCK < FLASH_SALE_CONTENDERS,
            "stock must be oversubscribed"
        );
    }

    for iteration in 0..RACE_ITERATIONS {
        let id = make_id(&unique_stream_id("flash-sale", iteration));
        let seeded = store
            .append(
                ExpectedVersion::NoStream,
                &id,
                &one_event_batch(make_event()),
            )
            .await
            .expect("seed append succeeds");
        assert_eq!(seeded.get(), 1, "the non-sale seed lands at sequence 1");

        let barrier = Arc::new(Barrier::new(FLASH_SALE_CONTENDERS));
        let contenders: Vec<_> = (0..FLASH_SALE_CONTENDERS)
            .map(|_| {
                let store = store.clone();
                let id = id.clone();
                let barrier = Arc::clone(&barrier);
                let make_event = make_event.clone();
                tokio::spawn(async move {
                    let mut first_attempt = true;
                    // A correct implementation conflicts a contender at
                    // most once per rival sale, so K + 1 retries is
                    // already generous; past this cap the backend is
                    // conflicting on correct expectations and the case
                    // fails loudly rather than spinning.
                    let mut retries_left = FLASH_SALE_STOCK + 2;
                    loop {
                        let observed = observed_sequence(&store, &id).await;
                        // Sold count: events beyond the non-sale seed.
                        let sold = (observed.get() - 1) as usize;
                        if sold >= FLASH_SALE_STOCK {
                            return false;
                        }
                        if first_attempt {
                            // Nothing is sold before the barrier opens,
                            // so every contender reaches this wait: the
                            // opening round collides deterministically.
                            barrier.wait().await;
                            first_attempt = false;
                        }
                        match store
                            .append(
                                ExpectedVersion::Exact(observed),
                                &id,
                                &one_event_batch(make_event()),
                            )
                            .await
                        {
                            Ok(_) => return true,
                            Err(AppendError::Conflict(_)) => {
                                retries_left = retries_left.checked_sub(1).expect(
                                    "retry cap exhausted: the implementation \
                                     conflicts on correct expectations",
                                );
                            }
                            Err(AppendError::LockTimeout(_)) => {
                                // The funnel is contended by design; the
                                // sale retries on its next loop turn, the
                                // same response a batch path gives a
                                // retryable timeout.
                            }
                            Err(AppendError::Backend(error)) => {
                                panic!("backend failure mid-sale: {error:?}")
                            }
                        }
                    }
                })
            })
            .collect();

        let mut sales = 0_usize;
        for contender in contenders {
            if contender.await.expect("contender task must not panic") {
                sales += 1;
            }
        }
        assert_eq!(
            sales, FLASH_SALE_STOCK,
            "exactly the stock count of sale appends succeeds"
        );
        assert_stored_event_count(&store, &id, FLASH_SALE_STOCK as u64 + 1).await;
    }
}

/// A keyed append's stored envelope is the envelope both load paths
/// hand back: the full load returns the batch intact and the slice
/// carries the same records. A backend must not rebuild a loaded
/// record with an empty envelope when storage holds a keyed one.
pub async fn a_keyed_append_loads_back_with_its_envelope<S, E>(
    store: S,
    make_id: impl Fn(&str) -> S::Id,
    make_event: impl Fn() -> E,
) where
    S: EventStreams<E>,
    E: Event + Clone + PartialEq + Send + Sync + Debug + 'static,
{
    let id = make_id(&unique_stream_id("envelope-load", 0));
    let mut envelope = EventMetadata::new();
    envelope.insert("intent", "saga-1/order-5/2");
    let batch = EventBatch::from_records(vec![RecordedEvent::keyed(make_event(), envelope)])
        .expect("one event is nonempty");
    store
        .append(ExpectedVersion::NoStream, &id, &batch)
        .await
        .expect("append succeeds");

    assert_eq!(
        store.load_stream(&id).await.expect("load succeeds"),
        StreamState::Present(batch.clone()),
        "the full load returns the stored envelope"
    );
    let slice = store
        .load_stream_from(&id, None)
        .await
        .expect("slice load succeeds");
    assert_eq!(
        slice.records(),
        batch.records(),
        "the slice carries the stored envelope"
    );
}

/// Checks inclusive tail reads, missing streams and past-tail cursors.
/// A cursor above `i64::MAX` exercises clamped binds at a small head;
/// this case does not test the filter against a stored `i64::MAX` row.
pub async fn a_slice_reads_from_the_cursor_to_the_head<S, E>(
    store: S,
    make_id: impl Fn(&str) -> S::Id,
    make_event: impl Fn(u32) -> E,
) where
    S: EventStreams<E>,
    E: Event + PartialEq + Send + Sync + Debug,
{
    let id = make_id(&unique_stream_id("cursor-slice", 0));
    let batch = EventBatch::from_records(
        (1..=3)
            .map(|slot| {
                let mut envelope = EventMetadata::new();
                envelope.insert("slot", slot.to_string());
                RecordedEvent::keyed(make_event(slot), envelope)
            })
            .collect(),
    )
    .expect("three records are nonempty");
    store
        .append(ExpectedVersion::NoStream, &id, &batch)
        .await
        .expect("seed append succeeds");

    let whole = batch.records();
    let reads: [(Option<u64>, &[RecordedEvent<E>]); 5] = [
        (None, whole),
        (Some(1), whole),
        (Some(2), &whole[1..]),
        (Some(4), &[]),
        (Some(i64::MAX as u64 + 1), &[]),
    ];
    for (cursor, expected) in reads {
        let from = cursor
            .map(StreamSequence::new)
            .transpose()
            .expect("a cursor is a 1-based position");
        let slice = store
            .load_stream_from(&id, from)
            .await
            .expect("slice load succeeds");
        assert_eq!(
            (slice.at(), slice.records()),
            (head_at(3), expected),
            "the slice holds exactly the cursor's events, observed at the head (cursor {cursor:?})"
        );
    }

    let missing = make_id(&unique_stream_id("cursor-missing", 0));
    assert_eq!(
        store.load_stream(&missing).await.expect("load succeeds"),
        StreamState::Missing,
        "a never-written stream loads Missing, never an error"
    );
    let slice = store
        .load_stream_from(&missing, None)
        .await
        .expect("slice load succeeds");
    assert!(
        slice.records().is_empty(),
        "a slice over a missing stream carries nothing"
    );
    assert_eq!(
        slice.at(),
        StreamVersion::NoStream,
        "a missing stream is observed as NoStream"
    );
}

/// Checks returned ids and events in interleaved append order.
/// The supplied category must contain no other events.
pub async fn a_category_read_round_trips_ids_in_append_order<S, E>(
    store: S,
    make_id: impl Fn(&str) -> S::Id,
    make_event: impl Fn(u32) -> E,
) where
    S: EventStreams<E>,
    S::Id: PartialEq + Debug,
    E: Event + PartialEq + Send + Sync + Debug,
{
    let first = make_id(&unique_stream_id("category-first", 0));
    let second = make_id(&unique_stream_id("category-second", 0));

    let one = StreamSequence::new(1).expect("1 is a position");
    let appends = [
        (&first, ExpectedVersion::NoStream, make_event(1), 1_u64),
        (&second, ExpectedVersion::NoStream, make_event(2), 1),
        (&first, ExpectedVersion::Exact(one), make_event(3), 2),
    ];
    for (stream, expectation, event, lands_at) in appends {
        let landed = store
            .append(expectation, stream, &one_event_batch(event))
            .await
            .expect("the append succeeds");
        assert_eq!(
            landed.get(),
            lands_at,
            "the append lands at sequence {lands_at}"
        );
    }

    let category = store.load_category().await.expect("category read succeeds");
    let expected = [
        (&first, make_event(1)),
        (&second, make_event(2)),
        (&first, make_event(3)),
    ];
    assert_eq!(
        category.len(),
        expected.len(),
        "every appended event is returned exactly once"
    );
    for (index, (returned, (id, event))) in category.iter().zip(expected).enumerate() {
        assert_eq!(returned.id, *id, "returned id {index} is the appended id");
        assert_eq!(
            returned.event, event,
            "returned event {index} is the appended event"
        );
    }
}

/// Checks that slice records and head agree during a rival append.
/// The barrier only coordinates task starts. A split-read regression is
/// caught when the rival commits between the read's two statements.
pub async fn a_slice_read_during_a_rival_append_stays_one_view<S, E>(
    store: S,
    rival: S,
    make_id: impl Fn(&str) -> S::Id,
    make_event: impl Fn(u32) -> E + Clone + Send + Sync + 'static,
) where
    S: EventStreams<E> + Clone + Send + Sync + 'static,
    S::Id: Clone + Send + Sync + 'static,
    E: Event + PartialEq + Send + Sync + Debug + 'static,
{
    for iteration in 0..RACE_ITERATIONS {
        let id = make_id(&unique_stream_id("read-under-write", iteration));
        let seeded = store
            .append(
                ExpectedVersion::NoStream,
                &id,
                &EventBatch::new(vec![make_event(1), make_event(2)])
                    .expect("two events are nonempty"),
            )
            .await
            .expect("seed append succeeds");
        assert_eq!(seeded.get(), 2, "the seed lands at sequence 2");

        let barrier = Arc::new(Barrier::new(2));
        let cursor = StreamSequence::new(2).expect("2 is a position");
        let reader = {
            let store = store.clone();
            let id = id.clone();
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move {
                barrier.wait().await;
                store.load_stream_from(&id, Some(cursor)).await
            })
        };
        let writer = {
            let rival = rival.clone();
            let id = id.clone();
            let barrier = Arc::clone(&barrier);
            let make_event = make_event.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                rival
                    .append(
                        ExpectedVersion::Exact(cursor),
                        &id,
                        &one_event_batch(make_event(3)),
                    )
                    .await
            })
        };

        let slice = reader
            .await
            .expect("reader task must not panic")
            .expect("slice load succeeds");
        let appended = writer
            .await
            .expect("writer task must not panic")
            .expect("the rival's append succeeds");
        assert_eq!(appended.get(), 3, "the rival lands at sequence 3");

        let seed_tail = vec![RecordedEvent::new(make_event(2))];
        let raced_tail = vec![
            RecordedEvent::new(make_event(2)),
            RecordedEvent::new(make_event(3)),
        ];
        match (slice.at(), slice.records()) {
            (at, records) if records == seed_tail.as_slice() => {
                assert_eq!(at, head_at(2), "the seed tail is observed at the seed head");
            }
            (at, records) if records == raced_tail.as_slice() => {
                assert_eq!(
                    at,
                    head_at(3),
                    "the rival's tail is observed at the rival's head"
                );
            }
            (at, records) => {
                panic!("the slice's head and records disagree: {at:?} over {records:?}")
            }
        }

        assert_eq!(
            store.load_stream(&id).await.expect("load succeeds"),
            StreamState::Present(
                EventBatch::new(vec![make_event(1), make_event(2), make_event(3)])
                    .expect("three events are nonempty"),
            ),
            "the stream finally holds the seed and the rival's event"
        );
    }
}

/// Checks bounded page reads: exact endpoints, truncation by count and
/// by upper bound, and the empty pages a zero limit, a reversed
/// range, a past-head bound and a missing stream read. Unsigned bounds
/// past `i64::MAX` keep their arithmetic meaning, and every page still
/// observes the head while carrying the envelopes its appends stored.
pub async fn a_page_reads_exactly_the_requested_bounds<S, E>(
    store: S,
    make_id: impl Fn(&str) -> S::Id,
    make_event: impl Fn(u32) -> E,
) where
    S: EventStreams<E>,
    E: Event + PartialEq + Send + Sync + Debug,
{
    let id = make_id(&unique_stream_id("page-bounds", 0));
    let batch = EventBatch::from_records(
        (1..=5)
            .map(|slot| {
                let mut envelope = EventMetadata::new();
                envelope.insert("slot", slot.to_string());
                RecordedEvent::keyed(make_event(slot), envelope)
            })
            .collect(),
    )
    .expect("five records are nonempty");
    store
        .append(ExpectedVersion::NoStream, &id, &batch)
        .await
        .expect("seed append succeeds");

    let whole = batch.records();
    let reads: [ExpectedPage<'_, E>; 10] = [
        (seq(1)..=seq(5), 5, whole),
        // A count smaller than the range pages toward the range's end.
        (seq(1)..=seq(5), 3, &whole[..3]),
        // An upper bound smaller than the stored history truncates it.
        (seq(1)..=seq(3), 5, &whole[..3]),
        // Exact endpoints hold their single event.
        (seq(2)..=seq(2), 5, &whole[1..2]),
        // A zero limit observes the head without selecting a page.
        (seq(1)..=seq(5), 0, &[]),
        // A reversed range reads an empty page.
        (seq(4)..=seq(2), 5, &[]),
        // A bound past the head reads an empty page.
        (seq(6)..=seq(9), 5, &[]),
        // An upper bound and a count past what any backend can store
        // still read the whole stream.
        (seq(1)..=seq(u64::MAX), usize::MAX, whole),
        // An open-ended range pages by count alone.
        (seq(1)..=seq(u64::MAX), 2, &whole[..2]),
        // A lower bound past every storable position selects none.
        (seq(i64::MAX as u64 + 1)..=seq(u64::MAX), 5, &[]),
    ];
    for (index, (range, limit, expected)) in reads.into_iter().enumerate() {
        let page = store
            .load_stream_page(&id, range, limit)
            .await
            .expect("page load succeeds");
        assert_eq!(
            (page.at(), page.records()),
            (head_at(5), expected),
            "page {index} holds exactly its bounds' events, observed at the head"
        );
    }

    let missing = make_id(&unique_stream_id("page-missing", 0));
    let page = store
        .load_stream_page(&missing, seq(1)..=seq(5), 5)
        .await
        .expect("page load succeeds");
    assert!(
        page.records().is_empty(),
        "a page over a missing stream carries nothing"
    );
    assert_eq!(
        page.at(),
        StreamVersion::NoStream,
        "a missing stream is observed as NoStream"
    );
}

/// Checks consumer-shaped paging: a lagging projection pages a dense
/// backlog, carrying its requested upper bound on the first page and
/// the endpoint captured from that page's observation on every later
/// one. A rival append landing inside the originally requested range
/// but beyond the captured endpoint does not extend the replay: the
/// later pages observe the moved head yet return no event past the
/// endpoint. The dense-stream assumption is exercised as written - the
/// next page starts at the prior page's requested start plus its
/// returned count, never at the observed head.
pub async fn a_consumer_pages_a_backlog_to_a_captured_endpoint<S, E>(
    store: S,
    rival: S,
    make_id: impl Fn(&str) -> S::Id,
    make_event: impl Fn(u32) -> E,
) where
    S: EventStreams<E>,
    E: Event + PartialEq + Send + Sync + Debug,
{
    const REQUESTED_UPPER: u64 = 10;
    const PAGE_SIZE: usize = 3;
    const BACKLOG: u64 = 7;

    let id = make_id(&unique_stream_id("page-consumer", 0));
    let batch = EventBatch::from_records(
        (1..=BACKLOG)
            .map(|slot| {
                let mut envelope = EventMetadata::new();
                envelope.insert("slot", slot.to_string());
                RecordedEvent::keyed(
                    make_event(u32::try_from(slot).expect("a slot fits the event payload")),
                    envelope,
                )
            })
            .collect(),
    )
    .expect("the backlog is nonempty");
    store
        .append(ExpectedVersion::NoStream, &id, &batch)
        .await
        .expect("seed append succeeds");
    let backlog = batch.records();

    // The first page carries the requested upper bound, and its
    // observation fixes the replay endpoint.
    let first = store
        .load_stream_page(&id, seq(1)..=seq(REQUESTED_UPPER), PAGE_SIZE)
        .await
        .expect("page load succeeds");
    let endpoint = match first.at() {
        // The replay endpoint is the observed head, capped by the
        // requested upper bound.
        StreamVersion::Exact(head) => head.get().min(REQUESTED_UPPER),
        StreamVersion::NoStream => panic!("the seeded stream must be present"),
    };
    assert_eq!(
        endpoint, BACKLOG,
        "the first page's observation fixes the endpoint at the backlog's head"
    );
    let first_count = first.records().len();
    assert_eq!(
        first.records(),
        &backlog[..first_count],
        "the first page holds the backlog's opening stretch, envelopes included"
    );
    let mut replayed = first_count;
    let mut next_start = 1 + first_count as u64;

    // The rival lands inside the originally requested range, beyond the
    // captured endpoint, before the consumer's later pages run.
    let mut beyond_envelope = EventMetadata::new();
    beyond_envelope.insert("slot", (BACKLOG + 1).to_string());
    let beyond_batch = EventBatch::from_records(vec![RecordedEvent::keyed(
        make_event(u32::try_from(BACKLOG + 1).expect("a slot fits the event payload")),
        beyond_envelope,
    )])
    .expect("one record is nonempty");
    let beyond = rival
        .append(ExpectedVersion::Exact(seq(BACKLOG)), &id, &beyond_batch)
        .await
        .expect("the rival's append succeeds");
    assert_eq!(
        beyond.get(),
        BACKLOG + 1,
        "the rival lands beyond the captured endpoint"
    );

    // Later pages carry the captured endpoint, so the head's move
    // cannot ride a page into the replay.
    loop {
        let page = store
            .load_stream_page(&id, seq(next_start)..=seq(endpoint), PAGE_SIZE)
            .await
            .expect("page load succeeds");
        assert_eq!(
            page.at(),
            head_at(BACKLOG + 1),
            "every later page observes the moved head"
        );
        let count = page.records().len();
        if count == 0 {
            // An empty page ends catch-up.
            break;
        }
        assert_eq!(
            page.records(),
            &backlog[replayed..replayed + count],
            "each later page holds the backlog's next stretch, never the rival's event"
        );
        replayed += count;
        // The page's dense positions run from its requested start
        // through one less than its returned count, and a page
        // bounded by the endpoint cannot return a position beyond
        // it - so completion is checked on that stretch, before any
        // next-start arithmetic, and the largest sequence cannot
        // overflow the check.
        let through = next_start
            .checked_add(count as u64 - 1)
            .expect("a page bounded by the endpoint cannot overflow past it");
        if through >= endpoint {
            break;
        }
        next_start = through + 1;
    }

    assert_eq!(
        replayed,
        backlog.len(),
        "the pages replayed the backlog exactly once, in order, with envelopes"
    );

    // The rival's event is stored and reachable by its own page: the
    // captured endpoint excluded it, not a missing append.
    let tail = store
        .load_stream_page(&id, seq(BACKLOG + 1)..=seq(BACKLOG + 1), PAGE_SIZE)
        .await
        .expect("page load succeeds");
    assert_eq!(
        tail.records(),
        beyond_batch.records(),
        "the rival's event pages on its own, beyond the endpoint"
    );
}

/// Checks that a page's records and head agree during a rival append:
/// an open-ended range under a page limit reads either the seed's tail
/// at the seed's head or the rival's tail at the rival's head, never a
/// mix of the two views.
pub async fn a_page_read_during_a_rival_append_stays_one_view<S, E>(
    store: S,
    rival: S,
    make_id: impl Fn(&str) -> S::Id,
    make_event: impl Fn(u32) -> E + Clone + Send + Sync + 'static,
) where
    S: EventStreams<E> + Clone + Send + Sync + 'static,
    S::Id: Clone + Send + Sync + 'static,
    E: Event + PartialEq + Send + Sync + Debug + 'static,
{
    for iteration in 0..RACE_ITERATIONS {
        let id = make_id(&unique_stream_id("page-race", iteration));
        let seeded = store
            .append(
                ExpectedVersion::NoStream,
                &id,
                &EventBatch::new(vec![make_event(1), make_event(2)])
                    .expect("two events are nonempty"),
            )
            .await
            .expect("seed append succeeds");
        assert_eq!(seeded.get(), 2, "the seed lands at sequence 2");

        let barrier = Arc::new(Barrier::new(2));
        let reader = {
            let store = store.clone();
            let id = id.clone();
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move {
                barrier.wait().await;
                store.load_stream_page(&id, seq(2)..=seq(u64::MAX), 5).await
            })
        };
        let writer = {
            let rival = rival.clone();
            let id = id.clone();
            let barrier = Arc::clone(&barrier);
            let make_event = make_event.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                rival
                    .append(
                        ExpectedVersion::Exact(seq(2)),
                        &id,
                        &one_event_batch(make_event(3)),
                    )
                    .await
            })
        };

        let page = reader
            .await
            .expect("reader task must not panic")
            .expect("page load succeeds");
        let appended = writer
            .await
            .expect("writer task must not panic")
            .expect("the rival's append succeeds");
        assert_eq!(appended.get(), 3, "the rival lands at sequence 3");

        let seed_tail = vec![RecordedEvent::new(make_event(2))];
        let raced_tail = vec![
            RecordedEvent::new(make_event(2)),
            RecordedEvent::new(make_event(3)),
        ];
        match (page.at(), page.records()) {
            (at, records) if records == seed_tail.as_slice() => {
                assert_eq!(at, head_at(2), "the seed tail is observed at the seed head");
            }
            (at, records) if records == raced_tail.as_slice() => {
                assert_eq!(
                    at,
                    head_at(3),
                    "the rival's tail is observed at the rival's head"
                );
            }
            (at, records) => {
                panic!("the page's head and records disagree: {at:?} over {records:?}")
            }
        }
    }
}

/// Checks that a strict event type pages without decoding what its
/// bounds and its count exclude: the rows a page never selects cannot
/// fail the read, while a selected undecodable row still fails it. A
/// zero limit observes the head without decoding anything, and every
/// page still reports the observed head. The strict reader's event
/// type must fail to decode the loose writer's payload - that
/// relationship is the wiring's to provide. Only the SQL backends
/// wire this case: the in-memory backend stores typed events and has
/// no decode step, so an undecodable row cannot exist there.
pub async fn a_page_selects_before_decoding<S, L, E, F>(
    strict: S,
    loose: L,
    make_id: impl Fn(&str) -> S::Id,
    make_object: impl Fn(&str) -> E,
    make_unit: impl Fn() -> F,
    is_decode_failure: impl Fn(&S::Error) -> bool,
) where
    S: EventStreams<E>,
    L: EventStreams<F, Id = S::Id>,
    E: Event + PartialEq + Send + Sync + Debug,
    F: Event + Send + Sync + Debug,
{
    let id = make_id(&unique_stream_id("page-decode", 0));
    // A three-event stream whose middle row cannot decode as the
    // strict reader's event type.
    strict
        .append(
            ExpectedVersion::NoStream,
            &id,
            &one_event_batch(make_object("one")),
        )
        .await
        .expect("sequence 1 lands");
    loose
        .append(
            ExpectedVersion::Exact(seq(1)),
            &id,
            &one_event_batch(make_unit()),
        )
        .await
        .expect("sequence 2 lands");
    strict
        .append(
            ExpectedVersion::Exact(seq(2)),
            &id,
            &one_event_batch(make_object("three")),
        )
        .await
        .expect("sequence 3 lands");

    // The upper bound excludes the undecodable middle row.
    let page = strict
        .load_stream_page(&id, seq(1)..=seq(1), 5)
        .await
        .expect("the upper bound excludes the undecodable row");
    assert_eq!(
        page.records(),
        vec![RecordedEvent::new(make_object("one"))],
        "the page holds the row the bound admits"
    );
    assert_eq!(page.at(), head_at(3), "the head is observed");

    // The lower bound excludes it too.
    let page = strict
        .load_stream_page(&id, seq(3)..=seq(3), 5)
        .await
        .expect("the lower bound excludes the undecodable row");
    assert_eq!(
        page.records(),
        vec![RecordedEvent::new(make_object("three"))],
        "the page holds the row the bound admits"
    );

    // The count limit excludes it: only the first row is selected.
    let page = strict
        .load_stream_page(&id, seq(1)..=seq(3), 1)
        .await
        .expect("the count limit excludes the undecodable row");
    assert_eq!(
        page.records(),
        vec![RecordedEvent::new(make_object("one"))],
        "the page holds the first row the limit admits"
    );

    // A selected undecodable row still fails the read.
    let outcome = strict
        .load_stream_page(&id, seq(2)..=seq(2), 5)
        .await
        .expect_err("a selected undecodable row fails the read");
    assert!(
        is_decode_failure(&outcome),
        "the failure is the decode failure, got {outcome:?}"
    );

    // A zero limit observes the head without decoding anything.
    let page = strict
        .load_stream_page(&id, seq(1)..=seq(3), 0)
        .await
        .expect("a zero limit decodes nothing");
    assert!(page.records().is_empty(), "no row was selected to decode");
    assert_eq!(page.at(), head_at(3), "the head is still observed");
}

fn unique_stream_id(configuration: &str, iteration: usize) -> String {
    format!(
        "spec-{configuration}-{}-{iteration}",
        rusty_ulid::generate_ulid_string()
    )
}

fn one_event_batch<E>(event: E) -> EventBatch<E> {
    // A single-event batch is trivially nonempty, so the empty-batch
    // error is unreachable.
    EventBatch::new(vec![event]).expect("a single event is nonempty")
}

fn head_at(position: u64) -> StreamVersion {
    StreamVersion::Exact(StreamSequence::new(position).expect("a head is a 1-based position"))
}

/// A page bound: 1-based, so zero cannot be written through it.
fn seq(position: u64) -> StreamSequence {
    StreamSequence::new(position).expect("a page bound is a 1-based position")
}

/// One expected page in the bounds case: the requested range, the page
/// limit, and the records the page must return.
type ExpectedPage<'a, E> = (
    std::ops::RangeInclusive<StreamSequence>,
    usize,
    &'a [RecordedEvent<E>],
);

/// Load a stream that must be present and report its observed position.
async fn observed_sequence<S, E>(store: &S, id: &S::Id) -> StreamSequence
where
    S: EventStreams<E>,
    E: Event + Send + Sync + Debug,
{
    let state = store.load_stream(id).await.expect("load succeeds");
    match state.version() {
        // A present stream's version is its event count.
        StreamVersion::Exact(sequence) => sequence,
        StreamVersion::NoStream => panic!("the stream must be present at this point"),
    }
}

/// Post-race stored-state pin: the final stream holds exactly this many
/// events, which under ADR 0003's semantics IS the final version.
async fn assert_stored_event_count<S, E>(store: &S, id: &S::Id, expected: u64)
where
    S: EventStreams<E>,
    E: Event + Send + Sync + Debug,
{
    let stored = observed_sequence(store, id).await;
    assert_eq!(
        stored.get(),
        expected,
        "the stored stream must hold exactly {expected} events (version {expected})"
    );
}

fn assert_exactly_one_winner<B>(
    a: Result<StreamSequence, AppendError<B>>,
    b: Result<StreamSequence, AppendError<B>>,
    winner_sequence: u64,
) where
    B: std::error::Error,
{
    match (a, b) {
        (Ok(winner), Err(AppendError::Conflict(_)))
        | (Err(AppendError::Conflict(_)), Ok(winner)) => {
            assert_eq!(
                winner.get(),
                winner_sequence,
                "the winner lands at the next sequence"
            );
        }
        (a, b) => {
            panic!("expected exactly one winner and one version conflict, got {a:?} and {b:?}")
        }
    }
}

#[cfg(all(test, feature = "in_memory"))]
mod tests {
    use super::*;
    use crate::streams::in_memory::InMemoryEventStreams;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct SomethingHappened;

    impl Event for SomethingHappened {
        type EntityId = ();

        fn event_type(&self) -> String {
            "SomethingHappened".to_owned()
        }

        fn get_id(&self) -> Self::EntityId {}
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_single_event_occ_race_on_empty_stream() {
        let store = InMemoryEventStreams::<SomethingHappened>::new();
        under_deadline(single_event_occ_race_on_empty_stream(
            store,
            str::to_owned,
            || SomethingHappened,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_single_event_occ_race_on_seeded_stream() {
        let store = InMemoryEventStreams::<SomethingHappened>::new();
        under_deadline(single_event_occ_race_on_seeded_stream(
            store,
            str::to_owned,
            || SomethingHappened,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_flash_sale_sells_exactly_the_stock() {
        let store = InMemoryEventStreams::<SomethingHappened>::new();
        under_deadline(flash_sale_sells_exactly_the_stock(
            store,
            str::to_owned,
            || SomethingHappened,
        ))
        .await;
    }

    /// A payload-carrying event, so read paths can tell which append
    /// produced which record.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Counted(u32);

    impl Event for Counted {
        type EntityId = ();

        fn event_type(&self) -> String {
            "Counted".to_owned()
        }

        fn get_id(&self) -> Self::EntityId {}
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_a_slice_reads_from_the_cursor_to_the_head() {
        let store = InMemoryEventStreams::<Counted>::new();
        under_deadline(a_slice_reads_from_the_cursor_to_the_head(
            store,
            str::to_owned,
            Counted,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_a_category_read_round_trips_ids_in_append_order() {
        let store = InMemoryEventStreams::<Counted>::new();
        under_deadline(a_category_read_round_trips_ids_in_append_order(
            store,
            str::to_owned,
            Counted,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_a_slice_read_during_a_rival_append_stays_one_view() {
        let store = InMemoryEventStreams::<Counted>::new();
        under_deadline(a_slice_read_during_a_rival_append_stays_one_view(
            store.clone(),
            store,
            str::to_owned,
            Counted,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_a_page_reads_exactly_the_requested_bounds() {
        let store = InMemoryEventStreams::<Counted>::new();
        under_deadline(a_page_reads_exactly_the_requested_bounds(
            store,
            str::to_owned,
            Counted,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_a_consumer_pages_a_backlog_to_a_captured_endpoint() {
        let store = InMemoryEventStreams::<Counted>::new();
        under_deadline(a_consumer_pages_a_backlog_to_a_captured_endpoint(
            store.clone(),
            store,
            str::to_owned,
            Counted,
        ))
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_a_page_read_during_a_rival_append_stays_one_view() {
        let store = InMemoryEventStreams::<Counted>::new();
        under_deadline(a_page_read_during_a_rival_append_stays_one_view(
            store.clone(),
            store,
            str::to_owned,
            Counted,
        ))
        .await;
    }
}
