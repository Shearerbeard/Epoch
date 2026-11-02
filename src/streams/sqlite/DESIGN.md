# E28 design record: the SQLite streams backend

Card: E28 (sqlite streams backend on the new surface). Board:
`~/dev/epoch-board`, card `docs/board/cards/e28-sqlite-streams-backend.md`.
Plan: `docs/board/plans/2026-11-02-e28-sqlite-streams-backend.md`.

This is the typed-holes Layer 1 design record: the type surface of
`src/streams/sqlite/` mapped to the rules it encodes and the seams it
reaches across. The holes it still carries are listed below, and so are
the risks the surface does not retire. The findings ledger the design
panel appends lives at the bottom.

## What this module is

A backend for the **new** streams surface (`src/streams/`), against the
same traits the in-memory and PostgreSQL backends implement:
`EventStreams` (ADR 0002-0005), `AtomicStreams` (ADR 0006), and
`EventFeed` (ADR 0010). It is net-new code, not an adapter over the old
`SqliteEventRepository`: the trait shapes differ, and so do the version
types (`StreamSequence(NonZeroU64)` against `SqliteVersion(i64)`) and
the error types. The old surface under `src/repository/sqlite/` is read
here as a precedent and is not touched.

It compiles under the existing `sqlite` feature, which already gates
`tokio-rusqlite` and `serde_json` - the two dependencies this backend
needs. No dependency and no `Cargo.toml` change comes with it.

## The type-to-business-rule map

Every public type below maps to exactly one business rule. A type that
could not be given one line would be two types, or a bag.

| Type | The one rule it encodes | The invalid state it forbids |
|---|---|---|
| `SqliteEventStreams<Id, E>` | One category of streams in one SQLite database, addressed by a typed id (ADR 0004). | A store that reads or writes two categories at once; a store whose category is not a namespace. |
| `SqliteStreamsError` | The backend failure surface: a fault, or a lock wait that ran out of time. | Collapsing a retryable lock timeout into a fault, or a fault into a version conflict. |
| `SqliteDatabase` | The database handle: the only value whose scope covers a cross-category batch (ADR 0006's ownership seam). | A batch that carries a connection or a store of its own, so a transact could reach a database other than the handle's. |
| `SqliteEncodedEvent` | An event erased to the backend's wire form at push, fallibly, before any transaction starts (ADR 0006's erasure contract). | A batch carrying an event that was never encoded, or an encoding failure that reaches a transaction. |
| `SqliteBatch` | A set of writes that commit as one unit or not at all, at most one write per stream. | A batch with two writes for one stream, so no write can observe another write's head. |
| `SqliteBatchBuilder` | The one route by which a typed event joins a batch. | A batch assembled outside the backend's own erasure bound. |
| `SqliteWriteError` | Why a write could not join a batch: a duplicate stream, or an encoding failure. | An encoding failure surfacing later, as a transaction failure. |
| `SqliteEventFeed<E>` | Delivery over the committed log for one consumer group, in one category (ADR 0010). | A feed whose cursor and writers sit in different categories. |

Private leaf types: `Progress` (one group's cursor and delivered
watermark, read together so the two can never disagree), `Migration`
(one numbered schema step: version, name, SQL).

## The visibility and seam table

| What the module reaches into | At what visibility | Why |
|---|---|---|
| `crate::streams::batch::expectation_satisfied` | `pub(crate)`, already there | The shared expectation predicate. This backend calls it; it does not re-implement it. |
| `crate::streams::batch::{Batch, BatchBuilder, BatchSource, StreamRef, TransactError, DuplicateWrite}` | `pub`, already there | The shared batch vocabulary, unchanged. |
| `crate::streams::feed::{EventFeed, ConsumerGroup, FeedEntry, FeedPosition, PollLimit, AckError, ...}` | `pub`, already there | The shared feed vocabulary, unchanged. |
| `crate::streams::{EventStreams, EventBatch, ExpectedVersion, StreamState, StreamSlice, CategoryEvent, LoadError, AppendError, VersionConflict, ...}` | `pub`, already there | The shared stream vocabulary, unchanged. |
| `crate::decider::Event` | `pub`, already there | The domain event trait. |
| `crate::streams::sqlite::{lock_timeout_ms, install_busy_timeout, is_lock_timeout}` | `pub(crate)`, module-private | Shared by the store, the batch, the feed, and the migration runner. Not part of the public surface. |
| `crate::streams::sqlite::migrations` | private module, `pub(super) fn apply` | The migration runner is an implementation detail of this backend. |
| `crate::streams::sqlite::batch::{statement_error, insert_error, locked_heads, head_of}` | private to the module | Failure classification and head lookup are the backend's own. |

