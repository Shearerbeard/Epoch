//! Gate M: one consumer flow, driven against SQLite and live PostgreSQL,
//! compared as a transcript.
//!
//! The card's definition of end to end for a library crate is a
//! consumer-shaped exercise of the new streams surface run against a
//! live backend. This module drives the same flow through both
//! backends - single appends, a committed batch, a rejected batch,
//! paged polls, acks, a second group - and records what a consumer
//! would observe. The two transcripts must be identical.
//!
//! The transcript deliberately records **order and identity, not
//! positions**. PostgreSQL's `global_sequence` is table-wide and burns
//! a value on a rolled-back append; SQLite's rowid is per-database and
//! reuses one. ADR 0010 calls that difference out, so comparing raw
//! positions would fail for a documented reason rather than a defect.
//! What must match is the sequence of entries a consumer sees, and how
//! each ack is answered.
//!
//! Run it with the compose fixture up:
//!
//! ```text
//! cp .env.example .env
//! docker compose up -d postgres
//! cargo test --features postgres,sqlite --test e28_gate_m
//! ```

#![cfg(all(feature = "postgres", feature = "sqlite"))]

use std::future::Future;

use serde::{Deserialize, Serialize};
use tokio_rusqlite::Connection;

use epoch::decider::Event;
use epoch::streams::feed::{
    AckError, ConsumerGroup, EventFeed, FeedEntry, FeedPosition, PollLimit,
};
use epoch::streams::postgres::{pool_from_conn_str, PgDatabase, PgEventFeed, PgEventStreams};
use epoch::streams::sqlite::{SqliteBatchBuilder, SqliteDatabase};
use epoch::streams::{
    AtomicStreams, BatchConstraint, EventBatch, EventStreams, ExpectedVersion, TransactError,
};

/// The event the consumer writes and reads back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct OrderPlaced {
    order: u32,
    amount_cents: u32,
}

impl Event for OrderPlaced {
    type EntityId = u32;

    fn event_type(&self) -> String {
        "OrderPlaced".to_owned()
    }

    fn get_id(&self) -> u32 {
        self.order
    }
}

fn order(order: u32, amount_cents: u32) -> OrderPlaced {
    OrderPlaced {
        order,
        amount_cents,
    }
}

fn batch_of(events: Vec<OrderPlaced>) -> EventBatch<OrderPlaced> {
    EventBatch::new(events).expect("the flow never builds an empty batch")
}

/// What a consumer observed, in the order it observed it.
#[derive(Debug, Default, PartialEq, Eq)]
struct Transcript {
    /// Each poll's entries, as `stream|event type|payload`.
    pages: Vec<Vec<String>>,
    /// Each ack's answer, position-independent.
    acks: Vec<String>,
    /// Each batch's outcome, as the variant name.
    batches: Vec<String>,
}

fn render_page(page: &[FeedEntry<OrderPlaced>]) -> Vec<String> {
    page.iter()
        .map(|entry| {
            let payload = serde_json::to_string(entry.record().event())
                .expect("the event serializes the same way it was written");
            format!(
                "{}|{}|{}",
                entry.stream().key(),
                entry.record().event().event_type(),
                payload
            )
        })
        .collect()
}

/// Answer an ack without naming an absolute position: a NotDelivered is
/// recorded as its distance past the watermark, a Regression as its
/// distance below the cursor. Both are the same on every backend.
fn describe_ack<E>(outcome: Result<(), AckError<E>>) -> String
where
    E: std::error::Error + Send + Sync,
{
    match outcome {
        Ok(()) => "ok".to_owned(),
        Err(AckError::NotDelivered(rejection)) => format!(
            "not-delivered:+{}",
            rejection.attempted().get() - rejection.delivered_to().get()
        ),
        Err(AckError::Regression(rejection)) => format!(
            "regression:-{}",
            rejection.cursor().get() - rejection.attempted().get()
        ),
        Err(AckError::Backend(_)) => "backend".to_owned(),
    }
}

/// One batch's outcome as a variant name. The two backends have their
/// own error types, so the classification happens where the batch is
/// built rather than in the shared flow.
fn classify_transact<E>(outcome: Result<(), TransactError<E>>) -> String
where
    E: std::error::Error,
{
    match outcome {
        Ok(()) => "ok".to_owned(),
        Err(TransactError::Conflict(_)) => "conflict".to_owned(),
        Err(TransactError::ConstraintViolated(_)) => "constraint".to_owned(),
        Err(TransactError::DuplicateIntent(_)) => "duplicate-intent".to_owned(),
        Err(TransactError::LockTimeout(_)) => "lock-timeout".to_owned(),
        Err(TransactError::Backend(_)) => "backend".to_owned(),
    }
}

