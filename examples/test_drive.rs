//! Test-drive CLI for event journal implementations: a local dev
//! harness, not an integration test and not a shipped feature.
//!
//! Seeds a deterministic event set into a chosen backend, then
//! inspects it: list streams, dump the events in a stream, or reify
//! the current state at a stream address. The same operations run
//! across backends so a maintainer can compare behavior
//! backend-to-backend. Identical seeds on two backends produce
//! diffable-identical inspection output, modulo the `backend:` header
//! line that names the backend.
//!
//! Usage (from the crate root):
//!
//! ```sh
//! cargo run --no-default-features --features in_memory,sqlite --example test_drive -- \
//!     --backend sqlite --db /tmp/ejtd.db seed
//! cargo run --no-default-features --features in_memory,sqlite --example test_drive -- \
//!     --backend sqlite --db /tmp/ejtd.db streams
//! cargo run --no-default-features --features in_memory,sqlite --example test_drive -- \
//!     --backend sqlite --db /tmp/ejtd.db events counter/alpha
//! cargo run --no-default-features --features in_memory,sqlite --example test_drive -- \
//!     --backend sqlite --db /tmp/ejtd.db reify counter/alpha
//! ```
//!
//! `--backend in-memory` needs no `--db`; every invocation seeds the
//! deterministic set before running the command, since an in-memory
//! store lives only for the process (the seeding note goes to stderr,
//! keeping stdout inspection-only).
//!
//! Both backends are driven over the old `src/repository/` surface
//! (`VersionedEventRepositoryWithStreams`), where both exist. The
//! `Journal` impls at the bottom are the only backend-specific code;
//! every command runs through the generic driver.

use std::process::ExitCode;

use async_trait::async_trait;
use epoch::{
    decider::{Event, Evolver},
    repository::RepositoryVersion,
};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// The fixture: a tiny counter decider local to this harness. Deterministic
// by construction - the seed script below is fixed, with no clock, no
// randomness, and no environment input.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum CounterEvent {
    Incremented { by: u64 },
    Decremented { by: u64 },
    Named { name: String },
}

impl Event for CounterEvent {
    // The old repository surface keys streams on `StreamId = String`;
    // this harness names its streams explicitly and never derives one
    // from an event, so the entity id carries nothing.
    type EntityId = ();

    fn event_type(&self) -> String {
        match self {
            CounterEvent::Incremented { .. } => "Incremented".to_owned(),
            CounterEvent::Decremented { .. } => "Decremented".to_owned(),
            CounterEvent::Named { .. } => "Named".to_owned(),
        }
    }

    fn get_id(&self) -> Self::EntityId {}
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct CounterState {
    name: Option<String>,
    value: i64,
}

struct Counter;

impl Evolver for Counter {
    type State = CounterState;
    type Evt = CounterEvent;

    fn evolve(mut state: CounterState, event: &CounterEvent) -> CounterState {
        match event {
            CounterEvent::Incremented { by } => state.value += *by as i64,
            CounterEvent::Decremented { by } => state.value -= *by as i64,
            CounterEvent::Named { name } => state.name = Some(name.clone()),
        }
        state
    }
}

/// The deterministic seed script: (stream address, events) in append
/// order. Every backend receives exactly this.
fn seed_script() -> Vec<(&'static str, Vec<CounterEvent>)> {
    vec![
        (
            "counter/alpha",
            vec![
                CounterEvent::Named {
                    name: "alpha".to_owned(),
                },
                CounterEvent::Incremented { by: 1 },
                CounterEvent::Incremented { by: 2 },
            ],
        ),
        (
            "counter/beta",
            vec![
                CounterEvent::Named {
                    name: "beta".to_owned(),
                },
                CounterEvent::Incremented { by: 10 },
                CounterEvent::Decremented { by: 3 },
            ],
        ),
        ("counter/gamma", vec![CounterEvent::Decremented { by: 5 }]),
    ]
}

// ---------------------------------------------------------------------------
// The backend-agnostic driver. `Journal` is the seam: one small impl per
// backend in the setup section below, everything else speaks `Journal`.
// ---------------------------------------------------------------------------

/// A stream's head, normalized for cross-backend output. The raw
/// version values stay inside the backends: the old surface's version
/// schemes differ per backend (the in-memory backend's 0-based event
/// index, sqlite's 1-based sequence), so printing them would make
/// identical seeds diff dirty by design. The variant plus the event
/// count is what identical seeds can be diffed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Head {
    Exact,
    NoStream,
    StreamExists,
    Any,
}

impl Head {
    fn as_str(&self) -> &'static str {
        match self {
            Head::Exact => "exact",
            Head::NoStream => "no-stream",
            Head::StreamExists => "stream-exists",
            Head::Any => "any",
        }
    }
}

