# SQLite Event Repository — Design Record

Internal planning artifact for the `s4/sqlite-repository` card stack.
Excluded from the published package (`/docs/internal/` in Cargo.toml
`exclude`). Port feasibility analysis and the vetted card plan live in
the session that produced this record; this file is the module's
standing design reference.

## Type-to-business-rule map

| Type | One business rule | Invalid state it forbids |
|---|---|---|
| `SqliteEventRepository<E>` | Events of exactly one stream type persist in one SQLite `events` table, addressed as `{stream_type}-{entity_id}` | One repository instance mixing stream types |
| `SqliteVersion` | A stream's version is its last per-stream sequence number; append produces only sequence >= 1 (schema `CHECK` enforces it at rest). Unchecked newtype mirroring `PgVersion` | Mixing a stream version with a bare `i64`, a global rowid, or another backend's version |
| `SqliteRepositoryError` | Driver failure (`Handle`, `Connection`) and payload failure (`Serialization`) are distinct failure modes | Collapsing a driver error into a version conflict |

## Error model

- `rusqlite::Error` (including SQLITE_BUSY after the busy timeout)
  surfaces as `RepoErr`. `VersionConflict` is produced only by the
  version comparison inside `append`, never by the driver; the type
  seam enforces this (`SqliteRepositoryError` has no conflict
  variant).
- `tokio_rusqlite::Error` envelopes every `call` result
  (`Error<E>` wraps the unit of work's error; `ConnectionClosed` and
  `Close` are actor-lifecycle failures). Two flatten functions resolve
  the panel's deferred overlap at the seam: `actor_error` maps inner
  and close failures to `Connection` and lifecycle failures to
  `Handle`; `actor_outcome` passes append's unit-of-work failures
  (including VersionConflict) through untouched. Underlying SQLite
  failures always reach `Connection`, never `Handle`.
- Panel finding dispositions live in the review ledger below.

## Concurrency model

- One `tokio_rusqlite::Connection` actor per repository; clones share
  it (cheap `Clone`, `Send + Sync`, satisfying the OCC spec bounds).
  All in-process database access serializes through the actor's
  background thread.
- `append` runs inside `BEGIN IMMEDIATE`
  (`transaction_with_behavior(TransactionBehavior::Immediate)`), making
  the read-`MAX(sequence)`-check-insert sequence atomic. This is the
  SQLite substitute for the postgres backend's
  `pg_advisory_xact_lock(hashtext($1))`; it is coarser (database-wide,
  not per-stream) but strictly stronger for correctness.
- Optimistic-concurrency contentions surface as `VersionConflict`;
  `LoadDecideAppend` retries (default 20x) absorb them under the OCC
  spec's 9 concurrent commands, the same contract the postgres backend
  runs.

## Schema decisions

- The postgres schema's `id BIGSERIAL` is unused by any query and is
  dropped; `global_sequence INTEGER PRIMARY KEY AUTOINCREMENT` makes
  the rowid the global order. `AUTOINCREMENT` keeps rowids monotonic
  and unreused even if rows are deleted.
