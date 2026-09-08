//! C3 (feed waits) repair proofs: the library feed paths -
//! `PgEventFeed::poll` and `PgEventFeed::ack` - bound their lock waits
//! with the configured `lock_timeout`, and an expiry surfaces as the
//! retryable `PgStreamsError::LockTimeout(effective_bound)` - wrapped
//! in `AckError::Backend` on the ack path - never a generic backend
//! fault. Runs against the live compose postgres:
//!
//! ```sh
//! cargo test --features postgres --test feed_timeouts -- --nocapture
//! ```
//!
//! Public APIs only. An independent raw holder makes the contention
//! real: the cursor-row case holds the group's existing cursor row
//! `FOR UPDATE`; the first-poll case holds a PLAIN uncommitted
//! `INSERT` into `epoch_feed_cursors` that the poll's watermark upsert
//! must wait behind. The classified expiry is itself the wait
//! evidence - the server reports `55P03` only after a wait that
//! outlasted the configured bound - so no sleep poses as proof and
//! there are no tight wallclock thresholds. Every state claim is an
//! independent raw readback (`cursor_row_state`), never feed output.
//! Every case runs under `under_deadline`'s outer bound, so a
//! regression to an unbounded wait fails fast instead of hanging the
//! suite.
//!
//! Honesty notes: the configured 1ms normalization floor IS tested -
//! the bounds case pins the REPORTED duration (`Duration` equality) -
//! but the five-second default is never timed, and the default's field
//! wiring remains unit-test material beside `configure_feed_timeout`.
//! The feed bounds lock waits only; nothing here claims an idle-holder
//! or client-duration bound.

#![cfg(feature = "postgres")]

use std::time::Duration;

use serde::{Deserialize, Serialize};

use epoch::streams::postgres::{PgEventFeed, PgEventStreams, PgPool};
use epoch::streams::spec::under_deadline;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Ticked;

impl epoch::decider::Event for Ticked {
    type EntityId = ();

    fn event_type(&self) -> String {
        "Ticked".to_owned()
    }

    fn get_id(&self) -> Self::EntityId {}
}

/// The explicitly configured lock-wait bound under test - short
/// enough that a contended poll or ack expires in milliseconds, never
/// the 5s default.
const FEED_BOUND: Duration = Duration::from_millis(100);

/// The live test database, exactly as the rest of the pg suite finds
/// it: an unset `EPOCH_PG_TEST_URL` stops the run rather than picking
/// a database.
fn conn_str() -> String {
    let _ = dotenv::dotenv();
    std::env::var("EPOCH_PG_TEST_URL").expect(
        "EPOCH_PG_TEST_URL must be set (see .env.example; \
         `cp .env.example .env && docker compose up -d`)",
    )
}

/// Categories carry a ULID nonce, so runs never collide and storage
/// is never cleared: every case gets a unique category.
fn unique_category(prefix: &str) -> String {
    format!("{prefix}-{}", rusty_ulid::generate_ulid_string())
}

/// The default library pool: the shared writer/feed handle's pool, and
/// - on a second call - the independent holder connections.
async fn feed_pool() -> PgPool {
    epoch::streams::postgres::pool_from_conn_str(&conn_str())
        .await
        .expect("pg pool from EPOCH_PG_TEST_URL")
}

/// The max-size-1 library pool: its one backend is the connection a
/// completed feed transaction ran on, which is what makes the reuse
/// readback (`SHOW lock_timeout` after the poll) meaningful.
async fn single_feed_pool() -> PgPool {
    let config: tokio_postgres::Config = conn_str().parse().expect("valid url");
    bb8::Pool::builder()
        .max_size(1)
        .build(bb8_postgres::PostgresConnectionManager::new(
            config,
            tokio_postgres::NoTls,
        ))
        .await
        .expect("the max-size-1 pg pool builds")
}

/// A writer and a feed over one fresh category, both on `pool`: the
/// feed carries `lock_timeout`; the writer is the public append path
/// the cases seed with. Everything here is public API surface.
async fn writer_and_feed(
    pool: PgPool,
    category: &str,
    lock_timeout: Duration,
) -> (PgEventStreams<String, Ticked>, PgEventFeed<Ticked>, PgPool) {
    let writer = PgEventStreams::new(pool.clone(), category);
    writer.migrate().await.expect("schema migrates");
    let feed = PgEventFeed::new(pool.clone(), category).with_lock_timeout(lock_timeout);
    (writer, feed, pool)
}