impl<V> From<&RepositoryVersion<V>> for Head {
    fn from(version: &RepositoryVersion<V>) -> Self {
        match version {
            RepositoryVersion::Any => Head::Any,
            RepositoryVersion::Exact(_) => Head::Exact,
            RepositoryVersion::NoStream => Head::NoStream,
            RepositoryVersion::StreamExists => Head::StreamExists,
        }
    }
}

#[async_trait]
trait Journal {
    /// Append one stream's seed events, `NoStream` first and then the
    /// returned `Exact` version, so a second `seed` run against the
    /// same store fails loudly instead of double-seeding.
    async fn seed_stream(&mut self, stream: &str, events: &[CounterEvent]) -> Result<(), String>;

    /// Load a stream's events by address, with the normalized head.
    async fn load_stream(&self, stream: &str) -> Result<(Vec<CounterEvent>, Head), String>;
}

fn render_event(event: &CounterEvent) -> String {
    // JSON via serde keeps the rendering stable across backends: both
    // deserialize into the same type, and this function is the single
    // place an event becomes text.
    serde_json::to_string(event).expect("fixture events serialize")
}

fn print_table(columns: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = columns.iter().map(|c| c.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    // The last column is never padded: trailing whitespace has no place
    // in output meant to be diffed.
    let last = columns.len() - 1;
    let line = |i: usize, cell: &str| {
        if i == last {
            cell.to_owned()
        } else {
            format!("{:<w$}", cell, w = widths[i])
        }
    };
    let header: Vec<String> = columns
        .iter()
        .enumerate()
        .map(|(i, c)| line(i, c))
        .collect();
    println!("{}", header.join("  "));
    let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    println!("{}", sep.join("  "));
    for row in rows {
        let rendered: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, cell)| line(i, cell))
            .collect();
        println!("{}", rendered.join("  "));
    }
}

async fn cmd_seed<J: Journal>(journal: &mut J) -> Result<(), String> {
    for (stream, events) in seed_script() {
        journal.seed_stream(stream, &events).await?;
    }
    println!("seeded {} streams", seed_script().len());
    Ok(())
}

/// Listing replays each seed address's head: the old surface's
/// category read (`load(None)`) returns events without the stream key
/// each came from, so the seed script's known addresses are the
/// listing mechanism.
async fn cmd_streams<J: Journal>(journal: &J) -> Result<(), String> {
    let mut rows = Vec::new();
    for (stream, _) in seed_script() {
        let (events, head) = journal.load_stream(stream).await?;
        rows.push(vec![
            stream.to_owned(),
            events.len().to_string(),
            head.as_str().to_owned(),
        ]);
    }
    print_table(&["stream", "events", "head"], &rows);
    Ok(())
}

async fn cmd_events<J: Journal>(journal: &J, stream: &str) -> Result<(), String> {
    let (events, head) = journal.load_stream(stream).await?;
    println!(
        "stream {stream} (head: {}, {} events)",
        head.as_str(),
        events.len()
    );
    let rows: Vec<Vec<String>> = events
        .iter()
        .enumerate()
        .map(|(i, e)| vec![(i + 1).to_string(), e.event_type(), render_event(e)])
        .collect();
    print_table(&["seq", "type", "data"], &rows);
    Ok(())
}

async fn cmd_reify<J: Journal>(journal: &J, stream: &str) -> Result<(), String> {
    let (events, head) = journal.load_stream(stream).await?;
    let state = events.iter().fold(CounterState::default(), Counter::evolve);
    println!(
        "stream {stream} (head: {}, {} events)",
        head.as_str(),
        events.len()
    );
    let rows = vec![
        vec![
            "name".to_owned(),
            state.name.unwrap_or_else(|| "-".to_owned()),
        ],
        vec!["value".to_owned(), state.value.to_string()],
    ];
    print_table(&["field", "value"], &rows);
    Ok(())
}

// ---------------------------------------------------------------------------
// Backend setup: the only backend-specific code in the harness.
// ---------------------------------------------------------------------------

enum Backend {
    InMemory,
    Sqlite { db: String },
}

enum Command {
    Seed,
    Streams,
    Events(String),
    Reify(String),
}

struct Args {
    backend: Backend,
    command: Command,
}

const USAGE: &str = "usage: test_drive --backend <in-memory|sqlite> [--db PATH] \
                     <seed|streams|events STREAM|reify STREAM>";

