# Contributing to Epoch

Thanks for the interest. This file is for code contributions; the
release process is maintainer-only and lives in
[README "Publishing (maintainers)"](README.md#publishing-maintainers).

## Before you open a pull request

CI runs these on every PR; run them locally first. The full commands
and the Docker Compose setup for the backend services are in README
["Build and test"](README.md#build-and-test).

```sh
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --no-default-features
cargo test --locked --doc
```

Backend tests need the Compose services (`cp .env.example .env &&
docker compose up -d`), then `cargo test --locked` and
`cargo test --locked --all-features`.

## Conventions

- The README's Rust block is executable documentation: it runs as a
  doctest and appears in the crate docs. If you change the example,
  keep all three copies in sync (README, doctest, `examples/counter.rs`).
- Cargo.lock is committed and CI builds `--locked`. When your change
  adds or updates dependencies, commit the lockfile update in the same
  PR.
- The declared Rust floor is 1.88; dependency updates that raise the
  floor are release decisions, not patch material.
- Prefer making illegal states unrepresentable over runtime checks;
  the internal coding style guide elaborates.
- Prose changes to README or CHANGELOG are linted with Vale; keep to
  the style already in the file (no emojis, plain hyphens).

## Pull requests

Small, focused PRs against `main`. Describe what changed and why.
Expect CI (fmt, clippy on all targets and features, tests with and
without backends, the README doctest) to pass before review.