/// The consumer's flow, written once against the traits. The two batch
/// commits are passed in because a batch's wire form is the backend's
/// own type, which is the seam ADR 0006 deliberately keeps concrete.
async fn drive<S, F, G, B>(store: S, feed: F, good_batch: G, bad_batch: B) -> Transcript
where
    S: EventStreams<OrderPlaced, Id = String> + Sync,
    F: EventFeed<OrderPlaced> + Sync,
    G: Future<Output = String>,
    B: Future<Output = String>,
{
    let mut transcript = Transcript::default();
    let billing = ConsumerGroup::new("billing").expect("a non-empty group");
    let audit = ConsumerGroup::new("audit").expect("a non-empty group");
    let two = PollLimit::new(2).expect("a nonzero limit");
    let ten = PollLimit::new(10).expect("a nonzero limit");

    // Seed through the single-append path: two events on one stream,
    // one on another, so a page can straddle streams.
    store
        .append(
            ExpectedVersion::NoStream,
            &"order-1".to_owned(),
            &batch_of(vec![order(1, 1_000), order(1, 2_500)]),
        )
        .await
        .expect("order-1 seeds");
    store
        .append(
            ExpectedVersion::NoStream,
            &"order-2".to_owned(),
            &batch_of(vec![order(2, 700)]),
        )
        .await
        .expect("order-2 seeds");

    // The first page, and the group's first poll. This also creates the
    // group's progress row.
    let first = feed.poll(&billing, two).await.expect("the first poll");
    transcript.pages.push(render_page(&first));

    // An ack one past what was delivered is rejected, and says how far
    // past it went.
    let beyond = FeedPosition::new(
        first
            .last()
            .expect("the seed guarantees a page")
            .position()
            .get()
            + 1,
    )
    .expect("a position one past a stored one is nonzero");
    transcript
        .acks
        .push(describe_ack(feed.ack(&billing, beyond).await));

    // Polling again before an ack redelivers the same page.
    let again = feed.poll(&billing, two).await.expect("the redelivery poll");
    transcript.pages.push(render_page(&again));

    // Ack the page's last entry, then one below the cursor: the first
    // is a normal advance, the second a regression.
    transcript.acks.push(describe_ack(
        feed.ack(&billing, first.last().expect("a non-empty page").position())
            .await,
    ));
    transcript.acks.push(describe_ack(
        feed.ack(
            &billing,
            first.first().expect("a non-empty page").position(),
        )
        .await,
    ));

    // The next poll starts after the cursor.
    let third = feed.poll(&billing, two).await.expect("the post-ack poll");
    transcript.pages.push(render_page(&third));
    transcript.acks.push(describe_ack(
        feed.ack(
            &billing,
            third.last().expect("the seed's third event").position(),
        )
        .await,
    ));

    // Caught up: an empty page.
    let caught_up = feed.poll(&billing, two).await.expect("the empty poll");
    transcript.pages.push(render_page(&caught_up));

    // A batch that writes one stream and asserts another exists.
    transcript.batches.push(good_batch.await);

    let after_batch = feed.poll(&billing, two).await.expect("the batch poll");
    transcript.pages.push(render_page(&after_batch));
    for entry in &after_batch {
        transcript
            .acks
            .push(describe_ack(feed.ack(&billing, entry.position()).await));
    }

    // A batch whose constraint fails: it must roll back whole, so its
    // write never reaches the log.
    transcript.batches.push(bad_batch.await);

    let after_rejection = feed
        .poll(&billing, ten)
        .await
        .expect("the post-rejection poll");
    transcript.pages.push(render_page(&after_rejection));

    // A second group reads the whole log from the start, then drains it
    // two at a time.
    let audit_page = feed.poll(&audit, ten).await.expect("the audit poll");
    transcript.pages.push(render_page(&audit_page));
    loop {
        let page = feed.poll(&audit, two).await.expect("an audit page");
        if page.is_empty() {
            transcript.pages.push(Vec::new());
            break;
        }
        let last = page.last().expect("a non-empty page").position();
        transcript.pages.push(render_page(&page));
        transcript
            .acks
            .push(describe_ack(feed.ack(&audit, last).await));
    }

    // Both groups are caught up.
    for group in [&billing, &audit] {
        let page = feed.poll(group, ten).await.expect("the final poll");
        transcript.pages.push(render_page(&page));
    }

    transcript
}

/// The good batch: a fresh stream's two events, plus an assertion that
/// a stream written earlier exists.
async fn sqlite_good_batch(root: &SqliteDatabase, category: &str) -> String {
    let mut builder: SqliteBatchBuilder = root.batch();
    if let Err(error) = builder.write(
        category,
        &"order-3".to_owned(),
        ExpectedVersion::NoStream,
        &batch_of(vec![order(3, 4_200), order(3, 4_300)]),
    ) {
        return format!("write-refused:{error}");
    }
    builder.require(
        category,
        &"order-1".to_owned(),
        BatchConstraint::StreamExists,
    );
    let batch = match builder.build() {
        Ok(batch) => batch,
        Err(error) => return format!("build-refused:{error}"),
    };
    classify_transact(root.transact(batch).await)
}

