# Epoch contributor context

Start with [README.md](README.md). It owns the install, example,
feature, build, and test instructions. [CHANGELOG.md](CHANGELOG.md)
records changes. The `TODO.md` file is an archived 2025 snapshot, not
the current release plan.

## Branch lineage (read this first)

This checkout's branches are diverged; knowing which surface you are on
matters more than anything else in this file.

- **`main`** — pre-redesign public head (epoch-journal 0.1.x). Has the
  old repository surface only; no `src/streams/` module.
- **`origin/redesign/bootstrap`** (tip `eee66d0`, released as
  `0.2.0-alpha.1`) — the streams trunk. Built by board cards
  E12 -> E19 -> E20 (via PR #16/#17) on the E1/E2/E3/E10 foundation.
  Holds the complete `src/streams/` module: the `StreamId` trait,
  `StreamSequence`, `EventStreams`, `AtomicStreams`/batch, the
  `EventFeed` with consumer groups and ack cursors, and the saga runner
  with its outbox executor. Backends: `in_memory`, `postgres`.
- **`s4/sqlite-repository`** — old-surface work branched from
  pre-redesign `main`: the sqlite repository backend
  (`src/repository/sqlite/`) and the postgres pair-keying refactor.
  Six commits (`f1fecef`..`d869a4f`) to be ported onto this branch; the
  port plan lives in
  [docs/internal/planning/sqlite-on-redesign-integration-prep.md](docs/internal/planning/sqlite-on-redesign-integration-prep.md).
- **`s5/sqlite-on-redesign`** (this branch) — integration branch off
  `redesign/bootstrap` for that port. Nothing pushed; local until the
  maintainer reviews.

## Two coexisting surfaces

The redesign is strictly additive. `src/repository/` (old:
`VersionedEventRepositoryWithStreams`, String stream ids, per-backend
version types) and `src/streams/` (new: typed `StreamId`,
`StreamSequence(NonZeroU64)`, `EventStreams`) coexist on this branch.
New work on the redesign surface goes in `src/streams/`; the old
surface stays put until the board assigns a teardown.

A sqlite **streams** backend is net-new (`src/streams/sqlite.rs` +
migrations implementing `EventStreams`/`AtomicStreams`/`EventFeed`),
not an adapter over the old `SqliteEventRepository` — trait shapes,
version types, and error types differ. No card exists for it yet;
model one on board cards E13/E14 and answer whether it implements
`EventFeed`.

## Board

The workboard lives at `~/dev/epoch-board` (cards `E`-prefix under
`docs/board/cards/`; card frontmatter is the source of truth). Its
`boardkit.toml` names `../epoch-postgres` as the review repo; on this
machine the checkout is `~/dev/Epoch`. The official redesign -> `main`
integration is card **E24** (backlog; gated on E15 + E7) — do not
improvise it here.

`src/decider.rs` defines the domain interfaces. The public counter
example is in `examples/counter.rs` and appears verbatim in the README.

Treat the README's Rust block as executable: it is included in crate
docs and runs under `cargo test --doc`. Run the relevant feature tests
and the checks named in the README when changing behavior or docs.
