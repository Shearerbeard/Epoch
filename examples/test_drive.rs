//! A consumer-shaped test drive of the event feed, against SQLite.
//!
//! Run it with:
//!
//! ```text
//! cargo run --example test_drive --features sqlite
//! ```
//!
//! The drive is written the way a real consumer drives a feed: open the
//! store, migrate it, write through the store and through a batch, then
//! poll and acknowledge as a named consumer group would. Each step
//! prints what it did and what came back, so the output is the lesson.
//!
//! Five things are worth watching for:
//!
//! 1. **A poll is not a read.** The entries a poll returns stay
//!    delivered until they are acknowledged, so a second poll before an
//!    ack returns the same entries again. That is at-least-once
//!    delivery, and it is why a consumer has to be idempotent.
//! 2. **The cursor and the delivered watermark are different numbers.**
//!    Polling moves the watermark; acknowledging moves the cursor. The
//!    gap between them is exactly the redelivery window.
//! 3. **An ack past the watermark is rejected**, because it would step
//!    over entries the group was never given.
//! 4. **A group's cursor is its own.** A second group reads the whole
//!    log from the beginning, no matter how far the first group got.
//! 5. **Paging is the caller's loop.** `poll` returns at most `limit`
//!    entries; a consumer that wants the backlog asks again.

#[cfg(feature = "sqlite")]
mod drive {
    use serde::{Deserialize, Serialize};
    use std::error::Error;
    use tokio_rusqlite::Connection;

    use epoch::decider::Event;
    use epoch::streams::feed::{
        AckError, ConsumerGroup, EventFeed, FeedEntry, FeedPosition, PollLimit,
    };
    use epoch::streams::sqlite::{SqliteDatabase, SqliteEventFeed, SqliteEventStreams};
    use epoch::streams::{
        AtomicStreams, BatchConstraint, EventBatch, EventStreams, ExpectedVersion,
    };

    /// The category this drive writes and reads. A feed reads one
    /// category; writers append under the same string.
    const CATEGORY: &str = "test-drive";

