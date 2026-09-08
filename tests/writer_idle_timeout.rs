//! C4 (idle writer) repair proofs: the library write paths -
//! `PgEventStreams::append` and `PgDatabase::transact` - evict a
//! writer that goes idle inside its event transaction, and the
//! eviction surfaces as a session fault, never the retryable funnel
//! timeout. Runs against the live compose postgres:
//!
//! ```sh
//! cargo test --features postgres --test writer_idle_timeout -- --nocapture
//! ```
//!
//! The idle bound is applied by the library's own
//! `configure_writer_timeouts` path; the test observes only
//! server-side consequences. A raw holder takes the single-writer
//! funnel lock; the library pool is `max_size(1)` and its backend's
//! pid is read before the pinned write future reuses it; the pinned
//! future is selected against a bounded `pg_locks` probe until that
//! exact pid is the funnel's ungranted waiter, then polling stops -
//! the future is retained, never dropped or cancelled - while the
//! holder rolls back. The tokio_postgres driver, independent of the
//! unpolled future, completes the lock query, leaving the backend
//! idle in transaction; bounded probes observe the grant, the
//! `idle in transaction` state, and the backend's disappearance at
//! the bound's expiry. The resumed future then fails as
//! `Backend(PgStreamsError::Connection(_))`, never `LockTimeout`, and
//! the inner SQLSTATE is not the retryable `55P03`; the termination's
//! own code is unpinned (the server may report `25P03`, or the
//! failure may arrive as an already-closed connection with no
//! SQLSTATE). The exact-pid expiry observation is the causal proof;
//! the assertion only pins the shape. Nothing may be stored, and a
//! fresh operation must succeed through the pool's replacement
//! connection. The whole case runs under `under_deadline`'s 60s
//! outer bound, and every probe polls under its own short deadline.
//!
//! Honesty notes: the 30-second default is not tested by waiting 30
//! seconds - the default field is private, so the wiring proof is the
//! configured 1s bound through a CLONE of the configured handle. The
//! config floor and the default itself remain unit-test material.
//! Active SQL, commit, and total client duration are outside this
//! bound by design.

#![cfg(feature = "postgres")]

use std::future::Future;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use epoch::streams::postgres::PgPool;
use epoch::streams::spec::under_deadline;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct IdleWriter;

impl epoch::decider::Event for IdleWriter {
    type EntityId = ();

    fn event_type(&self) -> String {
        "IdleWriter".to_owned()
    }

    fn get_id(&self) -> Self::EntityId {}
}

/// The test's own spelling of the crate-private single-writer funnel
/// key (`WRITER_LOCK`, the big-endian bytes of "epwriter"): the raw
/// holder must present the same key the library locks, or the probes
/// would watch a lock no library writer ever waits on.
const WRITER_LOCK: i64 = i64::from_be_bytes(*b"epwriter");

/// The explicitly configured idle bound under test - short enough that
/// the eviction window is seconds, never the 30s default.
const IDLE_BOUND: Duration = Duration::from_secs(1);

/// Every probe query the choreography runs is bounded in its own
/// right: a stalled probe fails its phase in seconds instead of
/// leaning on the case's outer 60s deadline (mandatory - the outer
/// bound is not the probe bound).
const PROBE_DEADLINE: Duration = Duration::from_secs(5);

/// Each choreography phase - waiter, grant+idle, expiry - is bounded
/// overall, so a regression fails fast with a diagnosis instead of
/// hanging the case.
const PHASE_DEADLINE: Duration = Duration::from_secs(5);

/// Slack over the configured idle bound for the expiry phase: the
/// server fires at the bound, and the probes need seconds - not
/// milliseconds - to observe the aftermath.
const EXPIRY_SLACK: Duration = Duration::from_secs(5);

/// How often the probes re-read server state between sleeps. A poll
/// spacing, never evidence: no sleep ever poses as an observation.
const PROBE_INTERVAL: Duration = Duration::from_millis(10);

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

/// Stream keys and categories carry a ULID nonce, so runs never
/// collide and storage is never cleared.
fn unique_category(prefix: &str) -> String {
    format!("{prefix}-{}", rusty_ulid::generate_ulid_string())
}