- Keying is the pair `(stream_type, stream_key)`, not a concatenated
  `stream_name`. This mirrors the redesign lineage's landed
  `stream_events` table (`src/streams/postgres/migrations/0001` in
  the unmerged streams module): `stream_type` is the repository's
  namespace and `stream_key` is exactly what the typed stream id
  renders (ADR 0004's two-way contract). No concatenation means no
  aliasing: `("a-b", "c")` and `("a", "b-c")` are different rows.
- `event_data` is `TEXT` with `CHECK(json_valid(...))` — JSONB's
  portability substitute; payloads round-trip through `serde_json`
  regardless.
- `sequence` is `CHECK (sequence > 0)` — 1-based positions enforced in
  storage, matching the lineage's `stream_events` and the panel's
  finding 1b disposition.
- `UNIQUE (stream_type, stream_key, sequence)` is the
  optimistic-concurrency backstop, identical in shape to the lineage's
  `stream_events_position`.

## Query-method compatibility (keying validated against every method)

| Query method | Works how on the pair keying |
|---|---|
| Per-stream load | `WHERE stream_type = ?1 AND stream_key = ?2 ORDER BY sequence` — direct pair lookup |
| Category read (`load(None)`) | `WHERE stream_type = ?1 ORDER BY global_sequence` — served by the index |
| Grouped streams / hierarchical addressing (fine-grained ids, read models) | `WHERE stream_type = ?1 AND (stream_key = ?2 OR stream_key LIKE ?2 || '-%')` — unambiguous because the namespace boundary is a column. The concatenated schema cannot express this correctly: it cannot tell where the type prefix ends |
| Event feed / cursor (ADR 0010) | Cursor over `global_sequence`. Under the single-writer actor + IMMEDIATE transactions, rowid allocation and commit are serialized, so the global sequence is commit-ordered by construction — the property BIGSERIAL lacks and ADR 0010's single-writer funnel exists to restore. SQLite satisfies the feed's no-skip cursor semantics natively |
| Batch (ADR 0006 AtomicStreams) | A multi-stream append is one transaction touching N `(stream_type, stream_key)` pairs, each with its own MAX check and UNIQUE backstop. Pair keying addresses each stream in a batch directly |
| Saga / outbox (ADR 0010 outbox section) | Saga streams and outbox rows are ordinary streams: a `SagaId` renders into a `stream_key` in its own category. Same pair primitive; no concatenated-name parsing anywhere |

## Visibility and seam table

| Item | Visibility | Reaches into |
|---|---|---|
| `SqliteEventRepository` | `pub`, gated on feature `sqlite` | `tokio_rusqlite::Connection` actor; `schema.sql` via `include_str!` |
| `SqliteVersion`, `SqliteRepositoryError` | `pub` | serde, thiserror, rusqlite via `tokio_rusqlite::rusqlite` re-export |
| `migrate` | `pub async` | `PRAGMA busy_timeout`, `PRAGMA journal_mode=WAL`, `execute_batch(SCHEMA)` |
| `#[cfg(test)] mod tests` (planned, card 2) | private | `crate::test_helpers` generic spec suites (trait-bound conformance, incl. OCC) plus the module tests named in the hole inventory |

## Degenerate arm

`load_from_version(Exact(v), None)` compares a per-stream sequence
against the global rowid ordering — a cross-ordering comparison the
single `Version` type permits and no strategy exercises
(`LoadDecideAppend` always passes a stream id; `load(None)` passes
`Any`). The fill keeps the postgres comparison verbatim rather than
inventing semantics the trait does not define for this arm.

## Hole inventory

Closed at card 3: `grep -rn 'todo!(' src/repository/sqlite/` returns
nothing; both `#[expect]` markers and the module-level
`#![allow(dead_code)]` are swept. Baseline (skeleton): `migrate`,
`load_from_version`, `append`.

Card 2 owned the test module (failing on arrival against the holes):
- both generic suites by trait bounds (spec, OCC spec)
- `load_missing_stream_reports_no_stream` (mirrors postgres)
- `concurrent_appends_conflict_deterministically` (mirrors postgres)
- `empty_load_on_existing_stream_reports_stream_exists` — pins the
  two-step empty-result arm no generic suite exercises (the position
  must be past the stream head: reads are inclusive)
- `empty_append_returns_current_version` — pins the empty-append
  contract no generic suite exercises
- `append_conflicts_report_expected_and_actual` — pins the
  `VersionDiff` contents of both conflict classes

## Line budget (plan commitment, `wc -l` at cards 3–4)

Planned: schema.sql 23, version.rs 19, error.rs 16, mod.rs ~360 (of
which ~110 tests). Module cap: 430.

Actual at card 3: schema.sql 32, version.rs 18, error.rs 14, mod.rs
487 (331 code + 156 tests); module total 551. The code surface
(schema + version + error + mod code = 395) lands inside the original
feasibility estimate (350-400). The module cap is exceeded by 121,
accounted: panel finding 5's three added tests (+55), the actor
envelope flatten functions the driver's `call` signature forced at
fill time (+33, ledger finding 4), the inclusive-position fix in the
stream-exists test (+12), and rustfmt reflow (~21). Accepted with
this accounting rather than cutting tests or documentation to reach
the number.

## Residual risks

- MSRV is enforced at introduction and on lockfile refresh only, not
  in CI (rusqlite 0.37 declares no `rust-version`; the `cargo +1.88
  check --features sqlite` gate at card 1 is the control, matching the
  uuid-cap precedent).
- OCC-spec wall-time under the actor model is proven by card 3's first
  green run, not by argument.
- `PRAGMA journal_mode=WAL` is a no-op on `:memory:` databases
  (reports back `memory`); harmless for tests, meaningful for
  file-backed use.
- The postgres backend this module mirrored originally keyed streams
  on a concatenated name; card 4 aligned it with this module's pair
  keying. Its `global_sequence` remains BIGSERIAL allocated at insert
  time, so concurrent cross-stream appends can still commit out of
  sequence order. That is a property of the old repository surface;
  the feed's commit-ordered cursor belongs to the streams surface
  (ADR 0010), which card 4 does not attempt to backport. SQLite does
  not share this risk (single-writer actor).
- Card 4 changes the postgres backend's table shape (pair keying).
  `CREATE TABLE IF NOT EXISTS` does not alter an existing table, so
  databases carrying the old `events` shape keep it until recreated;
  the changelog records this as a schema change of a pre-1.0 backend.

## Review ledger

Card 1 design panel (skeleton commit 4ae6dd1). Seat 1 invalid-states:
rust-reviewer (gpt-6.1-sol); seat 2 logic/seams: frontier-reviewer
(kimi-k3); skeleton author GLM-5.3.

| # | Finding (seat) | Disposition |
|---|---|---|
| 1a | `SqliteVersion` serde derives let `Deserialize` bypass the 1-based invariant (1) | Fix: derives removed; no code in the crate serializes any `Version` type (verified by grep), so the derives were speculative surface |
| 1b | `From<i64>` accepts 0/negative; proposed `TryFrom` (1) | Rejected: the failure mode is unreachable through owned paths — sequence positivity is schema-`CHECK`ed at ingress and append arithmetic starts from a schema-positive base. Fix: `CHECK (sequence > 0)` added; rule map reworded |
| 2 | `stream_name` concatenation aliases `(type "a-b", id "c")` with `(type "a", id "b-c")` (1, also seat 2 obs. 6) | Fix: user chose the pair-keying repair (option A). `stream_name` replaced by `(stream_type, stream_key)` columns aligned with the lineage's `stream_events` vocabulary; validated against every query method (matrix above). A follow-up card (card 4) applies the same simplification to the postgres backend |
| 3 | `Config(String)` has no producer (1, 2) | Fix: variant removed; revisit when a config-parsing operation exists |
| 4 | `Handle` justification misstated open's error type; payload overlaps `Connection` (1) | Partially rejected, ledger corrected twice: tokio-rusqlite 0.7.0's `open`/`open_in_memory` return `rusqlite::Error` (verified against the crate source), so open failures route to `Connection` through `?`; `Handle`'s producers are actor-lifecycle failures only, via the envelope flatteners `actor_error`/`actor_outcome` that the fill card added |
| 5 | Coverage claim untracked; `StreamExists` arm, explicit conflict contract, empty-append pinned by no suite (2) | Fix: test module named as card-2 deliverable with the three unpinned semantics as named tests |
| 6 | `None + Exact` category arm conflates orderings (2) | Fix: degenerate arm documented above; fill mirrors postgres comparison |
| 7 | Seam table listed the test module as present (2) | Fix: row marked planned (card 2) |

Card 3 fill gate (rust-reviewer, gpt-6.1-sol): PASS, two minor
findings, both repaired - the ledger's open-signature claim corrected
against the crate source (open returns rusqlite::Error; Handle is
produced only by the envelope flatteners), and the
independent-connections test added because the shared-actor race
pins the outcome, not the BEGIN IMMEDIATE mechanism.

Card 4 (user-directed follow-up): the postgres backend adopts the
same pair keying - stream_type + stream_key columns, no surrogate
id, CHECK (sequence > 0), pair-keyed advisory lock
(pg_advisory_xact_lock(hashtext($1), hashtext($2))). Its four tests
pass on the reshaped schema after a one-time DROP of the previous
table in long-lived databases.