    /// The domain event the drive stores.
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
        EventBatch::new(events).expect("the drive never builds an empty batch")
    }

    /// Print one page the way a consumer's log line would.
    fn print_page(label: &str, page: &[FeedEntry<OrderPlaced>]) {
        if page.is_empty() {
            println!("  {label}: (no entries)");
            return;
        }
        let rendered = page
            .iter()
            .map(|entry| {
                format!(
                    "position {} = order {} ({} cents) on {}",
                    entry.position().get(),
                    entry.record().event().order,
                    entry.record().event().amount_cents,
                    entry.stream().key(),
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        println!("  {label}: {rendered}");
    }

    /// Poll until the category is drained, acknowledging as we go. This
    /// is the loop a consumer actually runs.
    async fn drain(
        feed: &SqliteEventFeed<OrderPlaced>,
        group: &ConsumerGroup,
        limit: PollLimit,
    ) -> Result<usize, Box<dyn Error>> {
        let mut seen = 0;
        loop {
            let page = feed.poll(group, limit).await?;
            if page.is_empty() {
                break;
            }
            seen += page.len();
            let last = page.last().expect("the page is non-empty").position();
            print_page("page", &page);
            feed.ack(group, last).await?;
            println!("  acked through position {}", last.get());
        }
        Ok(seen)
    }

    pub async fn run() -> Result<(), Box<dyn Error>> {
        // A file-backed database, because the feed takes its own
        // connection: an in-memory database belongs to one connection
        // and cannot be shared with a second one.
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("test-drive.db");
        let path = path.to_str().expect("the temp path is utf8");

        println!("== opening {path}");

        // One connection for the writers, one for the consumer. Two
        // connections to one file is what two processes look like, and
        // it is what the write lock serializes.
        let writer_connection = Connection::open(path).await?;
        let consumer_connection = Connection::open(path).await?;

        let store =
            SqliteEventStreams::<String, OrderPlaced>::new(writer_connection.clone(), CATEGORY);
        // Opening a store does not create the schema. The migration is
        // explicit, and idempotent.
        store.migrate().await?;
        println!("== migrated");

        // A batch handle over the same connection: a consumer writing
        // more than one stream atomically uses this, not the store.
        let database = SqliteDatabase::new(writer_connection);

        let billing = ConsumerGroup::new("billing")?;
        let audit = ConsumerGroup::new("audit")?;
        let feed = SqliteEventFeed::<OrderPlaced>::new(consumer_connection, CATEGORY);
        let two = PollLimit::new(2)?;

        // ------------------------------------------------------------
        println!("\n== 1. seed three events through the single-append path");
        // ------------------------------------------------------------
        let first = store
            .append(
                ExpectedVersion::NoStream,
                &"order-1".to_owned(),
                &batch_of(vec![order(1, 1_000), order(1, 2_500)]),
            )
            .await?;
        println!("  order-1 is at stream position {}", first.get());

        store
            .append(
                ExpectedVersion::NoStream,
                &"order-2".to_owned(),
                &batch_of(vec![order(2, 700)]),
            )
            .await?;
        println!("  order-2 is at stream position 1");
        println!("  the committed log now holds three events, positions 1-3");

        // ------------------------------------------------------------
        println!("\n== 2. the first poll delivers a page and moves the watermark");
        // ------------------------------------------------------------
        let page = feed.poll(&billing, two).await?;
        print_page("billing poll (limit 2)", &page);

        // ------------------------------------------------------------
        println!("\n== 3. acking past the watermark is rejected");
        // ------------------------------------------------------------
        let beyond = FeedPosition::new(3)?;
        match feed.ack(&billing, beyond).await {
            Err(AckError::NotDelivered(rejection)) => println!(
                "  ack of position {} rejected: the group was delivered only through {}",
                rejection.attempted().get(),
                rejection.delivered_to().get(),
            ),
            other => println!("  unexpected outcome: {other:?}"),
        }

        // ------------------------------------------------------------
        println!("\n== 4. polling again without an ack redelivers the same page");
        // ------------------------------------------------------------
        let again = feed.poll(&billing, two).await?;
        print_page("billing poll (limit 2)", &again);
        println!("  the same two entries: delivery is at-least-once");

        // ------------------------------------------------------------
        println!("\n== 5. ack advances the cursor; a regression is rejected");
        // ------------------------------------------------------------
        let two_position = FeedPosition::new(2)?;
        feed.ack(&billing, two_position).await?;
        println!("  acked position 2; the cursor is now 2");

        let one_position = FeedPosition::new(1)?;
        match feed.ack(&billing, one_position).await {
            Err(AckError::Regression(rejection)) => println!(
                "  ack of position {} rejected: the cursor already sits at {}",
                rejection.attempted().get(),
                rejection.cursor().get(),
            ),
            other => println!("  unexpected outcome: {other:?}"),
        }

        // ------------------------------------------------------------
        println!("\n== 6. the next poll starts after the cursor");
        // ------------------------------------------------------------
        let page = feed.poll(&billing, two).await?;
        print_page("billing poll (limit 2)", &page);
        feed.ack(&billing, page.last().expect("one entry").position())
            .await?;
        println!("  acked position 3");

        let empty = feed.poll(&billing, two).await?;
        print_page("billing poll (limit 2)", &empty);
        println!("  the group is caught up");

        // ------------------------------------------------------------
        println!("\n== 7. a batch writes two events atomically");
        // ------------------------------------------------------------
        let mut builder = database.batch();
        builder.write(
            CATEGORY,
            &"order-3".to_owned(),
            ExpectedVersion::NoStream,
            &batch_of(vec![order(3, 4_200), order(3, 4_300)]),
        )?;
        // A constraint asserts something about a stream the batch does
        // not write; it is checked under the same lock as the writes.
        builder.require(
            CATEGORY,
            &"order-1".to_owned(),
            BatchConstraint::StreamExists,
        );
        database.transact(builder.build()?).await?;
        println!("  batch committed: order-3 gained two events, order-1 had to exist");

        // ------------------------------------------------------------
        println!("\n== 8. the batch's events arrive on the next poll");
        // ------------------------------------------------------------
        let page = feed.poll(&billing, two).await?;
        print_page("billing poll (limit 2)", &page);
        println!("  positions 4 and 5, in commit order");

        // Ack both, oldest first, as a consumer draining a page would.
        for entry in &page {
            feed.ack(&billing, entry.position()).await?;
            println!("  acked position {}", entry.position().get());
        }

        // ------------------------------------------------------------
        println!("\n== 9. a second group has its own cursor");
        // ------------------------------------------------------------
        let audit_page = feed.poll(&audit, PollLimit::new(10)?).await?;
        print_page("audit poll (limit 10)", &audit_page);
        println!(
            "  {} entries from position 1: billing's acks never moved audit",
            audit_page.len()
        );

        // ------------------------------------------------------------
        println!("\n== 10. paging is the consumer's loop");
        // ------------------------------------------------------------
        println!("  draining audit two at a time, acknowledging each page");
        let drained = drain(&feed, &audit, two).await?;
        println!("  audit drained {drained} entries");

        // ------------------------------------------------------------
        println!("\n== 11. after a drain, both groups are caught up");
        // ------------------------------------------------------------
        for (name, group) in [("billing", &billing), ("audit", &audit)] {
            let page = feed.poll(group, two).await?;
            println!("  {name}: {} entries waiting", page.len());
        }

        println!("\n== done");
        Ok(())
    }
}

#[cfg(feature = "sqlite")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    drive::run().await
}

#[cfg(not(feature = "sqlite"))]
fn main() {
    eprintln!(
        "This example drives the SQLite backend. Run it with:\n\
         \n    cargo run --example test_drive --features sqlite\n"
    );
    std::process::exit(1);
}