/// One group's committed progress row, read independently of the feed:
/// a raw SELECT over `epoch_feed_cursors`; `None` before the group's
/// first committed poll. The readback every state claim uses.
async fn cursor_row_state(pool: &PgPool, category: &str, group: &str) -> Option<(u64, u64)> {
    let conn = pool.get().await.expect("a readback connection");
    let found = conn
        .query_opt(
            "SELECT cursor, delivered_to FROM epoch_feed_cursors \
             WHERE category = $1 AND group_name = $2",
            &[&category, &group],
        )
        .await
        .expect("the cursor readback queries");
    found.map(|row| {
        (
            u64::try_from(row.get::<_, i64>("cursor")).expect("stored cursors are non-negative"),
            u64::try_from(row.get::<_, i64>("delivered_to"))
                .expect("stored watermarks are non-negative"),
        )
    })
}

/// C3, cursor-row contention (the v1 one-poller backstop). A holder
/// transaction on an independent connection locks the group's existing
/// cursor row `FOR UPDATE`; poll and ack must each return the typed
/// timeout and move nothing, and both must recover once the holder
/// releases.
#[tokio::test(flavor = "multi_thread")]
async fn a_held_cursor_row_times_out_poll_and_ack_without_moving_state() {
    under_deadline(async {
        use epoch::streams::feed::{AckError, ConsumerGroup, EventFeed, PollLimit};
        use epoch::streams::postgres::PgStreamsError;
        use epoch::streams::{EventBatch, EventStreams, ExpectedVersion};

        let shared = feed_pool().await;
        let category = unique_category("feed-timeout-cursor");
        let (writer, feed, pool) = writer_and_feed(shared, &category, FEED_BOUND).await;
        let group = ConsumerGroup::new("cursor-holders").expect("a named group is non-empty");
        let limit = PollLimit::new(10).expect("nonzero limit");

        let key = format!("k-{}", rusty_ulid::generate_ulid_string());
        writer
            .append(
                ExpectedVersion::NoStream,
                &key,
                &EventBatch::new(vec![Ticked]).expect("one event is nonempty"),
            )
            .await
            .expect("the seed append succeeds");

        // The establishing poll delivers and moves only the delivered
        // watermark; no ack follows, so the cursor stays at START.
        let first = feed
            .poll(&group, limit)
            .await
            .expect("the establishing poll delivers");
        assert_eq!(first.len(), 1, "the seeded event delivers alone");
        let tip = first.last().expect("the seeded entry").position();
        let baseline = Some((0, tip.get()));
        assert_eq!(
            cursor_row_state(&pool, &category, group.as_str()).await,
            baseline,
            "the unacked poll moves the watermark; the cursor stays at START"
        );

        // An independent connection holds the group's existing cursor
        // row FOR UPDATE, so every later poll and ack waits behind it.
        let holder_pool = feed_pool().await;
        let mut holder = holder_pool.get().await.expect("a holder connection");
        let held = holder.transaction().await.expect("holder transaction");
        held.query_one(
            "SELECT cursor, delivered_to FROM epoch_feed_cursors \
             WHERE category = $1 AND group_name = $2 FOR UPDATE",
            &[&category, &group.as_str()],
        )
        .await
        .expect("the holder locks the group's cursor row");

        let poll_outcome = feed.poll(&group, limit).await;
        assert!(
            matches!(&poll_outcome, Err(PgStreamsError::LockTimeout(reported)) if *reported == FEED_BOUND),
            "the contended poll is the typed timeout at the configured bound, got {poll_outcome:?}"
        );
        let ack_outcome = feed.ack(&group, tip).await;
        assert!(
            matches!(
                &ack_outcome,
                Err(AckError::Backend(PgStreamsError::LockTimeout(reported))) if *reported == FEED_BOUND
            ),
            "the contended ack is the typed timeout wrapped in Backend, got {ack_outcome:?}"
        );
        assert_eq!(
            cursor_row_state(&pool, &category, group.as_str()).await,
            baseline,
            "the timed-out poll and ack moved neither cursor nor watermark"
        );

        held.rollback().await.expect("the holder releases");

        let retried = feed
            .poll(&group, limit)
            .await
            .expect("the retry poll succeeds once the holder releases");
        assert_eq!(retried.len(), 1, "at-least-once redelivery after the release");
        assert_eq!(retried[0].position(), tip, "the same entry redelivers");
        feed.ack(&group, tip)
            .await
            .expect("the ack of the tip succeeds after the release");
        assert_eq!(
            cursor_row_state(&pool, &category, group.as_str()).await,
            Some((tip.get(), tip.get())),
            "the ack settles cursor and watermark at the tip"
        );
        let settled = feed
            .poll(&group, limit)
            .await
            .expect("the settled poll succeeds");
        assert!(settled.is_empty(), "nothing is left to deliver");
    })
    .await;
}

