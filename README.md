# Epoch

Epoch is a Rust event-sourcing library built around deciders and event
repositories. It separates the decision to emit events from the work of
storing them. `epoch-journal` is the package name on crates.io; the Rust
import remains `epoch`.

The library grew out of small personal projects, heavily influenced by
[Thalo](https://github.com/thalo-rs/thalo). Ideas from Haskell
[Eventful](https://github.com/jdreaver/eventful), F#
[Equinox](https://github.com/jet/equinox), and Kotlin
[f(model)](https://github.com/fraktalio/fmodel) have also informed its
direction. The API is still changing before 1.0.

## Start here

Add the package under its Rust import name:

```toml
[dependencies]
epoch = { package = "epoch-journal", version = "0.1.0", default-features = false }
```

The example below uses the pure `Decider` and `Evolver` traits. It needs
no database, runtime, or feature flag. Put it in `src/main.rs` and run
`cargo run`; it prints `1`. The same program lives in
[`examples/counter.rs`](examples/counter.rs), where the repository
build checks it as an example target.

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

fn main() {
    let state = 0;
    let events = Counter::decide(&state, &()).unwrap();
    let next = events.iter().fold(state, Counter::evolve);
    assert_eq!(next, 1);
    println!("{next}");
}
```

`decide` produces events from a command and current state; `evolve`
folds those events into a new state. A repository stores the resulting
events. The example stops at the domain boundary so it can run without
choosing a backend.

## Repositories and features

`src/decider.rs` defines the domain traits. `src/repository/` holds
repository interfaces and backend implementations; `src/strategies/`
composes repository operations with deciders. The `in_memory` feature
needs no service. The default feature set enables `in_memory`, `esdb`
(EventStoreDB), and `redis` (RedisJSON). `postgres` is opt-in and
provides a PostgreSQL repository. Enable only the backend you use, for
example:

```toml
[dependencies]
epoch = { package = "epoch-journal", version = "0.1.0", default-features = false, features = ["postgres"] }
```

This release has no `streams`, atomic-batch, feed, saga, or outbox API.
Backend version semantics differ; consult the repository trait and
backend implementation when moving stored events between backends.
The PostgreSQL repository applies its schema with
`PgEventRepository::migrate` before use. Deciders do not persist
anything on their own.

## Build and test

Rust 1.88 or later is required. For the example and the pure domain
surface, `cargo test --no-default-features` needs no external services.
The default suite exercises EventStoreDB and Redis; the PostgreSQL
tests run only with its feature enabled. From a clone of the repository:

```sh
cp .env.example .env
docker compose up -d
cargo test
cargo test --features postgres
cargo test --doc
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

Docker Compose supplies EventStoreDB, RedisJSON, and PostgreSQL.
`.env.example` names every connection string used by the integration
tests. These services and credentials are local test fixtures; use your
own connection settings in an application. The PostgreSQL tests call
`migrate` and require `EPOCH_PG_TEST_URL` rather than choosing a
database implicitly. Run `docker compose down` when finished.

## Versions and contributions

`0.1.0` is the first crates.io release of `epoch-journal`. Earlier
`1.0.0-alpha.*` versions were repository versions, not publications
under this package name. Existing git consumers that declare
`epoch = { git = "https://github.com/Shearerbeard/Epoch" }` must update
their Cargo manifest to use the package declaration above; keeping
the dependency key `epoch` keeps source imports the same.

See [CHANGELOG.md](CHANGELOG.md) for changes and earlier repository
history. [LICENSE](LICENSE) contains the Apache-2.0 terms. To contribute,
open a pull request with the relevant `cargo test` feature set,
`cargo fmt --check`, and `cargo clippy --all-targets --all-features --
-D warnings` results. CI runs the build checks on pull requests.