Nothing in `src/streams/spec.rs`, `src/streams/feed/spec.rs`,
`src/streams/postgres/**`, or `src/streams/in_memory/**` is touched.
The shared suites are the oracle; this card requires them to pass
unchanged.

The one change outside the new module is one attribute in
`src/streams/batch.rs`: `DuplicateIntent::new` carries
`#[cfg_attr(not(feature = "postgres"), expect(dead_code, ...))]`, which
becomes unfulfilled once this backend constructs it under
`--features sqlite` alone. The condition widens to
`not(any(feature = "postgres", feature = "sqlite"))`. Nothing else in
that file changes.

`src/streams.rs` gains the module declaration
`#[cfg(feature = "sqlite")] pub mod sqlite;` and nothing else: the
PostgreSQL backend is reached as `epoch::streams::postgres::...`, and
this backend is reached the same way, so no crate-level re-export is
added.

## The hole inventory

`grep -rn 'todo!(' src/streams/sqlite*` at this commit. Every hole
belongs to a named fill unit; a hole that outlives its unit is a
defect.

| Hole | File | Fill unit |
|---|---|---|
| `decode_records` | `src/streams/sqlite.rs` | 2 - `EventStreams` |
| `load_stream` | `src/streams/sqlite.rs` | 2 - `EventStreams` |
| `load_stream_from` | `src/streams/sqlite.rs` | 2 - `EventStreams` |
| `load_category` | `src/streams/sqlite.rs` | 2 - `EventStreams` |
| `append` | `src/streams/sqlite.rs` | 2 - `EventStreams` |
| `batch write` | `src/streams/sqlite/batch.rs` | 3 - `AtomicStreams` |
| `batch statement error` | `src/streams/sqlite/batch.rs` | 3 - `AtomicStreams` |
| `batch insert error` | `src/streams/sqlite/batch.rs` | 3 - `AtomicStreams` |
| `batch heads` | `src/streams/sqlite/batch.rs` | 3 - `AtomicStreams` |
| `batch head` | `src/streams/sqlite/batch.rs` | 3 - `AtomicStreams` |
| `transact` | `src/streams/sqlite/batch.rs` | 3 - `AtomicStreams` |
| `poll` | `src/streams/sqlite/feed.rs` | 4 - `EventFeed` |
| `ack` | `src/streams/sqlite/feed.rs` | 4 - `EventFeed` |
| `migration runner` | `src/streams/sqlite/migrations.rs` | 1 - schema and migration runner |

The `#[expect(unused_variables, ...)]` and
`#[expect(unused_imports, ...)]` markers are the compiler's half of the
same inventory. Each fill unit sweeps its own markers by hand: a filled
body that still ignores an argument keeps its marker silent, so the
compiler will not ask for the sweep. The module-level
`#![allow(dead_code)]` is removed slice by slice as the surface goes
live.

Zero-parameter holes carry no marker: `load_category` takes only
`&self`, so there is no `unused_variables` to expect, and an expectation
that cannot fire is itself a lint error.

## The rules the surface encodes, and where they come from

- **The single-writer funnel is structural.** SQLite serializes writers
  database-wide, and every write path opens with `BEGIN IMMEDIATE`, so
  exactly one event transaction is ever in flight and insert order is
  commit order. ADR 0010's funnel requirement is a platform property
  here, not a convention a writer must honor. The lock is coarser than
  PostgreSQL's single global advisory key - database-wide, never
  weaker - and there is no lock order to get wrong.
- **A deferred transaction never writes.** A deferred transaction that
  reads and then writes fails with `database is locked` when another
  connection committed in between. Every write path opens with
  `BEGIN IMMEDIATE`; the read paths need no transaction, because
  each load is a single statement and a statement is its own snapshot.
