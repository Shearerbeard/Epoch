//! C15 funnel benchmark: the real public write and feed APIs measured on a
//! dedicated postgres database. Feature-gated and `#[ignore]`d; owner-only.
//!
//! `EPOCH_BENCH_ONLY_ROUND` (optional) selects one declared round for A/B
//! alternating baseline/repair invocations; a filtered run is a protocol
//! selector, not a completed study on its own.
#![cfg(feature = "postgres")]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use epoch::decider::Event;
use epoch::streams::feed::{ConsumerGroup, EventFeed, PollLimit};
use epoch::streams::postgres::{PgDatabase, PgEventFeed, PgEventStreams, PgPool};
use epoch::streams::{AtomicStreams, EventBatch, EventStreams, ExpectedVersion};
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

const POOL_SIZE: usize = 32;
const WARMUP_OPS: usize = 100;
const PAYLOAD_BYTES: usize = 1024;
const BUDGET: Duration = Duration::from_secs(20 * 60);
const APPEND_CONCURRENCIES: [usize; 4] = [1, 4, 8, 16];
const BATCH_CALLERS: usize = 4;
const BATCH_STREAMS: usize = 3;
const BATCH_EVENTS: usize = 2;
const MIXED_CALLERS: usize = 8;
const MIXED_POLL_LIMIT: usize = 256;
const FEED_PAGE: usize = 32;
const FEED_BACKLOG: usize = 32;
const DRAIN_WAIT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct BenchEvent {
    payload: String,
}

impl BenchEvent {
    fn one() -> Self {
        Self {
            payload: "x".repeat(PAYLOAD_BYTES),
        }
    }
}

impl Event for BenchEvent {
    type EntityId = ();

    fn event_type(&self) -> String {
        "BenchEvent".to_owned()
    }

    fn get_id(&self) -> Self::EntityId {}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Writers,
    Feed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Profile {
    Measurement,
    Smoke,
}

struct Config {
    mode: Mode,
    profile: Profile,
    ops: usize,
    rounds: usize,
    only_round: Option<usize>,
    database: String,
    connection: tokio_postgres::Config,
}

impl Config {
    fn from_env() -> Result<Self, String> {
        let url = std::env::var("EPOCH_PG_BENCH_URL")
            .map_err(|_| "EPOCH_PG_BENCH_URL must be set".to_owned())?;
        let connection: tokio_postgres::Config = url
            .parse()
            .map_err(|error: tokio_postgres::Error| format!("EPOCH_PG_BENCH_URL: {error}"))?;
        let database = connection
            .get_dbname()
            .ok_or("EPOCH_PG_BENCH_URL names no database")?
            .to_owned();
        if !database.starts_with("epoch_e19_bench_") {
            return Err(format!(
                "refusing database {database:?}: the bench requires an epoch_e19_bench_* database"
            ));
        }
        let mode = match std::env::var("EPOCH_BENCH_MODE").as_deref() {
            Ok("writers") => Mode::Writers,
            Ok("feed") => Mode::Feed,
            _ => return Err("EPOCH_BENCH_MODE must be set to writers or feed".to_owned()),
        };
        let ops = match std::env::var("EPOCH_BENCH_OPS") {
            Ok(raw) => {
                let ops: usize = raw
                    .parse()
                    .map_err(|_| format!("EPOCH_BENCH_OPS {raw:?} is not a count"))?;
                if ops == 0 {
                    return Err("EPOCH_BENCH_OPS must be positive".to_owned());
                }
                ops
            }
            Err(_) => 10000,
        };
        let rounds = match std::env::var("EPOCH_BENCH_ROUNDS") {
            Ok(raw) => {
                let rounds: usize = raw
                    .parse()
                    .map_err(|_| format!("EPOCH_BENCH_ROUNDS {raw:?} is not a count"))?;
                if rounds == 0 {
                    return Err("EPOCH_BENCH_ROUNDS must be positive".to_owned());
                }
                rounds
            }
            Err(_) => 3,
        };
        let only_round = match std::env::var("EPOCH_BENCH_ONLY_ROUND") {
            Ok(raw) => {
                let selected: usize = raw
                    .parse()
                    .map_err(|_| format!("EPOCH_BENCH_ONLY_ROUND {raw:?} is not a round"))?;
                if selected == 0 || selected > rounds {
                    return Err(format!(
                        "EPOCH_BENCH_ONLY_ROUND {selected} outside 1..={rounds}"
                    ));
                }
                Some(selected)
            }
            Err(_) => None,
        };
        let profile = if ops >= 10000 && rounds >= 3 {
            Profile::Measurement
        } else {
            Profile::Smoke
        };
        Ok(Self {
            mode,
            profile,
            ops,
            rounds,
            only_round,
            database,
            connection,
        })
    }
}

#[derive(Default)]
struct Stats {
    expected: usize,
    completed: usize,
    successes: usize,
    events: u64,
    samples_us: Vec<u64>,
    errors: BTreeMap<String, u64>,
    by_kind: BTreeMap<&'static str, Vec<u64>>,
}

impl Stats {
    fn record(&mut self, attempt: Result<(u64, u64), String>) {
        self.completed += 1;
        match attempt {
            Ok((latency_us, events)) => {
                self.successes += 1;
                self.events += events;
                self.samples_us.push(latency_us);
            }
            Err(error) => {
                *self.errors.entry(format!("{error:?}")).or_default() += 1;
            }
        }
    }