/// The rejected batch: it writes a stream and asserts a stream that
/// does not exist. Nothing it wrote may survive.
async fn sqlite_bad_batch(root: &SqliteDatabase, category: &str) -> String {
    let mut builder: SqliteBatchBuilder = root.batch();
    if let Err(error) = builder.write(
        category,
        &"order-4".to_owned(),
        ExpectedVersion::NoStream,
        &batch_of(vec![order(4, 9_900)]),
    ) {
        return format!("write-refused:{error}");
    }
    builder.require(
        category,
        &"order-404".to_owned(),
        BatchConstraint::StreamExists,
    );
    let batch = match builder.build() {
        Ok(batch) => batch,
        Err(error) => return format!("build-refused:{error}"),
    };
    classify_transact(root.transact(batch).await)
}

async fn postgres_good_batch(db: &PgDatabase, category: &str) -> String {
    let mut builder = db.batch();
    if let Err(error) = builder.write(
        category,
        &"order-3".to_owned(),
        ExpectedVersion::NoStream,
        &batch_of(vec![order(3, 4_200), order(3, 4_300)]),
    ) {
        return format!("write-refused:{error}");
    }
    builder.require(
        category,
        &"order-1".to_owned(),
        BatchConstraint::StreamExists,
    );
    let batch = match builder.build() {
        Ok(batch) => batch,
        Err(error) => return format!("build-refused:{error}"),
    };
    classify_transact(db.transact(batch).await)
}

async fn postgres_bad_batch(db: &PgDatabase, category: &str) -> String {
    let mut builder = db.batch();
    if let Err(error) = builder.write(
        category,
        &"order-4".to_owned(),
        ExpectedVersion::NoStream,
        &batch_of(vec![order(4, 9_900)]),
    ) {
        return format!("write-refused:{error}");
    }
    builder.require(
        category,
        &"order-404".to_owned(),
        BatchConstraint::StreamExists,
    );
    let batch = match builder.build() {
        Ok(batch) => batch,
        Err(error) => return format!("build-refused:{error}"),
    };
    classify_transact(db.transact(batch).await)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_consumer_drives_sqlite_and_postgres_to_the_same_transcript() {
    let _ = dotenvy::dotenv();
    let conn_str = std::env::var("EPOCH_PG_TEST_URL").expect(
        "EPOCH_PG_TEST_URL must be set (see .env.example; \
         `cp .env.example .env && docker compose up -d`)",
    );
    let pool = pool_from_conn_str(&conn_str)
        .await
        .expect("a pool from EPOCH_PG_TEST_URL");

    // One category per run, so a rerun never sees an earlier run's log.
    // Both backends use the same string; the SQLite side is a private
    // in-memory database, so the name cannot collide with anything.
    let category = format!("gate-m-{}", rusty_ulid::generate_ulid_string());

    let postgres_store = PgEventStreams::<String, OrderPlaced>::new(pool.clone(), &category);
    postgres_store
        .migrate()
        .await
        .expect("the pg schema migrates");
    let postgres_feed = PgEventFeed::<OrderPlaced>::new(pool.clone(), &category);
    let postgres_db = PgDatabase::new(pool.clone());
    let postgres = drive(
        postgres_store,
        postgres_feed,
        postgres_good_batch(&postgres_db, &category),
        postgres_bad_batch(&postgres_db, &category),
    )
    .await;

    let sqlite_root = SqliteDatabase::new(
        Connection::open_in_memory()
            .await
            .expect("a private in-memory connection"),
    );
    sqlite_root
        .migrate()
        .await
        .expect("the sqlite schema migrates");
    let sqlite_store = sqlite_root.category::<String, OrderPlaced>(&category);
    let sqlite_feed = sqlite_root.feed::<OrderPlaced>(&category);
    let sqlite = drive(
        sqlite_store,
        sqlite_feed,
        sqlite_good_batch(&sqlite_root, &category),
        sqlite_bad_batch(&sqlite_root, &category),
    )
    .await;

    assert_eq!(
        postgres, sqlite,
        "the same consumer flow must observe the same log on both backends"
    );

    // Visible under `--nocapture`, so a gate run records what was
    // compared rather than only that the comparison passed.
    println!(
        "gate M transcript: {} polls, {} delivered entries, {} acks {:?}, batches {:?}",
        postgres.pages.len(),
        postgres.pages.iter().map(Vec::len).sum::<usize>(),
        postgres.acks.len(),
        postgres.acks,
        postgres.batches,
    );

    // The transcript is only worth comparing if it saw what the flow
    // intended: a rejected batch that wrote nothing, a second group
    // that read from the start, and acks that both advanced and were
    // refused.
    assert_eq!(postgres.batches, vec!["ok", "constraint"]);
    assert!(
        postgres.acks.iter().any(|ack| ack == "regression:-1"),
        "the flow must exercise a regression: {:?}",
        postgres.acks
    );
    assert!(
        postgres.acks.iter().any(|ack| ack == "not-delivered:+1"),
        "the flow must exercise an ack past the watermark: {:?}",
        postgres.acks
    );
    assert!(
        postgres
            .pages
            .iter()
            .flatten()
            .all(|entry| !entry.starts_with("order-4|")),
        "the rejected batch's write must not reach the log"
    );
}