- **The lock wait is bounded and classified.** `lock_timeout_ms` floors
  the bound to 1ms, because SQLite reads a zero timeout as "fail
  immediately", and the retryable payload then names the bound that was
  really in effect. `is_lock_timeout` maps `SQLITE_BUSY` and
  `SQLITE_LOCKED` to the retryable `LockTimeout` variants, never to a
  conflict.
- **The busy timeout is installed per operation.** It is a property of
  the connection, and cloned handles share one connection, so the bound
  in force is the one the operation installs inside its own closure,
  never one set once at connect.
- **The batch's checks all precede its writes.** The batch reads every
  addressed head, evaluates every constraint, evaluates every write
  expectation, and only then inserts.
- **A batch carries no handle.** The batch holds a category, a rendered
  key, and its events; it never holds a connection or a store. A
  transact reaches only the database of the handle it was
  handed to. Which handle that is stays the caller's discipline, the
  same way `BatchSource` leaves it on the shared surface: the
  guarantee is that a batch cannot reach a *second* database, not that
  it is branded for the first.
- **The feed's cursor is a plain maximum over `global_sequence`.** The
  funnel makes insert order commit order, so a poll reading
  `global_sequence > cursor` can neither skip a committed event nor
  wait on a hole.

## Residual risks

1. **Cancellation does not roll back a submitted transaction.** The
   tokio-rusqlite actor runs a submitted closure unconditionally and
   only then attempts to deliver the result, so dropping the awaiting
   future cannot stop the work: the transaction commits. Three phases -
   cancelled before enqueue (nothing happens), cancelled after enqueue
   (the transaction commits), and PostgreSQL's contrast (its
   transaction rolls back only while the commit has not begun). This
   comes from the actor, not from the design, and the divergence is
   recorded with a test rather than left as a surprise.
2. **No holder eviction.** PostgreSQL evicts a wedged idle writer with
   `idle_in_transaction_session_timeout`; SQLite has no equivalent. A
   wedged writer holds the database write lock until its closure
   finishes, and every other writer times out against it rather than
   displacing it. The actor's shape bounds this: the lock is held only
   while the closure runs.
3. **A poll is a writer.** The poll records the delivered watermark, so
   it takes the database write lock and contends with event writers.
   PostgreSQL's poll takes only a row lock and never does. Throughput
   against PostgreSQL is reduced; correctness is not.
4. **The busy timeout is connection-scoped.** Two handles sharing one
   connection share one busy handler, so the bound cannot be set once
   at connect and relied on. The discipline above is the mitigation.
5. **The intent-key index has no structured error.** SQLite reports a
   unique-constraint violation without naming the index in a field the
   driver exposes, so the mapping to `DuplicateIntent` must inspect the
   error code and the index identity in the message. The match is
   isolated in one helper, and the index expression
   (`json_extract(event_metadata, '$.intent')`) must stay deterministic
   or SQLite refuses the index outright.
6. **The grouped head read is bounded by SQLite's variable limit.** The
   `VALUES` list is chunked and the chunk results merged in Rust,
   inside the one transaction. A capacity note, not a correctness one.
7. **Stored bytes differ from PostgreSQL.** PostgreSQL's `JSONB`
   normalizes; SQLite's `TEXT` stores serde_json's exact output. The
   re-serialized event is identical, so the difference is visible only
   to someone diffing the database file.

## The coverage manifest

Filled at Stage 2, alongside the conformance tests: every trait method
and every error variant mapped to the case that covers it, plus the
exclusion rows with their reasons.

## Findings ledger

The design panel's findings land here, numbered, each marked blocking
or minor, each with its disposition (the repair, or the reason it was
rejected), and the author and reviewer models per row.

Author of the artifact under review: the board-owner session, fireworks-ai
`minimax-m3p1-flash`.

**Seat 1 - `frontier-reviewer` (amazon-bedrock `us.moonshotai.kimi-k3`),
the invalid-states seat.** Verdict: **PASS**, 1 minor, 0 blocking.