    fn percentiles(samples: &[u64]) -> Option<(u64, u64, u64, u64)> {
        let mut sorted = samples.to_vec();
        sorted.sort_unstable();
        let n = sorted.len();
        if n == 0 {
            return None;
        }
        let rank = |percent: u64| (percent * n as u64).div_ceil(100).clamp(1, n as u64) as usize;
        Some((
            sorted[rank(50) - 1],
            sorted[rank(95) - 1],
            sorted[rank(99) - 1],
            sorted[n - 1],
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Condition {
    Append(usize),
    Batch,
    Mixed,
    FeedEmpty,
    FeedReplay,
    FeedAckCycles,
}

impl Condition {
    fn name(self) -> &'static str {
        match self {
            Self::Append(1) => "append_c1",
            Self::Append(4) => "append_c4",
            Self::Append(8) => "append_c8",
            Self::Append(16) => "append_c16",
            Self::Append(_) => "append",
            Self::Batch => "batch_c4",
            Self::Mixed => "mixed_c8",
            Self::FeedEmpty => "feed_poll_empty",
            Self::FeedReplay => "feed_poll_replay",
            Self::FeedAckCycles => "feed_poll_ack_cycles",
        }
    }

    fn all(mode: Mode) -> Vec<Self> {
        match mode {
            Mode::Writers => APPEND_CONCURRENCIES
                .iter()
                .copied()
                .map(Self::Append)
                .chain([Self::Batch, Self::Mixed])
                .collect(),
            Mode::Feed => vec![Self::FeedEmpty, Self::FeedReplay, Self::FeedAckCycles],
        }
    }
}

async fn bench_pool(connection: &tokio_postgres::Config) -> PgPool {
    let manager =
        bb8_postgres::PostgresConnectionManager::new(connection.clone(), tokio_postgres::NoTls);
    let pool = bb8::Pool::builder()
        .max_size(POOL_SIZE as u32)
        .min_idle(Some(POOL_SIZE as u32))
        .build(manager)
        .await
        .expect("bench pool builds");
    // Warm: hold every lease concurrently before dropping them.
    let leases: Vec<_> = join_all((0..POOL_SIZE).map(|_| pool.get())).await;
    for lease in &leases {
        lease.as_ref().expect("warm lease acquired");
    }
    drop(leases);
    pool
}

async fn pg_environment(pool: &PgPool) -> serde_json::Value {
    let row = pool
        .get()
        .await
        .expect("pg probe connection")
        .query_one(
            "SELECT version() AS version, current_setting('fsync') AS fsync, \
             current_setting('synchronous_commit') AS synchronous_commit",
            &[],
        )
        .await
        .expect("pg environment probe");
    serde_json::json!({
        "version": row.get::<_, String>("version"),
        "fsync": row.get::<_, String>("fsync"),
        "synchronous_commit": row.get::<_, String>("synchronous_commit"),
    })
}

async fn append_attempt(
    store: &PgEventStreams<String, BenchEvent>,
    key: &str,
) -> Result<(u64, u64), String> {
    let key = key.to_owned();
    let events = EventBatch::new(vec![BenchEvent::one()]).expect("one event is nonempty");
    let started = Instant::now();
    store
        .append(ExpectedVersion::NoStream, &key, &events)
        .await
        .map_err(|error| format!("{error:?}"))?;
    Ok((started.elapsed().as_micros() as u64, 1))
}

async fn batch_attempt(
    db: &PgDatabase,
    category: &str,
    keys: &[String],
) -> Result<(u64, u64), String> {
    assert_eq!(keys.len(), BATCH_STREAMS, "batch keys match BATCH_STREAMS");
    let started = Instant::now();
    let mut builder = db.batch();
    let events = EventBatch::new((0..BATCH_EVENTS).map(|_| BenchEvent::one()).collect())
        .expect("batch events are nonempty");
    for key in keys {
        builder
            .write(category, key, ExpectedVersion::NoStream, &events)
            .map_err(|error| format!("{error:?}"))?;
    }
    let batch = builder.build().expect("the batch has writes");
    db.transact(batch)
        .await
        .map_err(|error| format!("{error:?}"))?;
    Ok((
        started.elapsed().as_micros() as u64,
        (BATCH_STREAMS * BATCH_EVENTS) as u64,
    ))
}

async fn feed_attempt(
    feed: &PgEventFeed<BenchEvent>,
    group: &ConsumerGroup,
    ack: bool,
) -> Result<(u64, u64), String> {
    // Ack cycles advance one event per poll; replay and empty polls page 32.
    let limit = if ack { 1 } else { FEED_PAGE };
    let started = Instant::now();
    let page = feed
        .poll(group, PollLimit::new(limit).expect("nonzero poll limit"))
        .await
        .map_err(|error| format!("{error:?}"))?;
    let mut events = page.len() as u64;
    if ack {
        let tip = page
            .last()
            .ok_or("ack cycle polled an empty page")?
            .position();
        feed.ack(group, tip)
            .await
            .map_err(|error| format!("{error:?}"))?;
        events = 1;
    }
    Ok((started.elapsed().as_micros() as u64, events))
}

async fn run_write_case(
    pool: &PgPool,
    config: &Config,
    condition: Condition,
    category: &str,
) -> (Stats, Duration, Option<(u64, u64)>) {
    let store = PgEventStreams::<String, BenchEvent>::new(pool.clone(), category);
    let db = PgDatabase::new(pool.clone());
    store.migrate().await.expect("untimed migration");

    for i in 0..WARMUP_OPS {
        match condition {
            Condition::Append(_) => {
                append_attempt(&store, &format!("{category}/warm-{i}"))
                    .await
                    .expect("warmup append");
            }
            Condition::Batch => {
                let keys = (0..BATCH_STREAMS)
                    .map(|s| format!("{category}/warm-{i}-{s}"))
                    .collect::<Vec<_>>();
                batch_attempt(&db, category, &keys)
                    .await
                    .expect("warmup batch");
            }
            _ => unreachable!("write case dispatched {condition:?}"),
        }
    }

    let ops = config.ops;
    let workers = match condition {
        Condition::Append(concurrency) => concurrency,
        Condition::Batch => BATCH_CALLERS,
        _ => unreachable!("write case dispatched {condition:?}"),
    };

    let started = Instant::now();
    let mut job_set: JoinSet<Vec<Result<(u64, u64), String>>> = JoinSet::new();
    for worker in 0..workers {
        let store = store.clone();
        let db = db.clone();
        let category = category.to_owned();
        job_set.spawn(async move {
            let mut attempts = Vec::new();
            for i in (worker..ops).step_by(workers) {
                let attempt = match condition {
                    Condition::Append(_) => {
                        append_attempt(&store, &format!("{category}/op-{i}")).await
                    }
                    Condition::Batch => {
                        let keys = (0..BATCH_STREAMS)
                            .map(|s| format!("{category}/op-{i}-{s}"))
                            .collect::<Vec<_>>();
                        batch_attempt(&db, &category, &keys).await
                    }
                    _ => unreachable!("write case dispatched {condition:?}"),
                };
                attempts.push(attempt);
            }
            attempts
        });
    }

    let mut stats = Stats {
        expected: ops,
        ..Stats::default()
    };
    while let Some(worker) = job_set.join_next().await {
        for attempt in worker.expect("write worker task") {
            stats.record(attempt);
        }
    }
    (stats, started.elapsed(), None)
}

type TaggedAttempt = (&'static str, Result<(u64, u64), String>);

async fn run_mixed_case(
    pool: &PgPool,
    config: &Config,
    category: &str,
) -> (Stats, Duration, Option<(u64, u64)>) {
    let store = PgEventStreams::<String, BenchEvent>::new(pool.clone(), category);
    let db = PgDatabase::new(pool.clone());
    let feed = PgEventFeed::<BenchEvent>::new(pool.clone(), category);
    let group = ConsumerGroup::new("bench-consumers").expect("non-empty group");
    let page_limit = PollLimit::new(MIXED_POLL_LIMIT).expect("nonzero poll limit");
    store.migrate().await.expect("untimed migration");

    // 100 warm attempts, untimed; every warmup event is then drained and
    // acked before the timer so the timed reader never re-counts them.
    let mut warm_events = 0u64;
    for i in 0..WARMUP_OPS {
        let attempt = if i % 2 == 0 {
            append_attempt(&store, &format!("{category}/warm-{i}")).await
        } else {
            let keys = (0..BATCH_STREAMS)
                .map(|s| format!("{category}/warm-{i}-{s}"))
                .collect::<Vec<_>>();
            batch_attempt(&db, category, &keys).await
        };
        warm_events += attempt.expect("warmup mixed attempt").1;
    }
    tokio::time::timeout(DRAIN_WAIT, async {
        let mut drained = 0u64;
        while drained < warm_events {
            let page = feed
                .poll(&group, page_limit)
                .await
                .expect("warmup drain poll");
            if page.is_empty() {
                panic!("warmup drain stalled at {drained} of {warm_events}");
            }
            feed.ack(&group, page.last().expect("nonempty page").position())
                .await
                .expect("warmup drain ack");
            drained += page.len() as u64;
        }
    })
    .await
    .expect("warmup drain exceeded its bound");

    let started = Instant::now();
    let (target_tx, target_rx) = tokio::sync::watch::channel(None::<u64>);
    let mut readers: JoinSet<Result<u64, String>> = JoinSet::new();
    readers.spawn({
        let feed = feed.clone();
        let group = group.clone();
        async move {
            let mut delivered = 0u64;
            loop {
                let target = *target_rx.borrow();
                if let Some(known) = target {
                    if delivered > known {
                        return Err(format!("reader overshot target {known} at {delivered}"));
                    }
                    if delivered == known {
                        return Ok(delivered);
                    }
                }
                let page = tokio::time::timeout(DRAIN_WAIT, feed.poll(&group, page_limit))
                    .await
                    .expect("reader poll exceeded its bound")
                    .map_err(|error| format!("{error:?}"))?;
                if page.is_empty() {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    continue;
                }
                tokio::time::timeout(
                    DRAIN_WAIT,
                    feed.ack(&group, page.last().expect("nonempty page").position()),
                )
                .await
                .expect("reader ack exceeded its bound")
                .map_err(|error| format!("{error:?}"))?;
                delivered += page.len() as u64;
            }
        }
    });

    let ops = config.ops;
    let mut writers: JoinSet<Vec<TaggedAttempt>> = JoinSet::new();
    for caller in 0..MIXED_CALLERS {
        let store = store.clone();
        let db = db.clone();
        let category = category.to_owned();
        writers.spawn(async move {
            let mut attempts = Vec::new();
            for (local, i) in (caller..ops).step_by(MIXED_CALLERS).enumerate() {
                let attempt = if local % 2 == 0 {
                    (
                        "append",
                        append_attempt(&store, &format!("{category}/op-{i}")).await,
                    )
                } else {
                    let keys = (0..BATCH_STREAMS)
                        .map(|s| format!("{category}/op-{i}-{s}"))
                        .collect::<Vec<_>>();
                    ("batch", batch_attempt(&db, &category, &keys).await)
                };
                attempts.push(attempt);
            }
            attempts
        });
    }

    let mut stats = Stats {
        expected: ops,
        ..Stats::default()
    };
    while let Some(caller) = writers.join_next().await {
        for (kind, attempt) in caller.expect("mixed writer task") {
            if let Ok((latency, _)) = &attempt {
                stats.by_kind.entry(kind).or_default().push(*latency);
            }
            stats.record(attempt);
        }
    }

    // Writer wall is captured before the drain so subscriber drain does
    // not deflate writer throughput; drain lag is reported separately.
    let writer_wall = started.elapsed();
    let drain_start = Instant::now();
    target_tx
        .send(Some(stats.events))
        .expect("reader holds the watch receiver");
    let delivered = tokio::time::timeout(DRAIN_WAIT, readers.join_next())
        .await
        .expect("whole drain exceeded its bound")
        .expect("reader task joined")
        .expect("reader task did not panic")
        .expect("reader drained the target exactly");
    drop(readers);
    assert_eq!(
        delivered, stats.events,
        "drain delivered exactly the successful events"
    );
    (
        stats,
        writer_wall,
        Some((delivered, drain_start.elapsed().as_micros() as u64)),
    )
}

async fn run_feed_case(
    pool: &PgPool,
    config: &Config,
    condition: Condition,
    category: &str,
) -> (Stats, Duration, Option<(u64, u64)>) {
    let store = PgEventStreams::<String, BenchEvent>::new(pool.clone(), category);
    let feed = PgEventFeed::<BenchEvent>::new(pool.clone(), category);
    let group = ConsumerGroup::new("bench-consumers").expect("non-empty group");
    let ack = matches!(condition, Condition::FeedAckCycles);
    store.migrate().await.expect("untimed migration");

    // Untimed seed through the public append API before warmup and
    // timing; the writer stays quiescent for the whole timed phase.
    let seed = match condition {
        Condition::FeedEmpty => 0,
        Condition::FeedReplay => FEED_BACKLOG,
        Condition::FeedAckCycles => config.ops + WARMUP_OPS,
        _ => unreachable!("feed case dispatched {condition:?}"),
    };
    if seed > 0 {
        let events: Vec<BenchEvent> = (0..seed).map(|_| BenchEvent::one()).collect();
        store
            .append(
                ExpectedVersion::NoStream,
                &format!("{category}/seed"),
                &EventBatch::new(events).expect("seed events are nonempty"),
            )
            .await
            .expect("untimed seed append");
    }

    for _ in 0..WARMUP_OPS {
        feed_attempt(&feed, &group, ack)
            .await
            .expect("warmup feed call");
    }

    let started = Instant::now();
    let mut stats = Stats {
        expected: config.ops,
        ..Stats::default()
    };
    for _ in 0..config.ops {
        stats.record(feed_attempt(&feed, &group, ack).await);
    }
    (stats, started.elapsed(), None)
}

fn render(
    config: &Config,
    condition: Condition,
    round: usize,
    stats: &Stats,
    wall: Duration,
    drain: Option<(u64, u64)>,
    pg: &serde_json::Value,
) -> serde_json::Value {
    let error_count: u64 = stats.errors.values().sum();
    assert_eq!(
        stats.expected, config.ops,
        "every condition expects config.ops"
    );
    assert_eq!(
        stats.completed, stats.expected,
        "every attempt completes exactly once"
    );
    assert_eq!(
        stats.successes,
        stats.samples_us.len(),
        "samples align with successes"
    );
    assert_eq!(
        stats.completed as u64,
        stats.successes as u64 + error_count,
        "completed = successes + errors"
    );
    let latency = |samples: &[u64]| {
        Stats::percentiles(samples).map(|(p50, p95, p99, max)| {
            serde_json::json!({
                "p50_us": p50, "p95_us": p95, "p99_us": p99, "max_us": max, "n": samples.len()
            })
        })
    };
    let latencies = latency(&stats.samples_us);
    let (timer_scope, op_unit, page_size) = match condition {
        Condition::Append(_) => (
            "append() call: key/batch prep excluded, funnel + serialization + commit included",
            "append (1 event)",
            None,
        ),
        Condition::Batch => (
            "batch builder construction through transact commit",
            "atomic batch (3 streams x 2 events)",
            None,
        ),
        Condition::Mixed => (
            "writer attempts only; concurrent feed drain excluded from wall",
            "mixed append or batch attempt",
            Some(MIXED_POLL_LIMIT),
        ),
        Condition::FeedEmpty | Condition::FeedReplay => (
            "poll only (1 db transaction per attempt)",
            "poll page",
            Some(FEED_PAGE),
        ),
        Condition::FeedAckCycles => (
            "poll + ack (2 db transactions per cycle)",
            "poll+ack cycle (1 event)",
            Some(1),
        ),
    };
    let seconds = wall.as_secs_f64();
    let mut record = serde_json::json!({
        "condition": condition.name(),
        "round": round,
        "profile": match config.profile {
            Profile::Measurement => "measurement",
            Profile::Smoke => "smoke",
        },
        "measured": config.profile == Profile::Measurement,
        "mode": match config.mode {
            Mode::Writers => "writers",
            Mode::Feed => "feed",
        },
        "ops": {
            "expected": stats.expected,
            "completed": stats.completed,
            "successes": stats.successes,
        },
        "errors_by_debug": &stats.errors,
        "error_count": error_count,
        "success_latency_us": latencies,
        "successful_operations_per_s": stats.successes as f64 / seconds,
        "events": stats.events,
        "events_per_s": stats.events as f64 / seconds,
        "timer_scope": timer_scope,
        "op_unit": op_unit,
        "page_size": page_size,
        "pool": POOL_SIZE,
        "payload_bytes": PAYLOAD_BYTES,
        "config": {
            "ops": config.ops,
            "rounds": config.rounds,
            "only_round": config.only_round,
            "database": config.database,
        },
        "protocol_rounds": config.rounds,
        "source_stamp": std::env::var("EPOCH_BENCH_SOURCE").unwrap_or_default(),
        "package_version": env!("CARGO_PKG_VERSION"),
        "pg": pg,
    });
    if !stats.by_kind.is_empty() {
        let kinds: serde_json::Map<String, serde_json::Value> = stats
            .by_kind
            .iter()
            .map(|(kind, samples)| {
                (
                    kind.to_string(),
                    latency(samples).unwrap_or(serde_json::Value::Null),
                )
            })
            .collect();
        record["latency_by_kind_us"] = serde_json::Value::Object(kinds);
    }
    if let Some((delivered, lag_us)) = drain {
        record["drained_events"] = delivered.into();
        record["drain_lag_us"] = lag_us.into();
    }
    record
}

async fn run_case(
    pool: &PgPool,
    config: &Config,
    pg: &serde_json::Value,
    condition: Condition,
    round: usize,
) -> serde_json::Value {
    // One fresh category per condition round; nothing is ever deleted.
    let category = format!(
        "bench-{}-{}-r{round}",
        condition.name(),
        rusty_ulid::generate_ulid_string()
    );
    let (stats, wall, drain) = match condition {
        Condition::Mixed => run_mixed_case(pool, config, &category).await,
        Condition::FeedEmpty | Condition::FeedReplay | Condition::FeedAckCycles => {
            run_feed_case(pool, config, condition, &category).await
        }
        Condition::Append(_) | Condition::Batch => {
            run_write_case(pool, config, condition, &category).await
        }
    };
    render(config, condition, round, &stats, wall, drain, pg)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "bench harness: owner-only against an epoch_e19_bench_* database"]
async fn funnel_bench() {
    let _ = dotenv::dotenv();
    let config = Config::from_env().unwrap_or_else(|error| panic!("bench config refused: {error}"));
    println!(
        "funnel_bench: mode={:?} profile={:?} ops={} rounds={} budget={:?}",
        config.mode, config.profile, config.ops, config.rounds, BUDGET
    );
    let run = tokio::time::timeout(BUDGET, async {
        let pool = bench_pool(&config.connection).await;
        let pg = pg_environment(&pool).await;
        let rounds = match config.only_round {
            Some(selected) => selected..=selected,
            None => 1..=config.rounds,
        };
        for condition in Condition::all(config.mode) {
            for round in rounds.clone() {
                let record = run_case(&pool, &config, &pg, condition, round).await;
                println!("{record}");
            }
        }
    })
    .await;
    if run.is_err() {
        panic!(
            "bench budget {BUDGET:?} expired: the case in flight is unfinished and emits no \
             record; the records printed above are complete and retained"
        );
    }
}
