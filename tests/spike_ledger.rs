//! The E19 gate-S spike: ADR 0010's validation gate, executed before
//! the ledger is built. Both paths run as raw SQL over dedicated
//! connections so the only delta between them is the ledger protocol
//! itself - the unledgered path mirrors the shipped append/transact
//! statement shapes, the ledgered path adds the allocation
//! transaction (advisory-locked `nextval` draws plus the ledger-row
//! insert), the first-statement claim, explicit sequence values, and
//! the guarded COMMITTED update.
//!
//! Pre-registered verdict (ADR 0010, applied mechanically):
//!
//! - FAIL if p99 single-append latency overhead exceeds 2x the
//!   unledgered path, at 8 concurrent writers over 10k appends;
//! - FAIL if batch lock-wait p99 regresses more than 50%, under 4
//!   concurrent multi-stream batches contending for a shared stream
//!   pool with IN_FLIGHT ledger rows outstanding.
//!
//! Either threshold broken pivots the wave to the single-writer
//! fallback. Run explicitly against the compose postgres:
//!
//! ```sh
//! cargo test --features postgres --test spike_ledger -- --ignored --nocapture
//! ```
#![cfg(feature = "postgres")]

use std::time::Duration;
use std::time::Instant;

use tokio_postgres::Client;
use tokio_postgres::NoTls;

/// Serializes allocations, distinct from the migration lock and from
/// the two-int stream locks; spells "e19_ledg".
const LEDGER_LOCK: i64 = 0x6531_395f_6c65_6467;

const APPEND_WORKERS: usize = 8;
const APPEND_ITERATIONS: usize = 10_000;
const APPEND_WARMUP: usize = 100;

const BATCH_WORKERS: usize = 4;
const BATCH_STREAM_POOL: usize = 8;
const BATCH_ITERATIONS_PER_WORKER: usize = 250;
const BATCH_WARMUP: usize = 20;
const BATCH_STREAMS_PER_BATCH: usize = 3;
const BATCH_EVENTS_PER_STREAM: usize = 2;

/// A parsed connection string; there is no safe default database, so
/// an unset variable must stop the run rather than pick one.
fn conn_str() -> String {
    let _ = dotenv::dotenv();
    std::env::var("EPOCH_PG_TEST_URL").expect("EPOCH_PG_TEST_URL must be set (see .env.example)")
}

/// One dedicated connection per worker: pool acquisition noise is
/// identical across paths anyway, and a held connection is the
/// steady-state shape of a real writer.
async fn connect() -> Client {
    let config: tokio_postgres::Config = conn_str().parse().expect("parse EPOCH_PG_TEST_URL");
    let (client, driver) = config.connect(NoTls).await.expect("connect");
    tokio::spawn(async move {
        let _ = driver.await;
    });
    client
}

/// The spike's throwaway ledger. Not the shipped schema - the real
/// table arrives as a migration step only if this spike passes.
async fn setup(client: &Client) {
    client
        .execute(
            "CREATE TABLE IF NOT EXISTS spike_e19_ledger ( \
                 first_seq BIGINT PRIMARY KEY, \
                 last_seq BIGINT NOT NULL, \
                 txid BIGINT, \
                 state TEXT NOT NULL \
             )",
            &[],
        )
        .await
        .expect("create spike ledger");
}

fn percentile(samples: &mut [Duration], fraction: f64) -> Duration {
    samples.sort_unstable();
    let index = ((samples.len() as f64) * fraction).ceil() as usize - 1;
    samples[index.min(samples.len() - 1)]
}

fn report(label: &str, mut samples: Vec<Duration>) {
    let mean = samples.iter().sum::<Duration>() / samples.len() as u32;
    println!(
        "{label}: n={} p50={} p90={} p99={} p999={} max={} mean={}",
        samples.len(),
        percentile(&mut samples, 0.50).as_micros(),
        percentile(&mut samples, 0.90).as_micros(),
        percentile(&mut samples, 0.99).as_micros(),
        percentile(&mut samples, 0.999).as_micros(),
        samples.iter().max().expect("nonempty").as_micros(),
        mean.as_micros(),
    );
}

/// The one-number comparison the verdict reads.
fn p99(samples: &[Duration]) -> f64 {
    let mut sorted = samples.to_vec();
    percentile(&mut sorted, 0.99).as_micros() as f64
}