/// C3, first-poll upsert contention: the group has no cursor row, and
/// a PLAIN uncommitted INSERT into `epoch_feed_cursors` from an
/// independent connection sits under the unique key the poll's
/// watermark upsert must write; the first poll times out classified,
/// commits nothing, and a post-rollback retry delivers and recovers.
#[tokio::test(flavor = "multi_thread")]
async fn a_first_poll_behind_an_uncommitted_cursor_row_times_out_and_recovers() {
    under_deadline(async {
        use epoch::streams::feed::{ConsumerGroup, EventFeed, PollLimit};
        use epoch::streams::postgres::PgStreamsError;
        use epoch::streams::{EventBatch, EventStreams, ExpectedVersion};

        let shared = feed_pool().await;
        let category = unique_category("feed-timeout-first-poll");
        let (writer, feed, pool) = writer_and_feed(shared, &category, FEED_BOUND).await;
        let group = ConsumerGroup::new("first-poll-holders").expect("a named group is non-empty");
        let limit = PollLimit::new(10).expect("nonzero limit");

        assert_eq!(
            cursor_row_state(&pool, &category, group.as_str()).await,
            None,
            "a group that has never polled has no cursor row"
        );

        let key = format!("k-{}", rusty_ulid::generate_ulid_string());
        writer
            .append(
                ExpectedVersion::NoStream,
                &key,
                &EventBatch::new(vec![Ticked]).expect("one event is nonempty"),
            )
            .await
            .expect("the seed append succeeds");

        // A PLAIN uncommitted INSERT under the group's unique key: the
        // first poll's watermark upsert is the statement that must
        // wait behind it.
        let holder_pool = feed_pool().await;
        let mut holder = holder_pool.get().await.expect("a holder connection");
        let held = holder.transaction().await.expect("holder transaction");
        held.execute(
            "INSERT INTO epoch_feed_cursors (category, group_name, cursor, delivered_to) \
             VALUES ($1, $2, 0, 0)",
            &[&category, &group.as_str()],
        )
        .await
        .expect("the uncommitted cursor row is inserted");

        let outcome = feed.poll(&group, limit).await;
        assert!(
            matches!(&outcome, Err(PgStreamsError::LockTimeout(reported)) if *reported == FEED_BOUND),
            "the first poll times out classified behind the uncommitted row, got {outcome:?}"
        );
        assert_eq!(
            cursor_row_state(&pool, &category, group.as_str()).await,
            None,
            "the timed-out poll committed nothing and the holder's row is invisible"
        );

        held.rollback().await.expect("the holder releases");

        let delivered = feed
            .poll(&group, limit)
            .await
            .expect("the retry poll succeeds once the holder releases");
        assert_eq!(delivered.len(), 1, "the seeded entry delivers");
        let tip = delivered.last().expect("the seeded entry").position();
        assert_eq!(
            cursor_row_state(&pool, &category, group.as_str()).await,
            Some((0, tip.get())),
            "the retry poll lands the row with the cursor at START"
        );
        feed.ack(&group, tip)
            .await
            .expect("the ack of the tip succeeds");
    })
    .await;
}

