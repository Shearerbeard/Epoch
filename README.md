# Epoch

Epoch is a Rust event-sourcing library built around deciders and event
repositories. The crates.io package is `epoch-journal`; its Rust import
is `epoch`.

This `0.2.0-alpha.1` preview adds versioned event streams, atomic
batches, a feed, a saga runner, and an outbox executor. The API may
change before the stable 0.2.0 release. The published 0.1.x line
does not include these stream and saga APIs.

## Start here

Select the prerelease explicitly:

```toml
[dependencies]
epoch = { package = "epoch-journal", version = "=0.2.0-alpha.1", default-features = false }
```

The `Decider` and `Evolver` traits need no database or async runtime.
The example below is checked as a doctest. Run the same code with
`cargo run --no-default-features --example counter`; it prints `1`.

```rust
use epoch::decider::{Decider, Event, Evolver};

#[derive(Debug)]
enum CounterEvent {
    Incremented,
}

impl Event for CounterEvent {
    type EntityId = ();

    fn event_type(&self) -> String {
        "Incremented".to_owned()
    }

    fn get_id(&self) -> Self::EntityId {}
}

struct Counter;

impl Evolver for Counter {
    type State = u64;
    type Evt = CounterEvent;

    fn evolve(state: u64, event: &CounterEvent) -> u64 {
        match event {
            CounterEvent::Incremented => state + 1,
        }
    }
}

impl Decider for Counter {
    type Cmd = ();
    type Err = std::convert::Infallible;

    fn decide(_state: &u64, _cmd: &()) -> Result<Vec<CounterEvent>, Self::Err> {
        Ok(vec![CounterEvent::Incremented])
    }
}

let state = 0;
let events = Counter::decide(&state, &()).unwrap();
let next = events.iter().fold(state, Counter::evolve);
assert_eq!(next, 1);
println!("{next}");
```

`decide` emits events from a command and current state. `evolve`
folds events into new state. A repository stores the events; the
example stops before choosing one.

## Repositories and streams

`src/decider.rs` defines the domain traits. `src/repository/` has
the earlier event and state repositories. `src/streams/` provides
the preview's stream API: `EventStreams` loads and appends versioned
streams, `AtomicStreams` commits a batch, and `feed::EventFeed`
tracks each consumer group's cursor. `saga::Runner` records command
events and effect intents together. `outbox::Executor` performs
effects later through a caller-supplied port.

The stream implementation is available in memory with the
`in_memory` feature and in PostgreSQL with the opt-in `postgres`
feature. The PostgreSQL writer serializes event transactions so a
feed cursor cannot skip committed events. Its intent-key uniqueness
index rejects repeated outbox intents. The in-memory backend does
not enforce that storage-level intent uniqueness. A feed may
redeliver after a crash; effect ports and compensation hooks must
dedupe by intent key. Effect delivery is ordered and at least once.

The default features also include EventStoreDB (`esdb`) and
RedisJSON (`redis`) for the earlier repository API. Those backends
do not implement the new atomic-batch or feed interfaces. For an
in-memory preview without external services:

```toml
epoch = { package = "epoch-journal", version = "=0.2.0-alpha.1", default-features = false, features = ["in_memory"] }
```

For the PostgreSQL stream backend, select `postgres` instead. Call
`PgEventStreams::migrate` before using the store. The package
requires Rust 1.88 or later.
The [architecture decisions](https://github.com/Shearerbeard/Epoch/tree/v0.2.0-alpha.1/docs/adr)
explain the batch, cursor, and outbox contracts.

## Build and test from a clone

The no-default-feature tests need no external service. Default
features exercise RedisJSON and EventStoreDB; the all-feature run
also exercises PostgreSQL.
The repository's `.env.example` lists the fixture connections:

```sh
cargo +1.88 test --locked --no-default-features
cp .env.example .env
docker compose up -d
cargo +1.88 test --locked
cargo +1.88 test --locked --all-features
cargo +1.88 fmt --check
cargo +1.88 clippy --all-targets --all-features -- -D warnings
docker compose down
```

See `CHANGELOG.md` for version history and `LICENSE` for the
Apache-2.0 terms. The stable 0.1.x package remains available for
consumers that do not need the redesign.