/// The max-size-1 library pool with its single backend named: read
/// `pg_backend_pid()` from the one pooled connection and return it to
/// the pool. With `max_size(1)` that same session is the one the
/// pinned write future drives, which is what lets the pg_locks and
/// pg_stat_activity probes name the library's own backend.
async fn single_writer_pool() -> (PgPool, i32) {
    use bb8_postgres::PostgresConnectionManager;
    use tokio_postgres::NoTls;

    let config: tokio_postgres::Config = conn_str().parse().expect("valid connection string");
    let pool = PgPool::builder()
        .max_size(1)
        .build(PostgresConnectionManager::new(config, NoTls))
        .await
        .expect("the single-writer pool builds");
    let conn = pool.get().await.expect("the pool's single connection");
    let pid: i32 = conn
        .query_one("SELECT pg_backend_pid() AS pid", &[])
        .await
        .expect("the single backend's pid")
        .get("pid");
    // Returned to the pool BEFORE the caller creates the pinned write
    // future, so the future leases this exact backend.
    drop(conn);
    (pool, pid)
}

/// The eviction choreography shared by both write paths - only the
/// pinned future differs, so one generic helper, no boolean-mode flag.
/// `pinned` is the caller's `Box::pin`ned library write future
/// (`append` or `transact`); the helper polls it only until the
/// library backend is provably the funnel's waiter, then stops
/// without dropping it, so the caller can resume it for the outcome
/// assertions after the eviction.
async fn hold_then_starve_then_expire(
    writer_pid: i32,
    pinned: &mut (impl Future + Unpin),
    idle_bound: Duration,
) {
    use tokio_postgres::NoTls;

    let conn_str = conn_str();

    // The holder and the monitor ride their own raw connections - the
    // max-size-1 library pool's only backend belongs to the pinned
    // future and is never touched here. Each connect is bounded, and
    // each connection's driver task is spawned IMMEDIATELY after the
    // connect and BEFORE any SQL: an unpolled `Connection` object
    // never reads the socket, so a query on an unspawned client would
    // hang until the case's outer deadline.
    let (mut holder, holder_connection) = tokio::time::timeout(
        PROBE_DEADLINE,
        tokio_postgres::connect(conn_str.as_str(), NoTls),
    )
    .await
    .expect("the raw holder connection connects under its own deadline")
    .expect("raw holder connection");
    let holder_driver = tokio::spawn(async move {
        // The driver ends when the client is dropped and the socket
        // closes - on the happy path below, and equally on panic
        // unwind, where the client's Drop runs the same way.
        let _ = holder_connection.await;
    });
    let (monitor, monitor_connection) = tokio::time::timeout(
        PROBE_DEADLINE,
        tokio_postgres::connect(conn_str.as_str(), NoTls),
    )
    .await
    .expect("the raw monitor connection connects under its own deadline")
    .expect("raw monitor connection");
    let monitor_driver = tokio::spawn(async move {
        let _ = monitor_connection.await;
    });

    // pg_locks stores the halves as oid (unsigned), hence u32 - the
    // same split the existing funnel proof uses.
    let classid = u32::try_from(WRITER_LOCK >> 32).expect("the lock key's high half");
    let objid = u32::try_from(WRITER_LOCK & 0xffff_ffff).expect("the lock key's low half");

    // The advisory-key halves, shared by both funnel probes.
    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 2] = [&classid, &objid];

    // Hold: the raw holder takes the funnel inside a transaction it
    // will only ever roll back. Every setup step is bounded so a
    // fixture hang diagnoses in seconds, not at the outer deadline.
    let held = tokio::time::timeout(PROBE_DEADLINE, holder.transaction())
        .await
        .expect("the holder transaction opens under its own deadline")
        .expect("holder transaction");
    tokio::time::timeout(
        PROBE_DEADLINE,
        held.execute("SELECT pg_advisory_xact_lock($1)", &[&WRITER_LOCK]),
    )
    .await
    .expect("the funnel acquisition runs under its own deadline")
    .expect("the raw holder takes the funnel");

    // Starve: select the pinned write against a bounded pg_locks
    // probe until the library backend is provably the funnel's
    // ungranted waiter, then STOP polling. The future is retained -
    // the helper holds only a borrow and never drops or cancels it -
    // so the caller can resume it for the outcome assertions after
    // the eviction. A completion under the held funnel is a
    // contract failure.
    enum Step {
        Waiter,
        Completed,
    }
    tokio::time::timeout(PHASE_DEADLINE, async {
        loop {
            let step = tokio::select! {
                _ = &mut *pinned => Step::Completed,
                probed = tokio::time::timeout(
                    PROBE_DEADLINE,
                    monitor.query(
                        "SELECT pid FROM pg_locks \
                         WHERE locktype = 'advisory' AND classid = $1 AND objid = $2 \
                           AND NOT granted",
                        &params,
                    ),
                ) => {
                    let rows: Vec<tokio_postgres::Row> = match probed {
                        Ok(Ok(rows)) => rows,
                        Ok(Err(error)) => panic!(
                            "the pg_locks waiter probe query failed: {error}"
                        ),
                        Err(_) => panic!(
                            "the waiter probe exceeded its own deadline"
                        ),
                    };
                    if rows.iter().any(|row| row.get::<_, i32>(0) == writer_pid) {
                        Step::Waiter
                    } else {
                        tokio::time::sleep(PROBE_INTERVAL).await;
                        continue;
                    }
                }
            };
            assert!(
                matches!(step, Step::Waiter),
                "the pinned write completed while the funnel was held"
            );
            break;
        }
    })
    .await
    .expect("the library backend becomes the funnel's ungranted waiter");

    // The holder rolls back (bounded), so the INDEPENDENT tokio-postgres
    // driver completes the library's queued lock query: the library
    // backend now holds the funnel, its transaction open with no
    // active statement. The holder's connection ends rolled back.
    tokio::time::timeout(PROBE_DEADLINE, held.rollback())
        .await
        .expect("the holder rollback runs under its own deadline")
        .expect("the holder releases the funnel");

    // Grant + idle: the exact pid owns a granted row for the key, and
    // pg_stat_activity reports it idle in transaction.
    tokio::time::timeout(PHASE_DEADLINE, async {
        loop {
            let observed = tokio::time::timeout(PROBE_DEADLINE, async {
                let granted = monitor
                    .query_one(
                        "SELECT COALESCE(array_agg(pid), '{}'::int[]) AS pids \
                         FROM pg_locks \
                         WHERE locktype = 'advisory' \
                           AND classid = $1 AND objid = $2 AND granted",
                        &[&classid, &objid],
                    )
                    .await
                    .expect("the grant probe query")
                    .get::<_, Vec<i32>>("pids")
                    .contains(&writer_pid);
                let idle = monitor
                    .query_opt(
                        "SELECT state FROM pg_stat_activity WHERE pid = $1",
                        &[&writer_pid],
                    )
                    .await
                    .expect("the activity probe query")
                    .map(|row| row.get::<_, String>("state"))
                    .as_deref()
                    == Some("idle in transaction");
                granted && idle
            })
            .await
            .expect("the grant probe runs under its own deadline");
            if observed {
                break;
            }
            tokio::time::sleep(PROBE_INTERVAL).await;
        }
    })
    .await
    .expect("the writer backend holds the granted funnel, idle in transaction");

    // Expire: under the configured bound plus slack, the evicted
    // backend AND its lock rows are gone - the exact-pid observation
    // that makes the configured expiry the causal proof.
    tokio::time::timeout(idle_bound + EXPIRY_SLACK, async {
        loop {
            let evicted = tokio::time::timeout(
                PROBE_DEADLINE,
                monitor.query_one(
                    "SELECT (SELECT count(*) FROM pg_stat_activity \
                                WHERE pid = $1) = 0 \
                         AND (SELECT count(*) FROM pg_locks \
                                WHERE pid = $1) = 0 AS evicted",
                    &[&writer_pid],
                ),
            )
            .await
            .expect("the eviction probe runs under its own deadline")
            .expect("the eviction probe query")
            .get::<_, bool>("evicted");
            if evicted {
                break;
            }
            tokio::time::sleep(PROBE_INTERVAL).await;
        }
    })
    .await
    .expect("the configured idle bound evicts the writer backend");

    // Cleanup: the holder already ended rolled back; dropping the raw
    // clients closes both sockets, which ends the spawned driver
    // tasks (a panic unwind drops them the same way, so the detached
    // drivers still land). The happy path awaits both preserved
    // JoinHandles under a bounded deadline instead of leaning on the
    // case's outer bound. The pinned future is the caller's borrow
    // and is left untouched for resumption.
    drop(holder);
    drop(monitor);
    tokio::time::timeout(PROBE_DEADLINE, async {
        let _ = holder_driver.await;
        let _ = monitor_driver.await;
    })
    .await
    .expect("the raw connection drivers close under their own deadline");
}