fn parse_args() -> Result<Args, String> {
    let mut backend: Option<Backend> = None;
    let mut db: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();

    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--backend" => {
                let name = argv.next().ok_or("--backend needs a value")?;
                backend = Some(match name.as_str() {
                    "in-memory" | "in_memory" => Backend::InMemory,
                    "sqlite" => Backend::Sqlite { db: String::new() },
                    other => return Err(format!("unknown backend {other:?}\n{USAGE}")),
                });
            }
            "--db" => {
                db = Some(argv.next().ok_or("--db needs a value")?);
            }
            "-h" | "--help" => return Err(USAGE.to_owned()),
            other => positional.push(other.to_owned()),
        }
    }

    if let (Some(Backend::Sqlite { db: slot }), Some(path)) = (&mut backend, db) {
        *slot = path;
    }

    let backend = backend.ok_or(format!("missing --backend\n{USAGE}"))?;
    if let Backend::Sqlite { db } = &backend {
        if db.is_empty() {
            return Err(format!("the sqlite backend needs --db PATH\n{USAGE}"));
        }
    }

    let command = match positional.as_slice() {
        [cmd] if cmd == "seed" => Command::Seed,
        [cmd] if cmd == "streams" => Command::Streams,
        [cmd, stream] if cmd == "events" => Command::Events(stream.clone()),
        [cmd, stream] if cmd == "reify" => Command::Reify(stream.clone()),
        _ => return Err(format!("unknown command\n{USAGE}")),
    };

    Ok(Args { backend, command })
}

const STREAM_TYPE: &str = "counter";

#[cfg(feature = "in_memory")]
mod in_memory_setup {
    use epoch::repository::{
        event::VersionedEventRepositoryWithStreams,
        in_memory::versioned_with_streams::InMemoryEventRepository,
    };

    use super::*;

    pub type Repo = InMemoryEventRepository<CounterEvent>;

    pub fn build() -> Repo {
        InMemoryEventRepository::new(STREAM_TYPE)
    }

    #[async_trait]
    impl Journal for Repo {
        async fn seed_stream(
            &mut self,
            stream: &str,
            events: &[CounterEvent],
        ) -> Result<(), String> {
            // Chunked appends exercise the version handoff, not just
            // the empty-stream case.
            let mut version = RepositoryVersion::NoStream;
            for chunk in events.chunks(2) {
                let (_, next) = self
                    .append(&version, &stream.to_owned(), &chunk.to_vec())
                    .await
                    .map_err(|e| format!("append to {stream}: {e:?}"))?;
                version = next;
            }
            Ok(())
        }

        async fn load_stream(&self, stream: &str) -> Result<(Vec<CounterEvent>, Head), String> {
            let (events, version) = self
                .load(Some(&stream.to_owned()))
                .await
                .map_err(|e| format!("load {stream}: {e:?}"))?;
            Ok((events, Head::from(&version)))
        }
    }
}

#[cfg(feature = "sqlite")]
mod sqlite_setup {
    use epoch::repository::{
        event::VersionedEventRepositoryWithStreams, sqlite::SqliteEventRepository,
    };

    use super::*;

    pub type Repo = SqliteEventRepository<CounterEvent>;

    pub async fn build(db: &str) -> Result<Repo, String> {
        let repo = SqliteEventRepository::<CounterEvent>::open(db, STREAM_TYPE)
            .await
            .map_err(|e| format!("open {db}: {e:?}"))?;
        repo.migrate()
            .await
            .map_err(|e| format!("migrate {db}: {e:?}"))?;
        Ok(repo)
    }

    #[async_trait]
    impl Journal for Repo {
        async fn seed_stream(
            &mut self,
            stream: &str,
            events: &[CounterEvent],
        ) -> Result<(), String> {
            let mut version = RepositoryVersion::NoStream;
            for chunk in events.chunks(2) {
                let (_, next) = self
                    .append(&version, &stream.to_owned(), &chunk.to_vec())
                    .await
                    .map_err(|e| format!("append to {stream}: {e:?}"))?;
                version = next;
            }
            Ok(())
        }

        async fn load_stream(&self, stream: &str) -> Result<(Vec<CounterEvent>, Head), String> {
            let (events, version) = self
                .load(Some(&stream.to_owned()))
                .await
                .map_err(|e| format!("load {stream}: {e:?}"))?;
            Ok((events, Head::from(&version)))
        }
    }
}

async fn run(args: Args) -> Result<(), String> {
    match args.backend {
        Backend::InMemory => {
            println!("backend: in-memory");
            let mut journal = in_memory_setup::build();
            // An in-memory store lives only for the process, so every
            // command starts from the deterministic seed. The note
            // goes to stderr to keep stdout inspection-only.
            for (stream, events) in seed_script() {
                journal.seed_stream(stream, &events).await?;
            }
            eprintln!("seeded {} streams", seed_script().len());
            match args.command {
                Command::Seed => Ok(()),
                Command::Streams => cmd_streams(&journal).await,
                Command::Events(stream) => cmd_events(&journal, &stream).await,
                Command::Reify(stream) => cmd_reify(&journal, &stream).await,
            }
        }
        Backend::Sqlite { db } => {
            println!("backend: sqlite");
            let mut journal = sqlite_setup::build(&db).await?;
            match args.command {
                Command::Seed => cmd_seed(&mut journal).await,
                Command::Streams => cmd_streams(&journal).await,
                Command::Events(stream) => cmd_events(&journal, &stream).await,
                Command::Reify(stream) => cmd_reify(&journal, &stream).await,
            }
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };

    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}