/// One unledgered append: the shipped statement shapes - stream
/// advisory lock, head read, insert with the column default.
async fn append_unledgered(client: &mut Client, category: &str, key: &str) -> Duration {
    let start = Instant::now();
    let tx = client.transaction().await.expect("begin");
    tx.execute(
        "SELECT pg_advisory_xact_lock(hashtext($1), hashtext($2))",
        &[&category, &key],
    )
    .await
    .expect("stream lock");
    let head: i64 = tx
        .query_one(
            "SELECT COALESCE(MAX(sequence), 0) AS head FROM stream_events \
             WHERE category = $1 AND stream_key = $2",
            &[&category, &key],
        )
        .await
        .expect("head read")
        .get("head");
    tx.execute(
        "INSERT INTO stream_events \
         (category, stream_key, event_type, sequence, event_data, event_metadata) \
         VALUES ($1, $2, 'Spike', $3, '\"{}\"'::jsonb, '{}'::jsonb)",
        &[&category, &key, &(head + 1)],
    )
    .await
    .expect("insert");
    tx.commit().await.expect("commit");
    start.elapsed()
}

/// One ledgered append: the allocation transaction (serialized draw
/// from the shared sequence, ledger row born IN_FLIGHT), then the
/// event transaction (first-statement claim, explicit sequence
/// value, guarded COMMITTED).
async fn append_ledgered(client: &mut Client, category: &str, key: &str) -> Duration {
    let start = Instant::now();

    let allocation = client.transaction().await.expect("begin allocation");
    allocation
        .execute("SELECT pg_advisory_xact_lock($1)", &[&LEDGER_LOCK])
        .await
        .expect("allocation lock");
    let first: i64 = allocation
        .query_one(
            "SELECT nextval('stream_events_global_sequence_seq') AS drawn",
            &[],
        )
        .await
        .expect("draw")
        .get("drawn");
    allocation
        .execute(
            "INSERT INTO spike_e19_ledger (first_seq, last_seq, txid, state) \
             VALUES ($1, $1, NULL, 'IN_FLIGHT')",
            &[&first],
        )
        .await
        .expect("ledger row");
    allocation.commit().await.expect("commit allocation");

    let events = client.transaction().await.expect("begin events");
    let claimed = events
        .execute(
            "UPDATE spike_e19_ledger SET txid = txid_current() \
             WHERE first_seq = $1 AND state = 'IN_FLIGHT'",
            &[&first],
        )
        .await
        .expect("claim");
    assert_eq!(claimed, 1, "a fresh row is claimable");
    events
        .execute(
            "SELECT pg_advisory_xact_lock(hashtext($1), hashtext($2))",
            &[&category, &key],
        )
        .await
        .expect("stream lock");
    let head: i64 = events
        .query_one(
            "SELECT COALESCE(MAX(sequence), 0) AS head FROM stream_events \
             WHERE category = $1 AND stream_key = $2",
            &[&category, &key],
        )
        .await
        .expect("head read")
        .get("head");
    events
        .execute(
            "INSERT INTO stream_events \
             (category, stream_key, event_type, sequence, event_data, event_metadata, \
              global_sequence) \
             VALUES ($1, $2, 'Spike', $3, '\"{}\"'::jsonb, '{}'::jsonb, $4)",
            &[&category, &key, &(head + 1), &first],
        )
        .await
        .expect("insert");
    let committed = events
        .execute(
            "UPDATE spike_e19_ledger SET state = 'COMMITTED' \
             WHERE first_seq = $1 AND state = 'IN_FLIGHT'",
            &[&first],
        )
        .await
        .expect("guarded commit mark");
    assert_eq!(committed, 1, "the claimed row commits");
    events.commit().await.expect("commit events");

    start.elapsed()
}

/// One unledgered multi-stream batch: locks in client-sorted order,
/// heads, then two inserts per stream - the shipped transact shapes.
async fn batch_unledgered(client: &mut Client, category: &str, streams: &[String]) -> Duration {
    let start = Instant::now();
    let tx = client.transaction().await.expect("begin");
    let mut ordered = streams.to_vec();
    ordered.sort();
    for key in &ordered {
        tx.execute(
            "SELECT pg_advisory_xact_lock(hashtext($1), hashtext($2))",
            &[&category, &key],
        )
        .await
        .expect("stream lock");
    }
    for key in &ordered {
        let head: i64 = tx
            .query_one(
                "SELECT COALESCE(MAX(sequence), 0) AS head FROM stream_events \
                 WHERE category = $1 AND stream_key = $2",
                &[&category, &key],
            )
            .await
            .expect("head read")
            .get("head");
        for offset in 1..=BATCH_EVENTS_PER_STREAM as i64 {
            tx.execute(
                "INSERT INTO stream_events \
                 (category, stream_key, event_type, sequence, event_data, event_metadata) \
                 VALUES ($1, $2, 'Spike', $3, '\"{}\"'::jsonb, '{}'::jsonb)",
                &[&category, &key, &(head + offset)],
            )
            .await
            .expect("insert");
        }
    }
    tx.commit().await.expect("commit");
    start.elapsed()
}

