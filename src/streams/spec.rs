//! Generic spec suite over any [`EventStreams`] implementation
//! (ADR 0003). E3's postgres implementation runs these cases unchanged
//! by wiring its own test module through them.
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

use super::{AppendError, EventBatch, EventStreams, ExpectedVersion, StreamSequence, StreamState};

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
pub(crate) async fn under_deadline(case: impl std::future::Future<Output = ()>) {
    tokio::time::timeout(CASE_DEADLINE, case)
        .await
        .expect("spec case must finish under the deadline");
}

/// Two contenders observe the same never-used stream, release together,
/// and each appends one event expecting the empty stream: exactly one
/// wins at sequence 1 and one gets the version conflict. A post-race
/// load pins the stored state: exactly one event, so version 1.
pub(crate) async fn single_event_occ_race_on_empty_stream<S, E>(
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
pub(crate) async fn single_event_occ_race_on_seeded_stream<S, E>(
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
pub(crate) async fn flash_sale_sells_exactly_the_stock<S, E>(
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

fn unique_stream_id(configuration: &str, iteration: usize) -> String {
    format!(
        "spec-{configuration}-{}-{iteration}",
        rusty_ulid::generate_ulid_string()
    )
}

fn one_event_batch<E>(event: E) -> EventBatch<E> {
    // `EventBatch::new` is an E1 hole outside E2's fill bound;
    // crate-internal construction here is trivially nonempty.
    EventBatch(vec![event])
}

/// Load a stream that must be present and report its observed position.
async fn observed_sequence<S, E>(store: &S, id: &S::Id) -> StreamSequence
where
    S: EventStreams<E>,
    E: Event + Send + Sync + Debug,
{
    let state = store.load_stream(id).await.expect("load succeeds");
    match &state {
        // `StreamState::version` is an E1 hole outside E2's fill
        // bound; a present stream's version is its event count.
        StreamState::Present(batch) => StreamSequence::new(batch.0.len() as u64)
            .expect("a present stream has at least one event"),
        StreamState::Missing => panic!("the stream must be present at this point"),
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
}