/// C3, bound hygiene on a max-size-1 pool: zero and sub-millisecond
/// bounds normalize to the reported 1ms floor, a custom bound reports
/// verbatim, the bound travels with a clone across the shared pool,
/// the GUC never leaks onto the reused connection (SHOW lock_timeout
/// after a committed custom-bound poll), and an absurd out-of-range
/// bound fails as a Connection/config error, never LockTimeout.
#[tokio::test(flavor = "multi_thread")]
async fn bounds_normalize_clone_and_stay_transaction_local() {
    under_deadline(async {
        use epoch::streams::feed::{ConsumerGroup, EventFeed, PollLimit};
        use epoch::streams::postgres::PgStreamsError;
        use epoch::streams::{EventBatch, EventStreams, ExpectedVersion};

        let shared = single_feed_pool().await;
        let category = unique_category("feed-timeout-bounds");
        let (writer, feed, pool) = writer_and_feed(shared, &category, FEED_BOUND).await;
        // The holder pool is independent of the max-size-1 library
        // pool: a holder connection must never be the library's own
        // single backend.
        let holder_pool = feed_pool().await;
        let group = ConsumerGroup::new("bounds-probes").expect("a named group is non-empty");
        let limit = PollLimit::new(10).expect("nonzero limit");

        // The session GUC every probe must leave exactly where it
        // found it, read before any configured poll runs.
        let baseline: String = pool
            .get()
            .await
            .expect("a library-pool connection")
            .query_one("SHOW lock_timeout", &[])
            .await
            .expect("SHOW lock_timeout queries")
            .get(0);

        let key = format!("k-{}", rusty_ulid::generate_ulid_string());
        writer
            .append(
                ExpectedVersion::NoStream,
                &key,
                &EventBatch::new(vec![Ticked]).expect("one event is nonempty"),
            )
            .await
            .expect("the seed append succeeds");
        let established = feed
            .poll(&group, limit)
            .await
            .expect("the establishing poll delivers");
        assert_eq!(established.len(), 1, "the seeded event delivers alone");
        let tip = established.last().expect("the seeded entry").position();

        // Each probe gets a feed handle configured off the fixture
        // feed (`with_lock_timeout` takes self; the handle is Clone)
        // and its own holder on the independent pool, rolled back
        // before the next probe.
        let zero = feed.clone().with_lock_timeout(Duration::ZERO);
        let quarter_milli = feed.clone().with_lock_timeout(Duration::from_micros(250));
        let fifty = feed.clone().with_lock_timeout(Duration::from_millis(50));
        let fifty_clone = fifty.clone();
        for (probe, reported) in [
            (&zero, Duration::from_millis(1)),
            (&quarter_milli, Duration::from_millis(1)),
            (&fifty, Duration::from_millis(50)),
            (&fifty_clone, Duration::from_millis(50)),
        ] {
            let mut holder = holder_pool.get().await.expect("a holder connection");
            let held = holder.transaction().await.expect("holder transaction");
            held.query_one(
                "SELECT cursor, delivered_to FROM epoch_feed_cursors \
                 WHERE category = $1 AND group_name = $2 FOR UPDATE",
                &[&category, &group.as_str()],
            )
            .await
            .expect("the holder locks the group's cursor row");

            let outcome = probe.poll(&group, limit).await;
            assert!(
                matches!(&outcome, Err(PgStreamsError::LockTimeout(bound)) if *bound == reported),
                "expected the normalized reported bound {reported:?}, got {outcome:?}"
            );

            held.rollback().await.expect("the holder releases");
        }

        // One successful poll through the 50ms feed, then the reused
        // session - with max_size(1) the very connection the poll just
        // ran on - must still read its pre-probe GUC: SET LOCAL is
        // transaction-local, nothing leaked.
        let settled = fifty
            .poll(&group, limit)
            .await
            .expect("the uncontended poll through the 50ms feed succeeds");
        assert_eq!(settled.len(), 1, "the seeded entry redelivers");
        assert_eq!(settled[0].position(), tip, "the same entry redelivers");
        let after_success: String = pool
            .get()
            .await
            .expect("the reused library connection")
            .query_one("SHOW lock_timeout", &[])
            .await
            .expect("SHOW lock_timeout queries")
            .get(0);
        assert_eq!(
            after_success, baseline,
            "SET LOCAL is transaction-local: the session GUC is unchanged"
        );

        // An absurd out-of-range bound fails as a config/connection
        // error - the server rejects the GUC before any lock wait -
        // never the retryable LockTimeout class.
        let absurd = feed
            .clone()
            .with_lock_timeout(Duration::from_millis(u64::MAX));
        let outcome = absurd.poll(&group, limit).await;
        match &outcome {
            Err(PgStreamsError::Connection(_)) => {}
            Err(PgStreamsError::LockTimeout(bound)) => panic!(
                "an out-of-range bound is a config failure, never the retryable class; got LockTimeout({bound:?})"
            ),
            other => panic!("expected a Connection/config error, got {other:?}"),
        }
        let after_failure: String = pool
            .get()
            .await
            .expect("the reused library connection")
            .query_one("SHOW lock_timeout", &[])
            .await
            .expect("SHOW lock_timeout queries")
            .get(0);
        assert_eq!(
            after_failure, baseline,
            "the failed poll leaked nothing onto the session either"
        );
    })
    .await;
}
