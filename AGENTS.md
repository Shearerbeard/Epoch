# Epoch contributor context

Start with [README.md](README.md). It owns the install, example,
feature, build, and test instructions. [CHANGELOG.md](CHANGELOG.md)
records changes. The `TODO.md` file is an archived 2025 snapshot, not
the current release plan.

`src/decider.rs` defines the domain interfaces. Persistence interfaces
and backends live in `src/repository/`; `src/strategies/` combines
deciders with repositories. The public counter example is in
`examples/counter.rs` and appears verbatim in the README.

Treat the README's Rust block as executable: it is included in crate
docs and runs under `cargo test --doc`. Run the relevant feature tests
and the checks named in the README when changing behavior or docs.
