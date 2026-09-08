//! C15 funnel benchmark skeleton (E19 repair): the real public write and
//! feed APIs measured on a dedicated postgres database. Feature-gated and
//! `#[ignore]`d; owner-only. Copies unchanged into the 5803b97 worktree.
#![cfg(feature = "postgres")]

use std::collections::BTreeMap;
use std::time::Duration;

use epoch::decider::Event;
use epoch::streams::feed::ConsumerGroup;
use epoch::streams::postgres::{PgDatabase, PgEventFeed, PgEventStreams, PgPool};
use serde::{Deserialize, Serialize};

#[expect(dead_code)]
const POOL_SIZE: usize = 32;
#[expect(dead_code)]
const WARMUP_OPS: usize = 100;
const PAYLOAD_BYTES: usize = 1024;
#[expect(dead_code)]
const BUDGET: Duration = Duration::from_secs(20 * 60);
const APPEND_CONCURRENCIES: [usize; 4] = [1, 4, 8, 16];
#[expect(dead_code)]
const BATCH_CALLERS: usize = 4;
#[expect(dead_code)]
const BATCH_STREAMS: usize = 3;
#[expect(dead_code)]
const BATCH_EVENTS: usize = 2;
#[expect(dead_code)]
const MIXED_CALLERS: usize = 8;
#[expect(dead_code)]
const MIXED_POLL_LIMIT: usize = 256;
#[expect(dead_code)]
const FEED_PAGE: usize = 32;
#[expect(dead_code)]
const FEED_BACKLOG: usize = 32;
#[expect(dead_code)]
const DRAIN_WAIT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct BenchEvent {
    payload: String,
}

impl BenchEvent {
    #[expect(dead_code)]
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

#[expect(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Writers,
    Feed,
}

#[expect(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Profile {
    Measurement,
    Smoke,
}

#[expect(dead_code)]
struct Config {
    mode: Mode,
    profile: Profile,
    ops: usize,
    rounds: usize,
    database: String,
    connection: tokio_postgres::Config,
}

impl Config {
    #[expect(dead_code)]
    fn from_env() -> Result<Self, String> {
        todo!("parse env; refuse non-epoch_e19_bench_* db; measured iff ops>=10000 && rounds>=3")
    }
}

#[expect(dead_code)]
#[derive(Default)]
struct Stats {
    expected: usize,
    completed: usize,
    successes: usize,
    events: u64,
    samples_us: Vec<u64>,
    errors: BTreeMap<String, u64>,
}

impl Stats {
    #[expect(dead_code, unused_variables)]
    fn record(&mut self, attempt: Result<(u64, u64), String>) {
        todo!("increment completed every attempt; fold success/error into samples/errors_by_debug")
    }

    #[expect(dead_code)]
    fn percentiles(&self) -> Option<(u64, u64, u64, u64)> {
        todo!("p50/p95/p99/max over samples; None when empty")
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
    #[expect(dead_code)]
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

    #[expect(dead_code)]
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

#[expect(dead_code, unused_variables)]
async fn bench_pool(connection: &tokio_postgres::Config) -> PgPool {
    todo!("build 32-lease pool and warm by holding every lease")
}

#[expect(dead_code, unused_variables)]
async fn pg_environment(pool: &PgPool) -> serde_json::Value {
    todo!("probe version/fsync/synchronous_commit")
}

#[expect(dead_code, unused_variables)]
async fn append_attempt(
    store: &PgEventStreams<String, BenchEvent>,
    key: &str,
) -> Result<(u64, u64), String> {
    todo!("allocate key String + batch before timer; one timed 1 KiB append")
}

#[expect(dead_code, unused_variables)]
async fn batch_attempt(
    db: &PgDatabase,
    category: &str,
    keys: &[String],
) -> Result<(u64, u64), String> {
    todo!("timer spans builder through commit; 3 streams x 2 events")
}

#[expect(dead_code, unused_variables)]
async fn feed_attempt(
    feed: &PgEventFeed<BenchEvent>,
    group: &ConsumerGroup,
    ack: bool,
) -> Result<(u64, u64), String> {
    todo!("one timed poll, optional ack")
}

#[expect(dead_code, unused_variables)]
async fn run_write_case(
    pool: &PgPool,
    config: &Config,
    condition: Condition,
    category: &str,
) -> (Stats, Duration, Option<(u64, u64)>) {
    todo!("migrate; warmup; spawn append/batch workers; fold exact ops")
}

#[expect(dead_code, unused_variables)]
async fn run_mixed_case(
    pool: &PgPool,
    config: &Config,
    category: &str,
) -> (Stats, Duration, Option<(u64, u64)>) {
    todo!("8 callers alternate append/batch KIND; feed concurrent; drain warmup before timer; target successful events (6/batch)")
}

#[expect(dead_code, unused_variables)]
async fn run_feed_case(
    pool: &PgPool,
    config: &Config,
    condition: Condition,
    category: &str,
) -> (Stats, Duration, Option<(u64, u64)>) {
    todo!("untimed seed; replay PAGE 32, cycles PAGE 1 over one group advancing ops+warmup backlog")
}

#[expect(dead_code, unused_variables)]
fn render(
    config: &Config,
    condition: Condition,
    round: usize,
    stats: &Stats,
    wall: Duration,
    drain: Option<(u64, u64)>,
    pg: &serde_json::Value,
) -> serde_json::Value {
    todo!("assert completed==successes+errors && successes==samples.len() && completed==expected==ops; report page_size/op_unit/timer_scope")
}

#[expect(dead_code, unused_variables)]
async fn run_case(
    pool: &PgPool,
    config: &Config,
    pg: &serde_json::Value,
    condition: Condition,
    round: usize,
) -> serde_json::Value {
    todo!("fresh category; dispatch to write/mixed/feed case; render")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "bench harness: owner-only against an epoch_e19_bench_* database"]
async fn funnel_bench() {
    todo!("parse config; build+warm pool; probe env; run each condition x round under BUDGET")
}