/// C4, append path. The pinned future is a CLONE of the configured
/// store, so the case doubles as the clone-carries-the-bound wiring
/// proof. Expected outcome: the eviction is
/// `AppendError::Backend(PgStreamsError::Connection(_))`, never
/// `AppendError::LockTimeout`, and the inner SQLSTATE is not `55P03`;
/// the termination's exact code is unpinned (see the module docs).
/// The stream holds nothing; a fresh append through the pool's
/// replacement connection succeeds and is the stream's only event.
#[tokio::test(flavor = "multi_thread")]
async fn an_idle_append_holder_is_evicted_and_the_append_fails_as_a_session_fault() {
    use epoch::streams::postgres::{PgEventStreams, PgStreamsError};
    use epoch::streams::{AppendError, EventBatch, EventStreams, ExpectedVersion, StreamState};

    under_deadline(async {
        let (pool, pid) = single_writer_pool().await;
        let store = PgEventStreams::<String, IdleWriter>::new(
            pool.clone(),
            &unique_category("idle-append"),
        )
        .with_idle_transaction_timeout(IDLE_BOUND);
        store.migrate().await.expect("schema migrates");

        let key = format!("idle-{}", rusty_ulid::generate_ulid_string());
        // The pinned future rides a CLONE: the case doubles as the
        // clone-carries-the-bound wiring proof. The batch is bound
        // because the pinned future borrows it.
        let writer = store.clone();
        let batch = EventBatch::new(vec![IdleWriter]).expect("one event is nonempty");
        let mut pinned = Box::pin(writer.append(ExpectedVersion::NoStream, &key, &batch));

        hold_then_starve_then_expire(pid, &mut pinned, IDLE_BOUND).await;

        let outcome = pinned.await;
        match outcome {
            Err(AppendError::Backend(PgStreamsError::Connection(error))) => {
                assert_ne!(
                    error.code(),
                    Some(&tokio_postgres::error::SqlState::LOCK_NOT_AVAILABLE),
                    "the eviction is never the retryable 55P03 lock-wait class"
                );
            }
            Err(AppendError::LockTimeout(reported)) => panic!(
                "the eviction must surface as a session fault, never the \
                 retryable funnel timeout: {reported:?}"
            ),
            other => panic!(
                "expected Err(AppendError::Backend(PgStreamsError::Connection(_))), \
                 got {other:?}"
            ),
        }

        assert_eq!(
            store.load_stream(&key).await.expect("load succeeds"),
            StreamState::Missing,
            "the evicted append stored nothing"
        );

        // bb8 discards the dead connection on checkout, so this fresh
        // append rides the pool's replacement backend.
        store
            .append(
                ExpectedVersion::NoStream,
                &key,
                &EventBatch::new(vec![IdleWriter]).expect("one event is nonempty"),
            )
            .await
            .expect("a fresh append succeeds through the replacement connection");
        assert_eq!(
            store.load_stream(&key).await.expect("load succeeds"),
            StreamState::Present(EventBatch::new(vec![IdleWriter]).expect("one event is nonempty")),
            "the stream holds exactly the fresh append's one event"
        );
    })
    .await;
}

