# Prep: port the sqlite work onto the redesign lineage

Status: **prepared, not started**. Implementation begins in a fresh session.
Written 2026-10-30 after the lineage-orientation pass; uncommitted, local
only — do not push until the maintainer has played with the result.

## Goal

Get the `s4/sqlite-repository` work (old-surface sqlite repository +
postgres pair-keying) onto the real streams trunk,
`origin/redesign/bootstrap`, so a sqlite **streams** backend can be
carded and built on the redesign surface.

## Lineage facts (verified 2026-10-30)

- **Trunk**: `origin/redesign/bootstrap`, tip `eee66d0` ("prepare
  epoch-journal 0.2.0-alpha.1"). Contains the complete `src/streams/`
  module (E12 -> E19 -> E20 via PR #16/#17; published as 0.2.0-alpha.1).
  NOT merged to `main`; `main` is still pre-redesign.
- **Our branch**: `s4/sqlite-repository`, tip `d869a4f`, 16 commits the
  trunk lacks; the trunk has ~101 commits we lack. Merge-base `b425427`.
- Board at `~/dev/epoch-board` (cards E-prefix; boardkit CLI not on
  PATH — read card frontmatter directly). Note: board's
  `boardkit.toml` says `[review].repo = "../epoch-postgres"`; on this
  machine the checkout is `~/dev/Epoch`.
- **No sqlite streams-backend card exists.** Model a new one on E13
  (KurrentDB) / E14 (redis): `depends: [E3]`, gates `S -> A -> M -> U`.
  Open question the card must answer: does the sqlite streams backend
  also implement `EventFeed` (E13/E14 defer this; E19 made the feed a
  first-class obligation).
- The board's own redesign->main integration is **E24** (backlog;
  gated on E15 + E7; staged PR A->B->C->D decomposition, not yet run).
  Our port is prep for the sqlite backend card, orthogonal to E24.

## The 6 s4 commits to port (cherry-pick set)

In order, oldest first:

1. `f1fecef` feat(sqlite): lay the sqlite repository type surface
2. `a7ed613` test(sqlite): wire generic spec suites against the skeleton
3. `c38dfc8` feat(sqlite): fill sqlite repository bodies
4. `3adc85e` refactor(postgres): key streams on the type-key pair
5. `8f9ebfa` docs(sqlite): document the sqlite feature and audit the budget
6. `d869a4f` docs: record session progress for the pr review

The other 10 of the 16 are `main`-history commits (0.1.x releases, CI,
toolchain pins) that E24 owns integrating — do NOT cherry-pick those
here; decide explicitly (below).

## Conflict forecast

| File | Why it conflicts | Resolution notes |
|---|---|---|
| `Cargo.toml` | Real dependency divergence: bootstrap pins `eventstore = "2.2.0"` and adds `trait-variant`; s4/main has `eventstore = "4.0.0"`, the `uuid <1.27` MSRV cap, `tokio-rusqlite`, `bb8*`. Maintainer decision (2026-10-30): the 2.2 pin is not load-bearing — take **4.0.0** contingent on the esdb tests passing; eventstore is low-priority if it causes trouble. Take both sides' other additions. | Highest-risk conflict; resolve deliberately, not by taking "ours". |
| `src/lib.rs` | Bootstrap adds `pub mod streams;`; s4 doesn't touch it. | Trivial: union. |
| `CHANGELOG.md` | Both added Unreleased entries. | Union; keep the 0.2.0-alpha.1 section from bootstrap. |
| `README.md` | `8f9ebfa` documents the sqlite feature; bootstrap's README documents streams. | Union; sqlite section sits under the old-surface repositories heading. |
| `src/repository/postgres/` | `3adc85e` pair-keys the OLD postgres repository. Bootstrap still has the old surface (it coexists with `src/streams/`), so this should apply cleanly — but verify the old postgres backend on bootstrap hasn't drifted from `b425427`. | Verify then apply. |

## Decision the new session must make first

Scope of the port — two options:

- **A (recommended): cherry-pick only the 6 commits onto a branch off
  `redesign/bootstrap`.** The branch then lags main's CI/toolchain
  (1.95 pin, lockfile, uuid cap) — acceptable for building the sqlite
  streams backend, which is redesign-side work. E24 reconciles main
  later.
- **B: merge `main` into the new branch first, then cherry-pick.**
  Brings CI/release plumbing forward now at the cost of doing a piece
  of E24's job ad hoc.

## Key architecture facts for the port (frontier-reviewer findings)

- The redesign is **strictly additive**: `src/repository/` (old) and
  `src/streams/` (new) coexist on bootstrap. Our sqlite repository
  backend lands as old-surface code and stays valid; no conflict with
  the streams module by design.
- A sqlite **streams** backend is net-new (`src/streams/sqlite.rs` +
  migrations), NOT an adapter over `SqliteEventRepository`: trait
  shapes (`EventStreams` uses `&self` append returning
  `StreamSequence`), version types (`StreamSequence(NonZeroU64)` vs
  `SqliteVersion(i64)`), and error types (`AppendError`/`LoadError` vs
  `VersionedRepositoryError`) are all different.
- The old sqlite schema is a **near-fit** for the streams table:
  `(stream_type, stream_key)` ≈ `(category, stream_key)`, 1-based
  `sequence`, AUTOINCREMENT `global_sequence`. Gaps a streams schema
  must add: `event_metadata` column (envelope seam), a feed-cursors
  table (per-group watermark), the intent-key uniqueness index
  (expression index on metadata), and the `stream_type`->`category`
  naming alignment.
- SQLite's `BEGIN IMMEDIATE` single-writer model satisfies the feed's
  serialized-writer funnel contract structurally (insert order = commit
  order), per the sqlite design record and the frontier review.

## Suggested first actions in the new session

1. `git switch -c s5/sqlite-on-redesign origin/redesign/bootstrap`
   (name adjustable; board convention is `<wave>/<topic>`).
2. Cherry-pick the 6 commits in order; resolve conflicts per the table.
3. Gates per board convention for backend cards: `cargo check`, the
   feature matrix (`--no-default-features`, `sqlite`, `postgres`,
   `--all-features`), `cargo fmt --check`,
   `cargo clippy --all-targets --all-features -- -D warnings`, and the
   full test suite including the redesign's spec/golden suites.
4. Card the sqlite streams backend on `~/dev/epoch-board` (model on
   E13/E14; answer the EventFeed question in the card).
5. Report to the maintainer before any push — this stays local until
   reviewed.

## Housekeeping from the orientation session

- A divergent `src/streams/` skeleton (wrong lineage: `StreamsError`,
  `StreamName`, ULID `StreamId`, `AddressingScheme`) was built and
  **discarded uncommitted**; the working tree is back to committed
  `s4/sqlite-repository` state. Do not resurrect it — the real surface
  is `src/streams.rs` + `src/streams/` on `redesign/bootstrap`.