/// One ledgered multi-stream batch: one allocation round-trip for the
/// whole batch (n draws inside the lock, one row spanning the range),
/// then the batch event transaction claims, takes the stream locks,
/// and writes explicit sequence values across its streams.
async fn batch_ledgered(client: &mut Client, category: &str, streams: &[String]) -> Duration {
    let n = (streams.len() * BATCH_EVENTS_PER_STREAM) as i64;
    let start = Instant::now();

    let allocation = client.transaction().await.expect("begin allocation");
    allocation
        .execute("SELECT pg_advisory_xact_lock($1)", &[&LEDGER_LOCK])
        .await
        .expect("allocation lock");
    let rows = allocation
        .query(
            "SELECT nextval('stream_events_global_sequence_seq') AS drawn \
             FROM generate_series(1, $1::bigint)",
            &[&n],
        )
        .await
        .expect("draw range");
    let first: i64 = rows.first().expect("n draws").get("drawn");
    let last: i64 = rows.last().expect("n draws").get("drawn");
    allocation
        .execute(
            "INSERT INTO spike_e19_ledger (first_seq, last_seq, txid, state) \
             VALUES ($1, $2, NULL, 'IN_FLIGHT')",
            &[&first, &last],
        )
        .await
        .expect("ledger row");
    allocation.commit().await.expect("commit allocation");

    let events = client.transaction().await.expect("begin events");
    let claimed = events
        .execute(
            "UPDATE spike_e19_ledger SET txid = txid_current() \
             WHERE first_seq = $1 AND state = 'IN_FLIGHT'",
            &[&first],
        )
        .await
        .expect("claim");
    assert_eq!(claimed, 1, "a fresh row is claimable");
    let mut ordered = streams.to_vec();
    ordered.sort();
    for key in &ordered {
        events
            .execute(
                "SELECT pg_advisory_xact_lock(hashtext($1), hashtext($2))",
                &[&category, &key],
            )
            .await
            .expect("stream lock");
    }
    let mut drawn = first;
    for key in &ordered {
        let head: i64 = events
            .query_one(
                "SELECT COALESCE(MAX(sequence), 0) AS head FROM stream_events \
                 WHERE category = $1 AND stream_key = $2",
                &[&category, &key],
            )
            .await
            .expect("head read")
            .get("head");
        for offset in 1..=BATCH_EVENTS_PER_STREAM as i64 {
            events
                .execute(
                    "INSERT INTO stream_events \
                     (category, stream_key, event_type, sequence, event_data, \
                      event_metadata, global_sequence) \
                     VALUES ($1, $2, 'Spike', $3, '\"{}\"'::jsonb, '{}'::jsonb, $4)",
                    &[&category, &key, &(head + offset as i64), &drawn],
                )
                .await
                .expect("insert");
            drawn += 1;
        }
    }
    let committed = events
        .execute(
            "UPDATE spike_e19_ledger SET state = 'COMMITTED' \
             WHERE first_seq = $1 AND state = 'IN_FLIGHT'",
            &[&first],
        )
        .await
        .expect("guarded commit mark");
    assert_eq!(committed, 1, "the claimed row commits");
    events.commit().await.expect("commit events");

    start.elapsed()
}

/// Run `iterations` per worker across `workers` dedicated connections,
/// each worker on its own stream set, and pool every sample.
async fn bench_append(
    category: &str,
    ledgered: bool,
    workers: usize,
    iterations: usize,
    warmup: usize,
) -> Vec<Duration> {
    let mut handles = Vec::new();
    for worker in 0..workers {
        let mut client = connect().await;
        let category = category.to_owned();
        handles.push(tokio::spawn(async move {
            let key = format!("w{worker}-{}", rusty_ulid::generate_ulid_string());
            for _ in 0..warmup {
                if ledgered {
                    append_ledgered(&mut client, &category, &key).await;
                } else {
                    append_unledgered(&mut client, &category, &key).await;
                }
            }
            let mut samples = Vec::with_capacity(iterations);
            for _ in 0..iterations {
                let sample = if ledgered {
                    append_ledgered(&mut client, &category, &key).await
                } else {
                    append_unledgered(&mut client, &category, &key).await
                };
                samples.push(sample);
            }
            samples
        }));
    }
    let mut pooled = Vec::with_capacity(workers * iterations);
    for handle in handles {
        pooled.extend(handle.await.expect("worker must not panic"));
    }
    pooled
}