/// C4, batch path: `PgDatabase::transact` under the same choreography
/// through the same helper. The batch writes one stream in each of two
/// categories, so "no partial batch" is observable: after the eviction
/// BOTH streams must be missing, the outcome must be
/// `TransactError::Backend(PgStreamsError::Connection(_))`, never
/// `TransactError::LockTimeout`, with an inner SQLSTATE that is not
/// `55P03`; the termination's exact code is unpinned (see the module
/// docs). A fresh two-write transact through the replacement
/// connection commits both.
#[tokio::test(flavor = "multi_thread")]
async fn an_idle_batch_holder_is_evicted_and_the_transact_fails_as_a_session_fault() {
    use epoch::streams::postgres::{PgDatabase, PgEventStreams, PgStreamsError};
    use epoch::streams::{
        AtomicStreams, EventBatch, EventStreams, ExpectedVersion, StreamState, TransactError,
    };

    under_deadline(async {
        let (pool, pid) = single_writer_pool().await;
        let category_a = unique_category("idle-batch-a");
        let category_b = unique_category("idle-batch-b");
        let key_a = format!("idle-{}", rusty_ulid::generate_ulid_string());
        let key_b = format!("idle-{}", rusty_ulid::generate_ulid_string());

        let reader_a = PgEventStreams::<String, IdleWriter>::new(pool.clone(), &category_a);
        let reader_b = PgEventStreams::<String, IdleWriter>::new(pool.clone(), &category_b);
        reader_a.migrate().await.expect("schema migrates");

        let db = PgDatabase::new(pool.clone()).with_idle_transaction_timeout(IDLE_BOUND);
        let batch = {
            let mut builder = db.batch();
            builder
                .write(
                    &category_a,
                    &key_a,
                    ExpectedVersion::NoStream,
                    &EventBatch::new(vec![IdleWriter]).expect("one event is nonempty"),
                )
                .expect("the first stream's write encodes");
            builder
                .write(
                    &category_b,
                    &key_b,
                    ExpectedVersion::NoStream,
                    &EventBatch::new(vec![IdleWriter]).expect("one event is nonempty"),
                )
                .expect("the second stream's write encodes");
            builder.build().expect("the batch has writes")
        };

        // The pinned future rides a CLONE of the configured handle:
        // the case doubles as the clone-carries-the-bound wiring
        // proof.
        let writer = db.clone();
        let mut pinned = Box::pin(writer.transact(batch));

        hold_then_starve_then_expire(pid, &mut pinned, IDLE_BOUND).await;

        let outcome = pinned.await;
        match outcome {
            Err(TransactError::Backend(PgStreamsError::Connection(error))) => {
                assert_ne!(
                    error.code(),
                    Some(&tokio_postgres::error::SqlState::LOCK_NOT_AVAILABLE),
                    "the eviction is never the retryable 55P03 lock-wait class"
                );
            }
            Err(TransactError::LockTimeout(reported)) => panic!(
                "the eviction must surface as a session fault, never the \
                 retryable funnel timeout: {reported:?}"
            ),
            other => panic!(
                "expected Err(TransactError::Backend(PgStreamsError::Connection(_))), \
                 got {other:?}"
            ),
        }

        assert_eq!(
            reader_a.load_stream(&key_a).await.expect("load succeeds"),
            StreamState::Missing,
            "no partial batch: the first stream holds nothing"
        );
        assert_eq!(
            reader_b.load_stream(&key_b).await.expect("load succeeds"),
            StreamState::Missing,
            "no partial batch: the second stream holds nothing"
        );

        // bb8 discards the dead connection on checkout, so this fresh
        // transact rides the pool's replacement backend.
        let fresh = {
            let mut builder = db.batch();
            builder
                .write(
                    &category_a,
                    &key_a,
                    ExpectedVersion::NoStream,
                    &EventBatch::new(vec![IdleWriter]).expect("one event is nonempty"),
                )
                .expect("the first stream's write encodes");
            builder
                .write(
                    &category_b,
                    &key_b,
                    ExpectedVersion::NoStream,
                    &EventBatch::new(vec![IdleWriter]).expect("one event is nonempty"),
                )
                .expect("the second stream's write encodes");
            builder.build().expect("the batch has writes")
        };
        db.transact(fresh)
            .await
            .expect("a fresh transact commits through the replacement connection");
        assert_eq!(
            reader_a.load_stream(&key_a).await.expect("load succeeds"),
            StreamState::Present(EventBatch::new(vec![IdleWriter]).expect("one event is nonempty")),
            "the first stream holds exactly its one event"
        );
        assert_eq!(
            reader_b.load_stream(&key_b).await.expect("load succeeds"),
            StreamState::Present(EventBatch::new(vec![IdleWriter]).expect("one event is nonempty")),
            "the second stream holds exactly its one event"
        );
    })
    .await;
}