| # | Finding | Disposition | Repair or reason |
|---|---|---|---|
| 1 | MINOR - `src/streams/sqlite/feed.rs` - `Progress` carries a cross-field invariant the type does not defend, with no validating constructor on the surface yet. The struct needs `delivered_to >= cursor`, the same rule the cursor table's `CHECK` enforces. | Accepted | Repaired in the skeleton, ahead of the fill unit that would have carried it. `Progress::from_row` now exists and mirrors `src/streams/postgres/feed.rs:116`. Its non-negativity casts are storage-corruption assertions. |

The seat recorded two observations that carry no repair: the
`DuplicateIntent::new` `cfg_attr` widening lands with the fill unit
that first constructs the type (the widening would leave the
expectation unfulfilled before that point), and the unvalidated
`category: &str` matches every other backend on the shared surface.

**Seat 2 - `rust-reviewer` (amazon-bedrock `us.openai.gpt-6.1-sol`),
the logic-and-seams seat.** Verdict: **FAIL**, 2 blocking, 1 minor.

| # | Finding | Disposition | Repair or reason |
|---|---|---|---|
| 1 | BLOCKING - `src/streams/sqlite.rs` - the decoding signature could not consume production query rows. `decode_records` required an ordinary iterator of owned `rusqlite::Row` values, but rusqlite exposes a fallible lending cursor (`Rows::next()` yields `Result<Option<&Row>>`) and `Row` has no public constructor. The hole compiled only because no caller existed yet; filling the load methods would have forced a seam change. | Accepted | Repaired in the skeleton. `decode_records` now takes owned `(String, String)` payload and envelope pairs, and the actor closure hands those back. Decoding happens on the caller's side of the actor boundary, which also keeps the event type off the closure's `'static` bound. |
| 2 | BLOCKING - `src/streams/sqlite.rs` - the migration performed lock-taking SQL before installing its operation's timeout, and bypassed timeout classification. `PRAGMA journal_mode=WAL` ran under whatever bound the connection carried, and its failure was converted straight into a backend fault, hiding the retryable outcome. | Accepted | Repaired in the skeleton. The effective bound goes in inside the actor closure, ahead of every statement that can contend for the write lock - the WAL pragma among them. The pragma's failure now routes through `classify_lock_failure`, so a busy expiry surfaces as the retryable `LockTimeout`. |
| 3 | MINOR - `src/streams/sqlite/DESIGN.md` - the record claimed the types forbid a batch from reaching a database other than the one it was built against. Every database returns the same unbranded `Batch<SqliteEncodedEvent>`, so any `SqliteDatabase` accepts any batch; `BatchSource` leaves handle identity to caller discipline. | Accepted | Repaired in this record. The map now states the guarantee that holds - a batch carries no connection or store, so a transact reaches only the receiving handle's database - and the rules section says plainly that choosing the receiving database stays the caller's discipline. No branding was added. |

**Checks the reviewers could not run.** Seat 2 ran `cargo check`,
`cargo fmt --check`, and the SQLite-only all-targets check, all passing;
its git-diff inspection and its Vale run were denied by its sandbox, so
the diff's scope and the prose lint are unverified by it and were run by
the board owner instead. Seat 1's shell was permission-gated, so its
skeleton gate claims are unverified by it; the board owner ran the
clippy matrix against the Stage 0 baselines.

**Re-review of the repair commit.** Both seats ran a fresh review over
the repair commit, against their own findings. Seat 1 (`frontier-
reviewer`, moonshot family): **PASS**, 0 findings - the `Progress::
from_row` repair verified as a faithful mirror of the PostgreSQL
constructor, signature and body alike. Seat 2 (`rust-reviewer`, openai
family): **PASS**, 0 findings - all three repairs verified in place, and
no repair-induced defect or unfillable seam found.

**Gate S after the repairs.** `cargo fmt --check` clean. The clippy
matrix against the Stage 0 baselines: default features 0 (baseline 0),
`--no-default-features` 15 (baseline 15), `--no-default-features
--features sqlite` 8 (baseline 9 - a reduction, because the module
now references the shared `expectation_satisfied`), `--all-features` 0
(baseline 0). No configuration gained a diagnostic.

