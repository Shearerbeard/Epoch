//! E19 feed property tests (ADR 0010, post-pivot): randomized
//! poll/ack invariants over the in-memory feed, and live-postgres
//! proofs for the properties only a real backend can exhibit - an
//! aborted append burning a sequence value the cursor passes without
//! waiting, committed positions staying contiguous under concurrent
//! writers (the single-writer funnel's observable shadow), and a
//! crashed consumer redelivered through a fresh handle.
//!
//! ```sh
//! cargo test --features postgres --test e19_feed_properties -- --nocapture
//! ```

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Noted;

impl epoch::decider::Event for Noted {
    type EntityId = ();

    fn event_type(&self) -> String {
        "Noted".to_owned()
    }

    fn get_id(&self) -> Self::EntityId {}
}

/// A tiny deterministic PRNG (xorshift64): every iteration's op
/// sequence derives from its index, so a failure replays exactly.
struct Rng(u64);

impl Rng {
    fn seeded(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

/// Randomized interleavings of append, poll, ack, and replayed acks
/// against a model that tracks the group's cursor and delivered
/// watermark. The feed must agree with the model on every outcome:
/// acks succeed exactly within delivery and strictly above the
/// cursor, at-cursor acks are no-ops, and below-cursor acks are
/// regressions.
#[cfg(feature = "in_memory")]
#[tokio::test(flavor = "multi_thread")]
async fn random_poll_ack_interleavings_agree_with_the_model() {
    use epoch::streams::feed::{AckError, ConsumerGroup, EventFeed, FeedPosition, PollLimit};
    use epoch::streams::in_memory::InMemoryDatabase;
    use epoch::streams::{EventBatch, EventStreams, ExpectedVersion};

    for iteration in 0..200 {
        let mut rng = Rng::seeded(iteration + 1);
        let db = InMemoryDatabase::new();
        let writer = db.category::<Noted>("prop").expect("fresh category");
        let feed = db.feed::<Noted>("prop").expect("claimed category");
        let group =
            ConsumerGroup::new(format!("g-{iteration}")).expect("a named group is non-empty");

        // The model: cursor and delivered watermark, plus the pool of
        // positions the group has seen delivered. `appended` tracks
        // the log independently of the implementation - this backend
        // burns no values and the case writes one category, so the
        // next append's position is known exactly - and the poll
        // assertions below compare delivery against it rather than
        // against what the feed reported.
        let mut model_cursor: u64 = 0;
        let mut model_delivered: u64 = 0;
        let mut positions: Vec<u64> = Vec::new();
        let mut appended: u64 = 0;

        for step in 0..60 {
            match rng.below(4) {
                0 | 1 => {
                    let stream = format!("s{}", rng.below(3));
                    let _ = writer
                        .append(
                            ExpectedVersion::Any,
                            &stream,
                            &EventBatch::new(vec![Noted]).expect("one event is nonempty"),
                        )
                        .await
                        .expect("append succeeds");
                    appended += 1;
                }
                2 => {
                    let limit = PollLimit::new(1 + rng.below(3)).expect("nonzero limit");
                    let entries = feed.poll(&group, limit).await.expect("poll succeeds");
                    assert!(
                        entries.len() as u64 == (appended - model_cursor).min(limit.get() as u64),
                        "iteration {iteration} step {step}: the page is the model's contiguous run"
                    );
                    for (offset, entry) in entries.iter().enumerate() {
                        let position = entry.position().get();
                        let expected = model_cursor + 1 + offset as u64;
                        assert_eq!(
                            position, expected,
                            "iteration {iteration} step {step}: delivery diverges from the model log"
                        );
                        positions.push(position);
                        model_delivered = model_delivered.max(position);
                    }
                }
                _ => {
                    let attempt = match rng.below(3) {
                        // A position the model knows was delivered.
                        0 if !positions.is_empty() => positions[rng.below(positions.len())],
                        // The at-cursor no-op; at START (0) no such
                        // position exists, so probe position 1 instead -
                        // the branches below model it correctly.
                        1 => model_cursor.max(1),
                        // Something at or past delivery (also the
                        // fallthrough when nothing was delivered yet).
                        _ => model_delivered + 1 + rng.below(3) as u64,
                    };
                    let position = FeedPosition::new(attempt).expect("a model position is nonzero");
                    let outcome = feed.ack(&group, position).await;
                    if attempt == model_cursor {
                        outcome.expect("at-cursor ack is a no-op");
                    } else if attempt < model_cursor {
                        match outcome {
                            Err(AckError::Regression(rejection)) => {
                                assert_eq!(rejection.attempted(), position);
                            }
                            other => panic!(
                                "iteration {iteration} step {step}: expected regression for {attempt} below {model_cursor}, got {other:?}"
                            ),
                        }
                    } else if attempt <= model_delivered {
                        outcome.expect("an in-delivery ack advances the cursor");
                        model_cursor = attempt;
                    } else {
                        match outcome {
                            Err(AckError::NotDelivered(rejection)) => {
                                assert_eq!(rejection.attempted(), position);
                            }
                            other => panic!(
                                "iteration {iteration} step {step}: expected not-delivered for {attempt} past {model_delivered}, got {other:?}"
                            ),
                        }
                    }
                }
            }
        }
    }
}

#[cfg(feature = "postgres")]
mod postgres_properties {
    use std::sync::Arc;

    use tokio::sync::Barrier;

    use epoch::streams::feed::{ConsumerGroup, EventFeed, PollLimit};
    use epoch::streams::postgres::{pool_from_conn_str, PgEventFeed, PgEventStreams};
    use epoch::streams::{EventBatch, EventStreams, ExpectedVersion};

    use super::Noted;

    fn conn_str() -> String {
        let _ = dotenv::dotenv();
        std::env::var("EPOCH_PG_TEST_URL")
            .expect("EPOCH_PG_TEST_URL must be set (see .env.example)")
    }

    async fn fixture(
        category: &str,
    ) -> (
        PgEventStreams<String, Noted>,
        PgEventFeed<Noted>,
        epoch::streams::postgres::PgPool,
    ) {
        let pool = pool_from_conn_str(&conn_str()).await.expect("pool");
        let store = PgEventStreams::new(pool.clone(), category);
        store.migrate().await.expect("schema migrates");
        (store, PgEventFeed::new(pool.clone(), category), pool)
    }

    fn unique(prefix: &str) -> String {
        format!("{prefix}-{}", rusty_ulid::generate_ulid_string())
    }

    /// A rolled-back insert burns a `global_sequence` value, leaving
    /// a gap no committed row carries. The cursor must pass the burn
    /// without waiting: the next poll delivers the events after it,
    /// with a position that skips the burned value.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_aborted_append_burns_a_value_the_cursor_passes() {
        let category = unique("feed-prop-burn");
        let (store, feed, pool) = fixture(&category).await;
        let stream = unique("s");

        store
            .append(
                ExpectedVersion::NoStream,
                &stream,
                &EventBatch::new(vec![Noted]).expect("one event is nonempty"),
            )
            .await
            .expect("seed before the burn");

        // A transaction that inserts and rolls back: the BIGSERIAL
        // draw is consumed, no row survives.
        let mut conn = pool.get().await.expect("raw connection");
        let tx = conn.transaction().await.expect("burn transaction");
        tx.execute(
            "INSERT INTO stream_events \
             (category, stream_key, event_type, sequence, event_data, event_metadata) \
             VALUES ($1, $2, 'Burned', 1, '\"{}\"'::jsonb, '{}'::jsonb)",
            &[&category, &unique("burned")],
        )
        .await
        .expect("the doomed insert");
        let burned: i64 = tx
            .query_one(
                "SELECT CURRVAL('stream_events_global_sequence_seq') AS drawn",
                &[],
            )
            .await
            .expect("read the burned value")
            .get("drawn");
        tx.rollback().await.expect("the burn rolls back");

        store
            .append(
                ExpectedVersion::NoStream,
                &unique("s2"),
                &EventBatch::new(vec![Noted]).expect("one event is nonempty"),
            )
            .await
            .expect("append after the burn");

        let group = ConsumerGroup::new("burn-testers").expect("non-empty group");
        let entries = feed
            .poll(&group, PollLimit::new(10).expect("nonzero limit"))
            .await
            .expect("poll passes the burn");
        assert_eq!(entries.len(), 2, "both committed events deliver");
        let positions: Vec<u64> = entries.iter().map(|e| e.position().get()).collect();
        let burned = u64::try_from(burned).expect("a sequence value is non-negative");
        assert!(
            !positions.contains(&burned),
            "the burned value delivers nothing"
        );

        // The burned value sits strictly between two delivered
        // positions. The gap may be wider than one - the sequence is
        // global, and concurrent categories' appends draw values too -
        // but the burn itself is in that gap and never blocked delivery.
        assert!(
            positions[0] < burned && burned < positions[1],
            "the burn {burned} sits between {} and {} without blocking them",
            positions[0],
            positions[1]
        );
    }

    /// Bulk completeness under concurrent writers. Eight concurrent
    /// writers append five events each; afterwards one poll delivers
    /// every committed row of the category, the tip is acked, and a
    /// second poll finds nothing below it. This pins completeness at
    /// a point in time, not the funnel's ordering theorem: every
    /// writer joins before the first poll, so a pre-funnel
    /// implementation would pass it too, and `global_sequence >
    /// cursor` could never reveal a latecomer below the cursor
    /// (review-round finding C1). The discriminating proof for the
    /// funnel itself is `a_funnel_holder_blocks_a_rival_append_until_release`
    /// in `src/streams/postgres.rs`. Literal position contiguity is
    /// not assertable on a shared database (the sequence is global
    /// and other categories' appends interleave draws); no-skip and
    /// no-latecomer is the property the feed actually promises.
    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_writers_deliver_completely_with_no_latecomers() {
        let category = unique("feed-prop-contig");
        let (store, _, pool) = fixture(&category).await;

        const WRITERS: usize = 8;
        const APPENDS: usize = 5;
        let barrier = Arc::new(Barrier::new(WRITERS));
        let mut writers = Vec::new();
        for worker in 0..WRITERS {
            let store = store.clone();
            let barrier = Arc::clone(&barrier);
            writers.push(tokio::spawn(async move {
                let stream = format!("w{worker}");
                barrier.wait().await;
                for _ in 0..APPENDS {
                    store
                        .append(
                            ExpectedVersion::Any,
                            &stream,
                            &EventBatch::new(vec![Noted]).expect("one event is nonempty"),
                        )
                        .await
                        .expect("funnel append succeeds");
                }
            }));
        }
        for w in writers {
            w.await.expect("writer task");
        }

        let count_pool = pool.clone();
        let conn = count_pool.get().await.expect("count connection");
        let n: i64 = conn
            .query_one(
                "SELECT COUNT(*) FROM stream_events WHERE category = $1",
                &[&category],
            )
            .await
            .expect("committed count")
            .get(0);
        assert_eq!(
            n,
            (WRITERS * APPENDS) as i64,
            "every append committed exactly once"
        );

        let feed = PgEventFeed::<Noted>::new(pool, &category);
        let group = ConsumerGroup::new("contig-testers").expect("non-empty group");
        let delivered = feed
            .poll(&group, PollLimit::new(1000).expect("nonzero limit"))
            .await
            .expect("the catch-up poll");
        assert_eq!(
            delivered.len(),
            n as usize,
            "one poll delivers every committed row: nothing skipped"
        );
        let tip = delivered.last().expect("writers appended").position();
        feed.ack(&group, tip).await.expect("ack the tip");
        let latecomers = feed
            .poll(&group, PollLimit::new(1000).expect("nonzero limit"))
            .await
            .expect("the settled poll");
        assert!(
            latecomers.is_empty(),
            "no committed event arrives below the acked tip after the fact"
        );
    }

    /// A consumer that crashes between delivery and ack redelivers:
    /// the original handle dropped, a fresh handle polls the same
    /// group and receives the same entries.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_crashed_consumer_redelivers_through_a_fresh_handle() {
        let category = unique("feed-prop-crash");
        let (store, feed, pool) = fixture(&category).await;
        store
            .append(
                ExpectedVersion::NoStream,
                &unique("s"),
                &EventBatch::new(vec![Noted, Noted]).expect("two events are nonempty"),
            )
            .await
            .expect("seed append");

        let group = ConsumerGroup::new("crash-testers").expect("non-empty group");
        let before = feed
            .poll(&group, PollLimit::new(10).expect("nonzero limit"))
            .await
            .expect("delivery before the crash");
        assert_eq!(before.len(), 2);

        // The crash: no ack ever arrives for this group. The durable
        // state lives in the database, so a fresh handle is the
        // consumer-restart shape.
        let fresh = PgEventFeed::<Noted>::new(pool, &category);
        let after = fresh
            .poll(&group, PollLimit::new(10).expect("nonzero limit"))
            .await
            .expect("delivery after the crash");
        assert_eq!(after.len(), 2, "at-least-once: the crash replays delivery");
        assert_eq!(
            after.iter().map(|e| e.position().get()).collect::<Vec<_>>(),
            before
                .iter()
                .map(|e| e.position().get())
                .collect::<Vec<_>>(),
            "redelivery is the same entries in the same order"
        );
    }
}
