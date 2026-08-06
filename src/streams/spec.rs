//! Generic spec suite over any [`EventStreams`] implementation
//! (ADR 0003). E3's postgres implementation runs these cases unchanged
//! by wiring its own test module through them.
//!
//! Every case generates a unique stream id per configuration and per
//! iteration (a ULID nonce in the id), so preconditions hold on any
//! backend without assuming storage can be cleared. Contenders load
//! state, synchronize on a shared [`tokio::sync::Barrier`], then
//! append, so the optimistic-concurrency collision is deterministic;
//! the iteration loop is defense in depth, never the mechanism.

use std::fmt::Debug;

use tokio::sync::Barrier;

use crate::decider::Event;

use super::{AppendError, EventBatch, EventStreams, ExpectedVersion, StreamSequence, StreamState};

const RACE_ITERATIONS: usize = 20;

/// Two contenders observe the same never-used stream, release together,
/// and each appends one event expecting the empty stream: exactly one
/// wins at sequence 1 and one gets the version conflict.
pub(crate) async fn single_event_occ_race_on_empty_stream<S, E>(
    store: &S,
    make_id: impl Fn(&str) -> S::Id + Sync,
    make_event: impl Fn() -> E + Sync,
) where
    S: EventStreams<E> + Sync,
    E: Event + Send + Sync + Debug,
{
    for iteration in 0..RACE_ITERATIONS {
        let id = make_id(&unique_stream_id("occ-empty", iteration));
        let barrier = Barrier::new(2);

        let contender = || async {
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
        };

        let (a, b) = tokio::join!(contender(), contender());
        assert_exactly_one_winner(a, b, 1);
    }
}

/// One event is appended first; two contenders observe the one-event
/// stream, release together, and each appends one event at the observed
/// version: exactly one wins at sequence 2 and one gets the version
/// conflict. This is the other side of the old backend's defect, where
/// an empty stream and a one-event stream shared version 0.
pub(crate) async fn single_event_occ_race_on_seeded_stream<S, E>(
    store: &S,
    make_id: impl Fn(&str) -> S::Id + Sync,
    make_event: impl Fn() -> E + Sync,
) where
    S: EventStreams<E> + Sync,
    E: Event + Send + Sync + Debug,
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

        let barrier = Barrier::new(2);

        let contender = || async {
            let state = store.load_stream(&id).await.expect("load succeeds");
            let observed = match &state {
                // `StreamState::version` is an E1 hole outside E2's fill
                // bound; a present stream's version is its event count.
                StreamState::Present(batch) => StreamSequence::new(batch.0.len() as u64)
                    .expect("a present stream has at least one event"),
                StreamState::Missing => panic!("the seeded stream must be present"),
            };
            assert_eq!(observed.get(), 1, "contenders observe the one-event stream");
            barrier.wait().await;
            store
                .append(
                    ExpectedVersion::Exact(observed),
                    &id,
                    &one_event_batch(make_event()),
                )
                .await
        };

        let (a, b) = tokio::join!(contender(), contender());
        assert_exactly_one_winner(a, b, 2);
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

    #[tokio::test]
    async fn in_memory_single_event_occ_race_on_empty_stream() {
        let store = InMemoryEventStreams::<SomethingHappened>::new();
        single_event_occ_race_on_empty_stream(&store, str::to_owned, || SomethingHappened).await;
    }

    #[tokio::test]
    async fn in_memory_single_event_occ_race_on_seeded_stream() {
        let store = InMemoryEventStreams::<SomethingHappened>::new();
        single_event_occ_race_on_seeded_stream(&store, str::to_owned, || SomethingHappened).await;
    }
}
