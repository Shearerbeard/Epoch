//! The SQLite backend's conformance tests: the shared stream and feed
//! suites called unchanged, plus the cases the shared suites do not
//! reach.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, Barrier};
use tokio_rusqlite::{rusqlite, Connection};

use crate::decider::Event;
use crate::streams::batch::{BatchConstraint, TransactError};
use crate::streams::feed::spec::{
    ack_is_monotonic, ack_rejects_undelivered, delivery_carries_the_envelope,
    poll_pages_the_backlog, poll_redelivers_until_acked,
};
use crate::streams::feed::{ConsumerGroup, EventFeed, PollLimit};
use crate::streams::spec::{
    a_keyed_append_loads_back_with_its_envelope, flash_sale_sells_exactly_the_stock,
    single_event_occ_race_on_empty_stream, single_event_occ_race_on_seeded_stream, under_deadline,
};
use crate::streams::{
    AppendError, AtomicStreams, EventBatch, EventMetadata, EventStreams, ExpectedVersion,
    RecordedEvent, StreamSequence, StreamState, StreamVersion,
};

use super::batch::{SqliteBatch, SqliteBatchBuilder, SqliteDatabase, SqliteWriteError};
use super::{SqliteEventFeed, SqliteEventStreams, SqliteStreamsError};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SomethingHappened;

impl Event for SomethingHappened {
    type EntityId = ();

    fn event_type(&self) -> String {
        "SomethingHappened".to_owned()
    }

    fn get_id(&self) -> Self::EntityId {}
}

/// A migrated store over a private in-memory database. Clones share the
/// actor, so the shared suite's contention runs against one store.
async fn store() -> SqliteEventStreams<String, SomethingHappened> {
    let store = SqliteEventStreams::<String, SomethingHappened>::open_in_memory("spec")
        .await
        .expect("memory connection");
    store.migrate().await.expect("schema migrates");
    store
}

/// A writer and a feed over one fresh category, per the feed suite's
/// fresh-store assumption: count assertions read the whole category
/// tail.
async fn writer_and_feed() -> (
    SqliteEventStreams<String, SomethingHappened>,
    SqliteEventFeed<SomethingHappened>,
) {
    let category = format!("feed-spec-{}", rusty_ulid::generate_ulid_string());
    let writer = SqliteEventStreams::<String, SomethingHappened>::open_in_memory(&category)
        .await
        .expect("memory connection");
    writer.migrate().await.expect("schema migrates");
    let feed = SqliteEventFeed::new(writer.connection.clone(), &category);
    (writer, feed)
}

/// An event carrying a payload, so a slice's contents can be told
/// apart from one another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Counted(u32);

impl Event for Counted {
    type EntityId = ();

    fn event_type(&self) -> String {
        "Counted".to_owned()
    }

    fn get_id(&self) -> Self::EntityId {}
}

/// An event whose serialization fails, so the builder's encoding error
/// is reachable.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RefusesToSerialize;

impl Serialize for RefusesToSerialize {
    fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        Err(serde::ser::Error::custom("this event refuses to serialize"))
    }
}

impl Event for RefusesToSerialize {
    type EntityId = ();

    fn event_type(&self) -> String {
        "RefusesToSerialize".to_owned()
    }

    fn get_id(&self) -> Self::EntityId {}
}

fn one_event() -> EventBatch<SomethingHappened> {
    EventBatch::new(vec![SomethingHappened]).expect("one event is nonempty")
}

fn unique_key(prefix: &str) -> String {
    format!("{prefix}-{}", rusty_ulid::generate_ulid_string())
}

/// A temporary database file, for the fixtures that need two
/// independent actors on one database.
fn temp_database() -> tempfile::NamedTempFile {
    tempfile::NamedTempFile::new().expect("temp file")
}

/// The SQL trace sink. SQLite's trace callback is a plain function
/// pointer, so it reaches the channel through this global. Only the
/// lock-order case installs a trace.
static SQL_TRACE: Mutex<Option<std::sync::mpsc::Sender<String>>> = Mutex::new(None);

/// Report every statement a traced connection begins running.
fn trace_statement(event: rusqlite::trace::TraceEvent<'_>) {
    if let rusqlite::trace::TraceEvent::Stmt(_, sql) = event {
        let sender = SQL_TRACE.lock().ok().and_then(|guard| guard.clone());
        if let Some(sender) = sender {
            let _ = sender.send(sql.to_owned());
        }
    }
}

/// Trace the statements a connection begins running, so a test can
/// see the order two connections ran their statements in.
async fn trace_statements(connection: &Connection) {
    connection
        .clone()
        .call(|conn| {
            conn.trace_v2(
                rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
                Some(trace_statement),
            );
            Ok::<(), rusqlite::Error>(())
        })
        .await
        .expect("the trace installs");
}

