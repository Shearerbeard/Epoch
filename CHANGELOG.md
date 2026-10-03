# Changelog

## [Unreleased]

## [0.2.0-alpha.1] - 2026-10-03

First prerelease of the stream and saga redesign under the
`epoch-journal` package name. The Rust import remains `epoch`.
The alpha API can change before the stable 0.2.0 release.

### Added

- Versioned streams, atomic multi-stream batches, and an event feed
  with per-group cursors. The PostgreSQL feed persists its cursor;
  the in-memory feed's cursor lives only as long as its database.
- A saga runner that commits command events and outbox intents in
  one batch. The outbox executor performs effects through a
  caller-supplied port. Delivery is ordered and at least once;
  ports and compensation hooks must dedupe by intent key.
- PostgreSQL schema migrations, an intent-key uniqueness index for
  the outbox category, and event metadata preserved across appends
  and feed reads. The in-memory backend does not enforce the
  PostgreSQL intent uniqueness rule.

### Changed

- PostgreSQL event transactions use a single-writer funnel so a
  feed cursor cannot skip committed events. The tradeoff is
  bounded append throughput under concurrent load.
- Stream loads return `RecordedEvent` values with their stored
  metadata. `EventBatch` and `StreamSlice` accessors now expose
  records rather than bare events.
- Version and constraint failures have distinct error variants;
  lock timeouts remain retryable.
- The EventStoreDB client moves to 4.0. Test fixtures use `dotenvy`.

### Known limits

- A caller must run `PgEventStreams::migrate` before using the
  PostgreSQL stream store. The PostgreSQL writer assumes no
  pre-funnel writers during a mixed-version deployment.
- The EventStoreDB and RedisJSON backends remain on the earlier
  repository API. Neither implements the new batch or feed traits.

## [0.1.1] - 2026-10-03

- Constrain `uuid` below 1.27 so fresh default-feature consumers
  can build on Rust 1.88. CI uses a committed lockfile. The repo
  gained a tag-driven publish workflow.

## [0.1.0] - 2026-10-02

- First crates.io release of `epoch-journal` with the `epoch` Rust
  import, Decider/Evolver traits, the in-memory, EventStoreDB, and
  RedisJSON repositories, and an opt-in PostgreSQL repository.

## Repository history (unpublished)

The repository previously used `1.0.0-alpha.18` as a development
version. It was not a crates.io release of `epoch-journal`. That
code line introduced deciders, repository abstractions, and
optimistic concurrency. Git history holds its detailed changes.

[Unreleased]: https://github.com/Shearerbeard/Epoch/compare/v0.2.0-alpha.1...HEAD
[0.2.0-alpha.1]: https://github.com/Shearerbeard/Epoch/compare/v0.1.1...v0.2.0-alpha.1
[0.1.1]: https://github.com/Shearerbeard/Epoch/releases/tag/v0.1.1
[0.1.0]: https://github.com/Shearerbeard/Epoch/releases/tag/v0.1.0