/// Four concurrent batch writers contending for one shared stream
/// pool, so advisory-lock waits (and rival IN_FLIGHT rows, on the
/// ledgered path) are the steady state.
async fn bench_batches(
    category: &str,
    ledgered: bool,
    iterations_per_worker: usize,
    warmup: usize,
) -> Vec<Duration> {
    let stream_pool: Vec<String> = (0..BATCH_STREAM_POOL)
        .map(|i| format!("pool-{i}"))
        .collect();
    let mut handles = Vec::new();
    for worker in 0..BATCH_WORKERS {
        let mut client = connect().await;
        let category = category.to_owned();
        let stream_pool = stream_pool.clone();
        handles.push(tokio::spawn(async move {
            let mut samples = Vec::with_capacity(iterations_per_worker);
            for iteration in 0..iterations_per_worker + warmup {
                let start = (worker * 7 + iteration * BATCH_STREAMS_PER_BATCH) % BATCH_STREAM_POOL;
                let streams: Vec<String> = (0..BATCH_STREAMS_PER_BATCH)
                    .map(|offset| stream_pool[(start + offset) % BATCH_STREAM_POOL].clone())
                    .collect();
                let sample = if ledgered {
                    batch_ledgered(&mut client, &category, &streams).await
                } else {
                    batch_unledgered(&mut client, &category, &streams).await
                };
                if iteration >= warmup {
                    samples.push(sample);
                }
            }
            samples
        }));
    }
    let mut pooled = Vec::with_capacity(BATCH_WORKERS * iterations_per_worker);
    for handle in handles {
        pooled.extend(handle.await.expect("worker must not panic"));
    }
    pooled
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "the E19 gate-S spike: minutes of live-postgres load"]
async fn the_allocation_ledger_spike() {
    let setup_client = connect().await;
    setup(&setup_client).await;
    drop(setup_client);

    // Migrate through the shipped mechanism so the schema is exactly
    // what production code sees (metadata column included).
    let pool = epoch::streams::postgres::pool_from_conn_str(&conn_str())
        .await
        .expect("pool");
    epoch::streams::postgres::PgEventStreams::<String, ()>::new(pool, "spike-e19")
        .migrate()
        .await
        .expect("migrate");

    println!("== single-append path: {APPEND_WORKERS} writers x {APPEND_ITERATIONS} appends ==");
    let bare = bench_append(
        "spike-append-bare",
        false,
        APPEND_WORKERS,
        APPEND_ITERATIONS,
        APPEND_WARMUP,
    )
    .await;
    let ledgered = bench_append(
        "spike-append-ledger",
        true,
        APPEND_WORKERS,
        APPEND_ITERATIONS,
        APPEND_WARMUP,
    )
    .await;
    report("unledgered", bare.clone());
    report("ledgered  ", ledgered.clone());
    let append_ratio = p99(&ledgered) / p99(&bare);
    println!("append p99 ratio: {append_ratio:.3} (FAIL above 2.000)");

    println!(
        "== batch path: {BATCH_WORKERS} concurrent multi-stream batches over a \
         {BATCH_STREAM_POOL}-stream pool =="
    );
    let bare_batches = bench_batches(
        "spike-batch-bare",
        false,
        BATCH_ITERATIONS_PER_WORKER,
        BATCH_WARMUP,
    )
    .await;
    let ledgered_batches = bench_batches(
        "spike-batch-ledger",
        true,
        BATCH_ITERATIONS_PER_WORKER,
        BATCH_WARMUP,
    )
    .await;
    report("unledgered", bare_batches.clone());
    report("ledgered  ", ledgered_batches.clone());
    let batch_ratio = p99(&ledgered_batches) / p99(&bare_batches);
    println!("batch p99 ratio: {batch_ratio:.3} (FAIL above 1.500)");

    let append_fail = append_ratio > 2.0;
    let batch_fail = batch_ratio > 1.5;
    let verdict = if append_fail || batch_fail {
        "FAIL"
    } else {
        "PASS"
    };
    println!(
        "SPIKE VERDICT: {verdict} \
         (append {append_ratio:.3} vs 2.000, batch {batch_ratio:.3} vs 1.500)"
    );
    if append_fail || batch_fail {
        println!(
            "Pre-registered pivot executes: single-writer fallback (ADR 0010 option 3); \
             consequences as named in the record."
        );
    }
}