/// The busy timeout the connection carries right now: the pragma
/// reads back what the last operation installed, which is the
/// operation's own bound and not the handle's report of it.
async fn installed_busy_timeout(store: &SqliteEventStreams<String, SomethingHappened>) -> i64 {
    store
        .connection
        .clone()
        .call(|conn| conn.query_row("PRAGMA busy_timeout", [], |row| row.get::<_, i64>(0)))
        .await
        .expect("the busy timeout reads back")
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_single_event_occ_race_on_empty_stream() {
    let store = store().await;
    under_deadline(single_event_occ_race_on_empty_stream(
        store,
        str::to_owned,
        || SomethingHappened,
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_single_event_occ_race_on_seeded_stream() {
    let store = store().await;
    under_deadline(single_event_occ_race_on_seeded_stream(
        store,
        str::to_owned,
        || SomethingHappened,
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_flash_sale_sells_exactly_the_stock() {
    let store = store().await;
    under_deadline(flash_sale_sells_exactly_the_stock(
        store,
        str::to_owned,
        || SomethingHappened,
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_a_keyed_append_loads_back_with_its_envelope() {
    let store = store().await;
    under_deadline(a_keyed_append_loads_back_with_its_envelope(
        store,
        str::to_owned,
        || SomethingHappened,
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_poll_redelivers_until_acked() {
    let (writer, feed) = writer_and_feed().await;
    poll_redelivers_until_acked(writer, feed, str::to_owned, || SomethingHappened).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_poll_pages_the_backlog() {
    let (writer, feed) = writer_and_feed().await;
    poll_pages_the_backlog(writer, feed, str::to_owned, || SomethingHappened).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_ack_is_monotonic() {
    let (writer, feed) = writer_and_feed().await;
    ack_is_monotonic(writer, feed, str::to_owned, || SomethingHappened).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_ack_rejects_undelivered() {
    let (writer, feed) = writer_and_feed().await;
    ack_rejects_undelivered(writer, feed, str::to_owned, || SomethingHappened).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_delivery_carries_the_envelope() {
    let (writer, feed) = writer_and_feed().await;
    delivery_carries_the_envelope(writer, feed, str::to_owned, || SomethingHappened).await;
}

/// A slice from a cursor is inclusive of the cursor's own position,
/// and observes the stream's head - not the cursor - as its position.
/// The shared suite only ever calls `load_stream_from` with `None`, so
/// an implementation that ignored the cursor entirely would pass every
/// shared case.
#[tokio::test(flavor = "multi_thread")]
async fn a_slice_from_a_cursor_is_inclusive() {
    let store = SqliteEventStreams::<String, Counted>::open_in_memory("slice")
        .await
        .expect("memory connection");
    store.migrate().await.expect("schema migrates");
    let key = unique_key("slice");
    store
        .append(
            ExpectedVersion::NoStream,
            &key,
            &EventBatch::new(vec![Counted(1), Counted(2), Counted(3)])
                .expect("three events are nonempty"),
        )
        .await
        .expect("append succeeds");

    let cursor = StreamSequence::new(2).expect("2 is a position");
    let slice = store
        .load_stream_from(&key, Some(cursor))
        .await
        .expect("slice load succeeds");
    assert_eq!(
        slice.records().len(),
        2,
        "the cursor's own position is included"
    );
    let events: Vec<&Counted> = slice
        .records()
        .iter()
        .map(|record| record.event())
        .collect();
    assert_eq!(
        events,
        vec![&Counted(2), &Counted(3)],
        "the slice holds exactly the suffix from the cursor"
    );
    assert_eq!(
        slice.at(),
        StreamVersion::Exact(StreamSequence::new(3).expect("3 is a position")),
        "the slice observes the stream's head, not the cursor"
    );
}

/// A cursor past the tail reads nothing and still reports the stream's
/// head: an empty slice at an exact position is valid.
#[tokio::test(flavor = "multi_thread")]
async fn a_slice_past_the_tail_is_empty_at_the_head() {
    let store = store().await;
    let key = unique_key("past-tail");
    store
        .append(
            ExpectedVersion::NoStream,
            &key,
            &EventBatch::new(vec![SomethingHappened; 2]).expect("two events are nonempty"),
        )
        .await
        .expect("append succeeds");

    let past = StreamSequence::new(9).expect("9 is a position");
    let slice = store
        .load_stream_from(&key, Some(past))
        .await
        .expect("slice load succeeds");
    assert!(slice.records().is_empty(), "nothing sits past the tail");
    assert_eq!(
        slice.at(),
        StreamVersion::Exact(StreamSequence::new(2).expect("2 is a position")),
        "a cursor past the tail still observes the stream's head"
    );
}

/// A missing stream is two different answers by two different methods:
/// the full load says `Missing` and never errors, while the slice is
/// empty and observes `NoStream`.
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_stream_loads_missing_and_slices_empty() {
    let store = store().await;
    let key = unique_key("never-written");

    assert_eq!(
        store.load_stream(&key).await.expect("load succeeds"),
        StreamState::Missing,
        "a never-written stream loads Missing, never an error"
    );

    let slice = store
        .load_stream_from(&key, None)
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

/// The round trip the shared suite never makes: a category read parses
/// the stored key back into the typed id (ADR 0004's two-way
/// contract).
#[tokio::test(flavor = "multi_thread")]
async fn a_category_read_returns_the_typed_id() {
    let store = store().await;
    let key = unique_key("round-trip");
    store
        .append(ExpectedVersion::NoStream, &key, &one_event())
        .await
        .expect("append succeeds");

    let category = store.load_category().await.expect("category read succeeds");
    assert_eq!(category.len(), 1);
    assert_eq!(
        category[0].id, key,
        "the stored key parses back into the typed id"
    );
    assert_eq!(category[0].event, SomethingHappened);
}

/// The envelope a keyed append stores is the envelope the loaded
/// record carries.
#[tokio::test(flavor = "multi_thread")]
async fn the_envelope_written_is_the_envelope_stored() {
    let store = store().await;
    let key = unique_key("envelope");
    let mut envelope = EventMetadata::new();
    envelope.insert("intent", "saga-1/order-5/2");
    let batch = EventBatch::from_records(vec![
        RecordedEvent::new(SomethingHappened),
        RecordedEvent::keyed(SomethingHappened, envelope),
    ])
    .expect("two records are nonempty");

    store
        .append(ExpectedVersion::NoStream, &key, &batch)
        .await
        .expect("append succeeds");

    let stored = store.load_stream(&key).await.expect("load succeeds");
    match stored {
        StreamState::Present(records) => {
            assert!(records.records()[0].metadata().is_empty());
            assert_eq!(
                records.records()[1].metadata().get("intent"),
                Some("saga-1/order-5/2")
            );
        }
        StreamState::Missing => panic!("the appended stream must be present"),
    }
}

/// The database's write lock is what serializes two writers, not the
/// actor. The shared OCC cases clone one store, and a SQLite clone
/// shares one actor, so they prove the actor serializes the racers.
/// This fixture opens two separately opened actors on one temporary
/// database file: the loser waits out the busy timeout and then loses
/// the version check, because the database's write lock is what made
/// it wait.
#[tokio::test(flavor = "multi_thread")]
async fn independent_actors_conflict_deterministically() {
    under_deadline(async {
        let file = temp_database();
        let path = file.path().to_str().expect("utf8 path").to_owned();

        let first = SqliteEventStreams::<String, SomethingHappened>::open(&path, "race")
            .await
            .expect("open the first actor");
        let second = SqliteEventStreams::<String, SomethingHappened>::open(&path, "race")
            .await
            .expect("open the second actor");
        first.migrate().await.expect("schema migrates");
        second.migrate().await.expect("schema migrates");

        let key = unique_key("independent");
        let event = one_event();
        let (left, right) = tokio::join!(
            first.append(ExpectedVersion::NoStream, &key, &event),
            second.append(ExpectedVersion::NoStream, &key, &event),
        );
        match (left, right) {
            (Ok(_), Err(AppendError::Conflict(_))) | (Err(AppendError::Conflict(_)), Ok(_)) => {}
            (left, right) => {
                panic!("expected exactly one winner and one conflict: {left:?}, {right:?}")
            }
        }
        let stored = first.load_stream(&key).await.expect("load succeeds");
        assert_eq!(
            stored.version(),
            StreamVersion::Exact(StreamSequence::new(1).expect("1 is a position")),
            "only the winning append landed"
        );
    })
    .await;
}

/// The database's write lock is what a contender waits for, and what
/// makes it observe the committed head rather than a stale one.
///
/// A holder takes the write lock and commits a row for the contested
/// stream; a rival append started while the holder owns the lock must
/// wait for it, and after the holder commits the rival must observe
/// the holder's row and conflict rather than write beside it.
///
/// The order is checked at the statement level, not at the task level:
/// both connections are traced, and the rival's head read must appear
/// after the holder's commit in the trace. A backend that read the
/// head before taking the write lock would show that read first, and
/// the case would fail.
#[tokio::test(flavor = "multi_thread")]
async fn a_contender_waits_for_the_write_lock_and_sees_the_committed_head() {
    under_deadline(async {
        let (sender, receiver) = std::sync::mpsc::channel::<String>();
        *SQL_TRACE.lock().expect("the trace sink locks") = Some(sender);

        let file = temp_database();
        let path = file.path().to_str().expect("utf8 path").to_owned();

        let store = SqliteEventStreams::<String, SomethingHappened>::open(&path, "contender")
            .await
            .expect("open");
        store.migrate().await.expect("schema migrates");
        trace_statements(&store.connection).await;
        let key = unique_key("contended");

        // The holder: a second connection that takes the write lock and
        // commits a row for the contested stream. It says so once the
        // lock is taken, and holds the lock until the test releases it.
        let (locked, locking) = oneshot::channel();
        let (releasing, released) = std::sync::mpsc::channel::<()>();
        let holder = Connection::open(&path).await.expect("a second connection");
        trace_statements(&holder).await;
        let holder_key = key.clone();
        let holder_task = tokio::spawn(async move {
            holder
                .call(move |conn| {
                    let transaction =
                        conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                    transaction.execute(
                        "INSERT INTO stream_events \
                         (category, stream_key, event_type, sequence, event_data, event_metadata) \
                         VALUES ('contender', ?1, 'SomethingHappened', 1, 'null', '{}')",
                        [&holder_key],
                    )?;
                    let _ = locked.send(());
                    let _ = released.recv();
                    transaction.commit()
                })
                .await
        });

        locking
            .await
            .expect("the holder takes the database write lock");

        // The rival: it cannot get the write lock while the holder owns
        // it, so it must wait.
        let rival = {
            let store = store.clone();
            let key = key.clone();
            tokio::spawn(async move {
                store
                    .append(ExpectedVersion::NoStream, &key, &one_event())
                    .await
            })
        };

        // Release the holder, so its row commits.
        releasing.send(()).expect("the holder releases");
        holder_task
            .await
            .expect("the holder task")
            .expect("the holder commits");
        let outcome = rival.await.expect("the rival task");

        assert!(
            matches!(outcome, Err(AppendError::Conflict(_))),
            "the rival observed the committed head and conflicted, got {outcome:?}"
        );

        // The trace shows the order the two connections ran their
        // statements in. The rival's head read must come after the
        // holder's commit: a backend that read the head before taking
        // the write lock would show that read first.
        let traced: Vec<String> = receiver.try_iter().collect();
        let commit = traced
            .iter()
            .position(|sql| sql == "COMMIT")
            .expect("the holder's commit is traced");
        let head_read = traced
            .iter()
            .position(|sql| sql.contains("SELECT"))
            .expect("the rival's head read is traced");
        assert!(
            head_read > commit,
            "the head was read under the write lock, not before it: {traced:?}"
        );
    })
    .await;
}
#[tokio::test(flavor = "multi_thread")]
async fn a_cancelled_append_still_commits() {
    let store = store().await;
    let key = unique_key("cancelled");
    let event = one_event();

    // The actor is held behind a blocker, so the append's submission
    // is observable: the queued closure cannot run until the blocker
    // releases it, and the append cannot be ready before then.
    let (releasing, released) = std::sync::mpsc::channel::<()>();
    let blocker_connection = store.connection.clone();
    let mut blocker = Box::pin(blocker_connection.call(move |_conn| {
        let _ = released.recv();
        Ok::<(), rusqlite::Error>(())
    }));
    let polled = futures::poll!(&mut blocker);
    assert!(polled.is_pending(), "the blocker is queued on the actor");

    let mut append = Box::pin(store.append(ExpectedVersion::NoStream, &key, &event));
    let polled = futures::poll!(&mut append);
    assert!(
        polled.is_pending(),
        "the append is still in flight after the work was enqueued"
    );
    drop(append);

    // Release the actor, so the queued append runs.
    releasing.send(()).expect("the blocker releases");
    blocker.await.expect("the blocker resolves");

    // The load goes through the same actor, and the actor runs closures
    // in the order they were enqueued, so the load sees whatever the
    // cancelled append committed.
    let stored = store.load_stream(&key).await.expect("load succeeds");
    assert_eq!(
        stored.version(),
        StreamVersion::Exact(StreamSequence::new(1).expect("1 is a position")),
        "the cancelled append committed anyway"
    );
}

/// Hold the database's write lock from a second, independent
/// connection, so a writer on the store's own connection has to wait.
/// The signal fires once the lock is taken.
async fn wedge_the_write_lock(path: &str, held: oneshot::Sender<()>) {
    let connection = Connection::open(path).await.expect("a second connection");
    tokio::spawn(async move {
        let _ = connection
            .call(move |conn| {
                let transaction =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let _ = held.send(());
                // Hold the lock long enough for the bounded waits under
                // test to expire, then release it.
                std::thread::sleep(Duration::from_secs(2));
                transaction.rollback()
            })
            .await;
    });
}

/// Wait for the wedge to report that it holds the write lock, with a
/// deadline, so a wedge that never takes the lock fails the test
/// instead of hanging it.
async fn await_the_wedge(waiting: oneshot::Receiver<()>) {
    tokio::time::timeout(Duration::from_secs(10), waiting)
        .await
        .expect("the wedge takes the write lock within the deadline")
        .expect("the wedge reports that it holds the write lock");
}

/// A wedged writer is a retryable timeout on the append path, never a
/// hang and never a conflict. The payload names the effective bound,
/// not the wall-clock time.
#[tokio::test(flavor = "multi_thread")]
async fn a_wedged_writer_is_a_retryable_append_timeout() {
    under_deadline(async {
        let file = temp_database();
        let path = file.path().to_str().expect("utf8 path").to_owned();

        let store = SqliteEventStreams::<String, SomethingHappened>::open(&path, "wedge")
            .await
            .expect("open");
        store.migrate().await.expect("schema migrates");
        let bounded = store.clone().with_lock_timeout(Duration::from_millis(100));

        let (held, waiting) = oneshot::channel();
        wedge_the_write_lock(&path, held).await;
        await_the_wedge(waiting).await;

        let key = unique_key("wedged");
        let outcome = bounded
            .append(ExpectedVersion::NoStream, &key, &one_event())
            .await;
        assert!(
            matches!(outcome, Err(AppendError::LockTimeout(reported)) if reported == Duration::from_millis(100)),
            "expected a retryable lock timeout, got {outcome:?}"
        );
        assert_eq!(
            store.load_stream(&key).await.expect("load succeeds"),
            StreamState::Missing,
            "a timed-out append stores nothing"
        );
    })
    .await;
}

/// A zero configured bound is floored to 1ms, and the timeout payload
/// reports that effective bound, not the zero the caller configured.
#[tokio::test(flavor = "multi_thread")]
async fn a_zero_bound_times_out_reporting_the_floor() {
    under_deadline(async {
        let file = temp_database();
        let path = file.path().to_str().expect("utf8 path").to_owned();

        let store = SqliteEventStreams::<String, SomethingHappened>::open(&path, "zero")
            .await
            .expect("open");
        store.migrate().await.expect("schema migrates");
        let bounded = store.clone().with_lock_timeout(Duration::ZERO);

        let (held, waiting) = oneshot::channel();
        wedge_the_write_lock(&path, held).await;
        await_the_wedge(waiting).await;

        let key = unique_key("zero");
        let outcome = bounded
            .append(ExpectedVersion::NoStream, &key, &one_event())
            .await;
        assert!(
            matches!(outcome, Err(AppendError::LockTimeout(reported)) if reported == Duration::from_millis(1)),
            "expected the floored 1ms bound, got {outcome:?}"
        );
    })
    .await;
}

/// The busy timeout is a property of the connection, and cloned handles
/// share one connection. Each operation installs its own bound inside
/// its own closure, so two handles over one connection each time out on
/// their own bound. The shared suite's clone-based cases cannot see
/// this: one bound covers the whole store.
#[tokio::test(flavor = "multi_thread")]
async fn each_handle_times_out_on_its_own_bound() {
    under_deadline(async {
        let file = temp_database();
        let path = file.path().to_str().expect("utf8 path").to_owned();

        let store = SqliteEventStreams::<String, SomethingHappened>::open(&path, "bounds")
            .await
            .expect("open");
        store.migrate().await.expect("schema migrates");
        let quick = store.clone().with_lock_timeout(Duration::from_millis(100));
        let slow = store.clone().with_lock_timeout(Duration::from_millis(600));

        let (held, waiting) = oneshot::channel();
        wedge_the_write_lock(&path, held).await;
        await_the_wedge(waiting).await;

        let quick_outcome = quick
            .append(ExpectedVersion::NoStream, &unique_key("quick"), &one_event())
            .await;
        assert!(
            matches!(quick_outcome, Err(AppendError::LockTimeout(reported)) if reported == Duration::from_millis(100)),
            "the quick handle times out on its own bound, got {quick_outcome:?}"
        );
        assert_eq!(
            installed_busy_timeout(&store).await,
            100,
            "the quick handle's bound is the one installed on the connection"
        );

        let slow_outcome = slow
            .append(ExpectedVersion::NoStream, &unique_key("slow"), &one_event())
            .await;
        assert!(
            matches!(slow_outcome, Err(AppendError::LockTimeout(reported)) if reported == Duration::from_millis(600)),
            "the slow handle times out on its own bound, got {slow_outcome:?}"
        );
        assert_eq!(
            installed_busy_timeout(&store).await,
            600,
            "the slow handle replaced the connection's bound with its own"
        );
    })
    .await;
}

/// Opening a store does not create the schema (E12's rule): a read
/// against an unmigrated database is a driver fault, not an empty
/// result.
#[tokio::test(flavor = "multi_thread")]
async fn an_unmigrated_database_is_a_driver_fault() {
    let store = SqliteEventStreams::<String, SomethingHappened>::open_in_memory("unmigrated")
        .await
        .expect("memory connection");
    let outcome = store.load_stream(&unique_key("k")).await;
    assert!(
        matches!(outcome, Err(SqliteStreamsError::Connection(_))),
        "an unmigrated database is a driver fault, got {outcome:?}"
    );
}

/// A closed connection is a handle failure, not a driver fault.
#[tokio::test(flavor = "multi_thread")]
async fn a_closed_connection_is_a_handle_failure() {
    let connection = Connection::open_in_memory()
        .await
        .expect("memory connection");
    let store = SqliteEventStreams::<String, SomethingHappened>::new(connection.clone(), "closed");
    connection.close().await.expect("the connection closes");

    let outcome = store.load_stream(&unique_key("k")).await;
    assert!(
        matches!(outcome, Err(SqliteStreamsError::Handle(_))),
        "a closed connection is a handle failure, got {outcome:?}"
    );
}

/// A stored payload the event type cannot decode is a serialization
/// error, not a silent empty stream.
#[tokio::test(flavor = "multi_thread")]
async fn a_stored_payload_that_does_not_decode_is_a_serialization_error() {
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct NeedsAnObject {
        name: String,
    }

    impl Event for NeedsAnObject {
        type EntityId = ();

        fn event_type(&self) -> String {
            "NeedsAnObject".to_owned()
        }

        fn get_id(&self) -> Self::EntityId {}
    }

    let file = temp_database();
    let path = file.path().to_str().expect("utf8 path").to_owned();
    let writer = SqliteEventStreams::<String, SomethingHappened>::open(&path, "decode")
        .await
        .expect("open");
    writer.migrate().await.expect("schema migrates");
    let key = unique_key("k");
    writer
        .append(ExpectedVersion::NoStream, &key, &one_event())
        .await
        .expect("append succeeds");

    let reader = SqliteEventStreams::<String, NeedsAnObject>::open(&path, "decode")
        .await
        .expect("open");
    let outcome = reader.load_stream(&key).await;
    assert!(
        matches!(outcome, Err(SqliteStreamsError::Serialization(_))),
        "a payload that does not decode is a serialization error, got {outcome:?}"
    );
}

/// A stored key the typed id cannot parse back is an invalid key, not
/// a backend fault (ADR 0004's two-way contract).
#[tokio::test(flavor = "multi_thread")]
async fn a_stored_key_that_does_not_parse_is_an_invalid_key() {
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct StrictId(String);

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct UnparsableKey;

    impl std::fmt::Display for UnparsableKey {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "the stored key does not parse into the typed id")
        }
    }

    impl std::error::Error for UnparsableKey {}

    impl crate::streams::StreamId for StrictId {
        type ParseError = UnparsableKey;

        fn stream_key(&self) -> String {
            self.0.clone()
        }

        fn parse_key(key: &str) -> Result<Self, Self::ParseError> {
            key.strip_prefix("ok-")
                .map(|rest| StrictId(format!("ok-{rest}")))
                .ok_or(UnparsableKey)
        }
    }

    let file = temp_database();
    let path = file.path().to_str().expect("utf8 path").to_owned();
    let store = SqliteEventStreams::<StrictId, SomethingHappened>::open(&path, "strict")
        .await
        .expect("open");
    store.migrate().await.expect("schema migrates");

    // A row the typed id cannot parse back: the key does not carry the
    // prefix this id type writes.
    let connection = Connection::open(&path).await.expect("a raw connection");
    connection
        .call(move |conn| {
            conn.execute(
                "INSERT INTO stream_events \
                 (category, stream_key, event_type, sequence, event_data, event_metadata) \
                 VALUES ('strict', 'not-a-parsable-key', 'SomethingHappened', 1, 'null', '{}')",
                [],
            )
        })
        .await
        .expect("the raw insert succeeds");

    let outcome = store.load_category().await;
    assert!(
        matches!(outcome, Err(crate::streams::LoadError::InvalidKey(_))),
        "a stored key that does not parse is an invalid key, got {outcome:?}"
    );
}

// The batch conformance set.
//
// Two cases from the in-memory backend's suite are deliberately absent:
// `a_batch_cannot_contradict_a_category_event_type` and
// `two_writes_cannot_disagree_over_an_unclaimed_category`. Both enforce
// the in-memory backend's Rust `TypeId` category registry, which neither
// the PostgreSQL backend nor this one has: a SQLite row carries an
// event-type string, not a type identity. The coverage manifest in
// `src/streams/sqlite/DESIGN.md` records the exclusion and its reason.

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct KidHoldsCard;

impl Event for KidHoldsCard {
    type EntityId = ();

    fn event_type(&self) -> String {
        "KidHoldsCard".to_owned()
    }

    fn get_id(&self) -> Self::EntityId {}
}

/// A second, unrelated event type: a batch spans categories whose
/// event types differ, which is ADR 0006's mixed-type driver.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CardAssigned;

impl Event for CardAssigned {
    type EntityId = ();

    fn event_type(&self) -> String {
        "CardAssigned".to_owned()
    }

    fn get_id(&self) -> Self::EntityId {}
}

const KIDS: &str = "batch-kids";
const CHORES: &str = "batch-chores";
const POOL: &str = "batch-pool";
const OUTBOX: &str = "saga-outbox";

fn kid_event() -> EventBatch<KidHoldsCard> {
    EventBatch::new(vec![KidHoldsCard]).expect("a single event is nonempty")
}

fn chore_event() -> EventBatch<CardAssigned> {
    EventBatch::new(vec![CardAssigned]).expect("a single event is nonempty")
}

/// The database handle plus the two category views the cases read
/// through, each on its own actor over one temporary database file.
/// Separate actors are the point: the database's write lock is what
/// serializes these cases, not the actor's single thread.
struct Fixture {
    _file: tempfile::NamedTempFile,
    db: SqliteDatabase,
    rival_db: SqliteDatabase,
    kids: SqliteEventStreams<String, KidHoldsCard>,
    chores: SqliteEventStreams<String, CardAssigned>,
}

async fn fixture() -> Fixture {
    let file = temp_database();
    let path = file.path().to_str().expect("utf8 path").to_owned();

    let db = SqliteDatabase::new(
        Connection::open(&path)
            .await
            .expect("a database connection"),
    );
    let rival_db = SqliteDatabase::new(
        Connection::open(&path)
            .await
            .expect("a rival database connection"),
    );
    let kids = SqliteEventStreams::new(
        Connection::open(&path).await.expect("a kids connection"),
        KIDS,
    );
    let chores = SqliteEventStreams::new(
        Connection::open(&path).await.expect("a chores connection"),
        CHORES,
    );
    kids.migrate().await.expect("schema migrates");

    Fixture {
        _file: file,
        db,
        rival_db,
        kids,
        chores,
    }
}

/// A draw: one write in each of two categories, both expecting the
/// streams to be untouched. `reversed` flips the declaration order;
/// the outcome must be the same either way.
fn draw(db: &SqliteDatabase, kid: &str, chore: &str, reversed: bool) -> SqliteBatch {
    let mut builder = db.batch();
    let write_kid = |builder: &mut SqliteBatchBuilder| {
        builder
            .write(
                KIDS,
                &kid.to_owned(),
                ExpectedVersion::NoStream,
                &kid_event(),
            )
            .expect("one write per stream, encodable")
    };
    let write_chore = |builder: &mut SqliteBatchBuilder| {
        builder
            .write(
                CHORES,
                &chore.to_owned(),
                ExpectedVersion::NoStream,
                &chore_event(),
            )
            .expect("one write per stream, encodable")
    };
    if reversed {
        write_chore(&mut builder);
        write_kid(&mut builder);
    } else {
        write_kid(&mut builder);
        write_chore(&mut builder);
    }
    builder.build().expect("the batch has writes")
}

async fn kid_state(fixture: &Fixture, kid: &str) -> StreamState<KidHoldsCard> {
    fixture
        .kids
        .load_stream(&kid.to_owned())
        .await
        .expect("load succeeds")
}

async fn chore_state(fixture: &Fixture, chore: &str) -> StreamState<CardAssigned> {
    fixture
        .chores
        .load_stream(&chore.to_owned())
        .await
        .expect("load succeeds")
}

/// Two batches want the same two streams and declare them in opposite
/// orders: exactly one wins, the other conflicts on the shared
/// pre-batch head, and no partial rows survive. Both batches are built
/// before either transacts and the two tasks release together on a
/// barrier, so the transactions genuinely contend on the write lock
/// rather than one finishing before the other begins.
#[tokio::test(flavor = "multi_thread")]
async fn overlapping_batches_leave_exactly_one_winner() {
    under_deadline(async {
        let fixture = fixture().await;
        for _ in 0..10 {
            let (kid, chore) = (unique_key("kid"), unique_key("chore"));
            let first = draw(&fixture.db, &kid, &chore, false);
            let second = draw(&fixture.rival_db, &kid, &chore, true);

            let barrier = Arc::new(Barrier::new(2));
            let racer = |batch: SqliteBatch, db: SqliteDatabase| {
                let barrier = Arc::clone(&barrier);
                tokio::spawn(async move {
                    barrier.wait().await;
                    db.transact(batch).await
                })
            };

            let (a, b) = tokio::join!(
                racer(first, fixture.db.clone()),
                racer(second, fixture.rival_db.clone())
            );
            let a = a.expect("racer task must not panic");
            let b = b.expect("racer task must not panic");
            match (a, b) {
                (Ok(()), Err(TransactError::Conflict(_)))
                | (Err(TransactError::Conflict(_)), Ok(())) => {}
                (a, b) => {
                    panic!("expected exactly one winner and one conflict: {a:?}, {b:?}")
                }
            }

            // One winner wrote each stream once; the loser wrote
            // neither, so no partial rows survived.
            assert_eq!(
                kid_state(&fixture, &kid).await,
                StreamState::Present(kid_event())
            );
            assert_eq!(
                chore_state(&fixture, &chore).await,
                StreamState::Present(chore_event())
            );
        }
    })
    .await;
}

/// A violated constraint rolls the whole batch back: no partial rows
/// survive.
#[tokio::test(flavor = "multi_thread")]
async fn a_violated_constraint_aborts_the_whole_batch() {
    under_deadline(async {
        let fixture = fixture().await;
        let (kid, chore, pinned) = (unique_key("kid"), unique_key("chore"), unique_key("pool"));

        let mut builder = fixture.db.batch();
        builder
            .write(KIDS, &kid, ExpectedVersion::NoStream, &kid_event())
            .expect("one write per stream");
        builder
            .write(CHORES, &chore, ExpectedVersion::NoStream, &chore_event())
            .expect("one write per stream");
        // A pin on a stream the batch does not write, and does not
        // hold: the whole batch must roll back.
        builder.require(POOL, &pinned, BatchConstraint::StreamExists);

        let outcome = fixture
            .db
            .transact(builder.build().expect("the batch has writes"))
            .await;
        let violation = match outcome {
            Err(TransactError::ConstraintViolated(violation)) => violation,
            other => panic!("expected a constraint violation, got {other:?}"),
        };
        assert_eq!(violation.stream().category(), POOL);
        assert_eq!(violation.observed(), StreamVersion::NoStream);

        assert_eq!(kid_state(&fixture, &kid).await, StreamState::Missing);
        assert_eq!(
            chore_state(&fixture, &chore).await,
            StreamState::Missing,
            "a violated constraint leaves no partial insert"
        );
    })
    .await;
}

/// Append parity: a batch and a single append take the same write lock
/// on the same stream and evaluate the same head, so the batch loses to
/// an append that got there first - and takes its other write down with
/// it.
#[tokio::test(flavor = "multi_thread")]
async fn a_batch_and_an_append_contend_on_one_stream() {
    under_deadline(async {
        let fixture = fixture().await;
        let (kid, chore) = (unique_key("kid"), unique_key("chore"));
        fixture
            .kids
            .append(ExpectedVersion::NoStream, &kid, &kid_event())
            .await
            .expect("the append lands first");

        let outcome = fixture
            .db
            .transact(draw(&fixture.db, &kid, &chore, false))
            .await;
        let conflict = match outcome {
            Err(TransactError::Conflict(conflict)) => conflict,
            other => panic!("expected a version conflict, got {other:?}"),
        };
        assert_eq!(conflict.stream().category(), KIDS);
        assert_eq!(
            chore_state(&fixture, &chore).await,
            StreamState::Missing,
            "the batch's other write rolled back with it"
        );
    })
    .await;
}

/// The same parity under a genuine race, which is what pins the lock
/// identity rather than just the check: a batch and a plain append go
/// for one stream released together on a barrier. Either may win -
/// they contend for the same physical write lock - but exactly one
/// does, and the batch's second stream follows its own outcome.
#[tokio::test(flavor = "multi_thread")]
async fn a_batch_and_an_append_race_for_one_stream() {
    under_deadline(async {
        let fixture = fixture().await;
        for _ in 0..10 {
            let (kid, chore) = (unique_key("kid"), unique_key("chore"));
            let batch = draw(&fixture.db, &kid, &chore, false);
            let barrier = Arc::new(Barrier::new(2));

            let batch_side = {
                let db = fixture.db.clone();
                let barrier = Arc::clone(&barrier);
                tokio::spawn(async move {
                    barrier.wait().await;
                    db.transact(batch).await
                })
            };
            let append_side = {
                let kids = fixture.kids.clone();
                let barrier = Arc::clone(&barrier);
                let kid = kid.clone();
                tokio::spawn(async move {
                    barrier.wait().await;
                    kids.append(ExpectedVersion::NoStream, &kid, &kid_event())
                        .await
                })
            };

            let (batched, appended) = tokio::join!(batch_side, append_side);
            let batched = batched.expect("batch task must not panic");
            let appended = appended.expect("append task must not panic");

            match (batched, appended) {
                (Ok(()), Err(AppendError::Conflict(_))) => assert_eq!(
                    chore_state(&fixture, &chore).await,
                    StreamState::Present(chore_event()),
                    "the batch won, so both its writes are stored"
                ),
                (Err(TransactError::Conflict(_)), Ok(_)) => assert_eq!(
                    chore_state(&fixture, &chore).await,
                    StreamState::Missing,
                    "the batch lost, so neither of its writes is stored"
                ),
                (batched, appended) => panic!(
                    "expected exactly one winner on the shared stream: \
                     {batched:?}, {appended:?}"
                ),
            }

            // Whoever won, the contested stream holds one event.
            assert_eq!(
                kid_state(&fixture, &kid).await,
                StreamState::Present(kid_event())
            );
        }
    })
    .await;
}

/// A satisfied constraint and two writes, one onto an existing stream:
/// the batch commits at the next sequence, exactly as an append would
/// have.
#[tokio::test(flavor = "multi_thread")]
async fn a_satisfied_batch_commits_every_write() {
    under_deadline(async {
        let fixture = fixture().await;
        let (kid, chore) = (unique_key("kid"), unique_key("chore"));
        fixture
            .kids
            .append(ExpectedVersion::NoStream, &kid, &kid_event())
            .await
            .expect("seed append");

        let mut builder = fixture.db.batch();
        builder
            .write(
                KIDS,
                &kid,
                ExpectedVersion::Exact(StreamSequence::new(1).expect("1 is a position")),
                &kid_event(),
            )
            .expect("one write per stream");
        builder
            .write(CHORES, &chore, ExpectedVersion::NoStream, &chore_event())
            .expect("one write per stream");
        builder.require(KIDS, &kid, BatchConstraint::StreamExists);

        fixture
            .db
            .transact(builder.build().expect("the batch has writes"))
            .await
            .expect("every check is satisfied");

        assert_eq!(
            kid_state(&fixture, &kid).await,
            StreamState::Present(
                EventBatch::new(vec![KidHoldsCard, KidHoldsCard]).expect("two events are nonempty")
            )
        );
        assert_eq!(
            chore_state(&fixture, &chore).await,
            StreamState::Present(chore_event())
        );
    })
    .await;
}

/// A keyed batch write's envelope reaches the stored row (the runner's
/// intent keys ride exactly this path).
#[tokio::test(flavor = "multi_thread")]
async fn a_keyed_batch_write_stores_its_envelope() {
    under_deadline(async {
        let fixture = fixture().await;
        let kid = unique_key("kid");
        let mut envelope = EventMetadata::new();
        envelope.insert("intent", "saga-3/draw-7/1");
        let events = EventBatch::from_records(vec![RecordedEvent::keyed(KidHoldsCard, envelope)])
            .expect("one event is nonempty");

        let mut builder = fixture.db.batch();
        builder
            .write(KIDS, &kid, ExpectedVersion::NoStream, &events)
            .expect("one write per stream");
        fixture
            .db
            .transact(builder.build().expect("the batch has writes"))
            .await
            .expect("the batch commits");

        let stored = kid_state(&fixture, &kid).await;
        match stored {
            StreamState::Present(records) => assert_eq!(
                records.records()[0].metadata().get("intent"),
                Some("saga-3/draw-7/1")
            ),
            StreamState::Missing => panic!("the keyed write must be stored"),
        }
    })
    .await;
}

/// A second write for a stream the batch already writes is a
/// build-time error, before any transaction starts.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_write_for_one_stream_is_a_build_error() {
    let fixture = fixture().await;
    let kid = unique_key("kid");
    let mut builder = fixture.db.batch();
    builder
        .write(KIDS, &kid, ExpectedVersion::NoStream, &kid_event())
        .expect("the first write for a stream is admitted");

    let outcome = builder.write(KIDS, &kid, ExpectedVersion::NoStream, &kid_event());
    assert!(
        matches!(outcome, Err(SqliteWriteError::Duplicate(_))),
        "a second write for one stream is a build error, got {outcome:?}"
    );
}

/// An event that will not encode is a build-time error too, and the
/// failed write leaves the builder without a write.
#[tokio::test(flavor = "multi_thread")]
async fn an_event_that_will_not_encode_is_a_build_error() {
    let fixture = fixture().await;
    let kid = unique_key("kid");
    let events = EventBatch::new(vec![RefusesToSerialize]).expect("one event is nonempty");

    let mut builder = fixture.db.batch();
    let outcome = builder.write(KIDS, &kid, ExpectedVersion::NoStream, &events);
    assert!(
        matches!(outcome, Err(SqliteWriteError::Encoding(_))),
        "an event that will not encode is a build error, got {outcome:?}"
    );
    assert!(
        builder.build().is_err(),
        "the failed write left the builder without a write"
    );
}

/// A duplicate intent key is rejected by the storage-level index, not
/// by a version expectation: the `Any` expectation is what makes the
/// index the rejecting mechanism. The batch rolls back whole, so its
/// other write is not stored either.
#[tokio::test(flavor = "multi_thread")]
async fn a_duplicate_intent_is_rejected_by_the_index() {
    under_deadline(async {
        let fixture = fixture().await;
        let key = unique_key("intent");
        let mut envelope = EventMetadata::new();
        envelope.insert("intent", "saga-3/draw-7/1");

        let first =
            EventBatch::from_records(vec![RecordedEvent::keyed(KidHoldsCard, envelope.clone())])
                .expect("one event is nonempty");
        let mut builder = fixture.db.batch();
        builder
            .write(OUTBOX, &key, ExpectedVersion::Any, &first)
            .expect("one write per stream");
        fixture
            .db
            .transact(builder.build().expect("the batch has writes"))
            .await
            .expect("the first batch commits");

        let second = EventBatch::from_records(vec![RecordedEvent::keyed(KidHoldsCard, envelope)])
            .expect("one event is nonempty");
        let other = unique_key("other");
        let mut builder = fixture.db.batch();
        // The successful write comes first, so a backend that stored
        // writes one at a time would leave it behind, and the rollback
        // is what this case proves.
        builder
            .write(CHORES, &other, ExpectedVersion::Any, &chore_event())
            .expect("one write per stream");
        builder
            .write(OUTBOX, &other, ExpectedVersion::Any, &second)
            .expect("one write per stream");
        let outcome = fixture
            .db
            .transact(builder.build().expect("the batch has writes"))
            .await;

        match outcome {
            Err(TransactError::DuplicateIntent(duplicate)) => {
                assert_eq!(duplicate.stream().category(), OUTBOX);
                assert_eq!(duplicate.stream().key(), other);
            }
            other => panic!("expected a duplicate intent rejection, got {other:?}"),
        }

        assert_eq!(
            chore_state(&fixture, &other).await,
            StreamState::Missing,
            "the rejected batch rolled back the write that came first"
        );
    })
    .await;
}

/// A lock the batch cannot take inside its bound is a retryable
/// timeout, not a conflict, and the timed-out batch stores nothing.
#[tokio::test(flavor = "multi_thread")]
async fn an_unavailable_lock_is_a_retryable_timeout() {
    under_deadline(async {
        let fixture = fixture().await;
        let path = fixture
            ._file
            .path()
            .to_str()
            .expect("utf8 path")
            .to_owned();
        let bounded = fixture.db.clone().with_lock_timeout(Duration::from_millis(100));

        let (held, waiting) = oneshot::channel();
        wedge_the_write_lock(&path, held).await;
        await_the_wedge(waiting).await;

        let kid = unique_key("kid");
        let outcome = bounded
            .transact(draw(&fixture.db, &kid, &unique_key("chore"), false))
            .await;
        assert!(
            matches!(outcome, Err(TransactError::LockTimeout(reported)) if reported == Duration::from_millis(100)),
            "expected a retryable lock timeout, got {outcome:?}"
        );
        assert_eq!(
            kid_state(&fixture, &kid).await,
            StreamState::Missing,
            "a timed-out batch stores nothing"
        );
    })
    .await;
}

/// A zero configured bound is floored to 1ms, and the timeout payload
/// reports that effective bound, not the zero the caller configured.
#[tokio::test(flavor = "multi_thread")]
async fn a_wedged_writer_times_out_a_feed_poll() {
    under_deadline(async {
        let file = temp_database();
        let path = file.path().to_str().expect("utf8 path").to_owned();

        let store = SqliteEventStreams::<String, SomethingHappened>::open(&path, "feed")
            .await
            .expect("open");
        store.migrate().await.expect("schema migrates");
        let feed = SqliteEventFeed::<SomethingHappened>::new(store.connection.clone(), "feed")
            .with_lock_timeout(Duration::from_millis(100));

        let (held, waiting) = oneshot::channel();
        wedge_the_write_lock(&path, held).await;
        await_the_wedge(waiting).await;

        let group = ConsumerGroup::new("wedged").expect("a non-empty group");
        let outcome = feed
            .poll(&group, PollLimit::new(10).expect("a nonzero limit"))
            .await;
        assert!(
            matches!(outcome, Err(SqliteStreamsError::LockTimeout(reported)) if reported == Duration::from_millis(100)),
            "expected a retryable lock timeout, got {outcome:?}"
        );
    })
    .await;
}

/// A zero configured bound is floored to 1ms, and the timeout payload
/// reports that effective bound, not the zero the caller configured.
#[tokio::test(flavor = "multi_thread")]
async fn a_zero_funnel_bound_times_out_reporting_the_floor() {
    under_deadline(async {
        let fixture = fixture().await;
        let path = fixture
            ._file
            .path()
            .to_str()
            .expect("utf8 path")
            .to_owned();
        let bounded = fixture.db.clone().with_lock_timeout(Duration::ZERO);

        let (held, waiting) = oneshot::channel();
        wedge_the_write_lock(&path, held).await;
        await_the_wedge(waiting).await;

        let kid = unique_key("kid");
        let outcome = bounded
            .transact(draw(&fixture.db, &kid, &unique_key("chore"), false))
            .await;
        assert!(
            matches!(outcome, Err(TransactError::LockTimeout(reported)) if reported == Duration::from_millis(1)),
            "expected the floored 1ms bound, got {outcome:?}"
        );
        assert_eq!(
            kid_state(&fixture, &kid).await,
            StreamState::Missing,
            "a timed-out batch stores nothing"
        );
    })
    .await;
}
