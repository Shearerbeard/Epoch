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
| `SqliteDatabase` | The database handle: the root an application holds, whose `category`, `feed`, and `migrate` hand out per-category stores, feeds, and the schema over one connection, and the only value whose scope covers a cross-category batch (ADR 0006's ownership seam). | A batch that carries a connection or a store of its own, so a transact could reach a database other than the handle's; a handle whose children do not share its connection or its lock bound. |
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
| `crate::streams::sqlite::batch::{statement_error, locked_heads, head_of}` | private to the module | Failure classification and head lookup are the backend's own. |
| `crate::streams::sqlite::batch::insert_error` | `pub(super)`, module-internal | The intent-index classification, driven directly by the conformance tests with the synthetic error pair real SQLite cannot produce. Not public API. |

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

Every trait method and every error variant on this backend, mapped to
the case that covers it. The exclusion rows carry their reason.

The shared suites are called unchanged; the SQLite-local cases cover
what the shared suites do not reach.

### `EventStreams`

| Member | Covered by |
|---|---|
| `load_stream` | the shared OCC races and the shared envelope case; local `a_missing_stream_loads_missing_and_slices_empty` |
| `load_stream_from` | the shared envelope case (with `None`); local `a_slice_from_a_cursor_is_inclusive`, `a_slice_past_the_tail_is_empty_at_the_head`, `a_missing_stream_loads_missing_and_slices_empty` |
| `load_category` | local `a_category_read_returns_the_typed_id` |
| `append` | the shared OCC races, the shared flash sale, the shared envelope case; local `independent_actors_conflict_deterministically`, `a_contender_waits_for_the_write_lock_and_sees_the_committed_head`, `a_cancelled_append_still_commits`, `a_wedged_writer_is_a_retryable_append_timeout`, `a_zero_bound_times_out_reporting_the_floor`, `each_handle_times_out_on_its_own_bound`, `a_child_of_the_root_handle_times_out_on_the_root_bound`, `the_envelope_written_is_the_envelope_stored` |

### `AtomicStreams`

| Member | Covered by |
|---|---|
| `transact` | local `overlapping_batches_leave_exactly_one_winner`, `a_violated_constraint_aborts_the_whole_batch`, `a_batch_and_an_append_contend_on_one_stream`, `a_batch_and_an_append_race_for_one_stream`, `a_satisfied_batch_commits_every_write`, `a_keyed_batch_write_stores_its_envelope`, `a_duplicate_intent_is_rejected_by_the_index`, `a_check_violation_is_a_backend_fault_not_a_duplicate_intent`, `an_unavailable_lock_is_a_retryable_timeout`, `a_zero_funnel_bound_times_out_reporting_the_floor` |

### `EventFeed`

| Member | Covered by |
|---|---|
| `poll` | the shared redelivery case (`poll_redelivers_until_acked`), the shared paging case (`poll_pages_the_backlog`), the shared envelope case (`delivery_carries_the_envelope`); local `a_wedged_writer_times_out_a_feed_poll`, `a_root_handle_hands_out_a_store_and_a_feed_over_one_connection`, `a_child_of_the_root_handle_times_out_on_the_root_bound` |
| `ack` | the shared redelivery case (`poll_redelivers_until_acked`), the shared paging case (`poll_pages_the_backlog`), the shared monotonicity case (`ack_is_monotonic`), the shared undelivered case (`ack_rejects_undelivered`); local `a_root_handle_hands_out_a_store_and_a_feed_over_one_connection` |

### The backend's own error variants

| Variant | Covered by |
|---|---|
| `SqliteStreamsError::Connection` | local `an_unmigrated_database_is_a_driver_fault` |
| `SqliteStreamsError::Handle` | local `a_closed_connection_is_a_handle_failure` |
| `SqliteStreamsError::Serialization` | local `a_stored_payload_that_does_not_decode_is_a_serialization_error` |
| `SqliteStreamsError::LockTimeout` | local `a_wedged_writer_times_out_a_feed_poll` - the feed's poll returns the backend's own error type, not the append wrapper |
| `SqliteWriteError::Duplicate` | local `a_second_write_for_one_stream_is_a_build_error` |
| `SqliteWriteError::Encoding` | local `an_event_that_will_not_encode_is_a_build_error` |

### The shared error variants this backend produces

| Variant | Covered by |
|---|---|
| `AppendError::Conflict` | the shared OCC races; local `independent_actors_conflict_deterministically`, `a_contender_waits_for_the_write_lock_and_sees_the_committed_head`, `a_batch_and_an_append_race_for_one_stream` |
| `AppendError::LockTimeout` | local `a_wedged_writer_is_a_retryable_append_timeout`, `a_zero_bound_times_out_reporting_the_floor`, `each_handle_times_out_on_its_own_bound` |
| `TransactError::Conflict` | local `overlapping_batches_leave_exactly_one_winner`, `a_batch_and_an_append_contend_on_one_stream`, `a_batch_and_an_append_race_for_one_stream` |
| `TransactError::ConstraintViolated` | local `a_violated_constraint_aborts_the_whole_batch` |
| `TransactError::DuplicateIntent` | local `a_duplicate_intent_is_rejected_by_the_index`, `only_a_unique_violation_maps_to_a_duplicate_intent` |
| `TransactError::Backend` | local `a_check_violation_is_a_backend_fault_not_a_duplicate_intent`, `only_a_unique_violation_maps_to_a_duplicate_intent` |
| `TransactError::LockTimeout` | local `an_unavailable_lock_is_a_retryable_timeout`, `a_zero_funnel_bound_times_out_reporting_the_floor` |
| `LoadError::InvalidKey` | local `a_stored_key_that_does_not_parse_is_an_invalid_key` |
| `AckError::Regression` | the shared monotonicity case (`ack_is_monotonic`) |
| `AckError::NotDelivered` | the shared undelivered case (`ack_rejects_undelivered`) |

### Exclusions

| Excluded | Why |
|---|---|
| `a_batch_cannot_contradict_a_category_event_type` (in-memory suite) | The case enforces the in-memory backend's Rust `TypeId` category registry (`src/streams/in_memory/batch.rs:129`, `src/streams/in_memory.rs:182`). Neither the PostgreSQL backend nor this one carries such a registry. A stored row holds an event-type string, not a type identity. |
| `two_writes_cannot_disagree_over_an_unclaimed_category` (in-memory suite) | The same registry, reached from the other side. |
| `AppendError::Backend`, `AckError::Backend` | These are the wrappers the backend's own error takes. The variants they wrap are covered individually above, and a wrapper's identity is the shared type's business, not this backend's. (`TransactError::Backend` was in this row until the fix round; the CHECK-violation and classifier cases now assert it directly, so it moved up.) |
| The PostgreSQL funnel's advisory-lock proof (`a_funnel_holder_blocks_a_rival_append_until_release`) | The proof reads PostgreSQL's `pg_locks`, which SQLite has no equivalent of. This backend's equivalent is `a_contender_waits_for_the_write_lock_and_sees_the_committed_head`. A holder owns the database write lock and commits a row for the contested stream, and the rival is started while the holder owns it. The rival's own statements are traced, and its head read appears after its lock-taking statement, so the head was read under the write lock. The rival then observes the committed head and conflicts. |

### The scope of the identity claim

A green suite proves that the events this backend stored are the events
the shared contract says it should have stored. It does not prove that
a consumer's behavior downstream of the store is unchanged, and it does
not compare the stored bytes with another backend's: the raw bytes
differ by design (divergence D3).

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

**Stage 2 Gate A - `rust-reviewer` (amazon-bedrock `us.openai.gpt-6.1-sol`),
over the conformance set.** Verdict: **FAIL**, 7 blocking, 0 minor.
Every finding was verified against the test file before disposition.

| # | Finding | Disposition | Repair or reason |
|---|---|---|---|
| 1 | BLOCKING - the slice cases asserted the cursor as the slice's position. The contract reports the stream's observed position, so the cases rejected a correct implementation and accepted one that echoed the cursor. | Accepted | Repaired. Both slice cases now assert the stream's head, and the inclusive case uses a payload-carrying event so the slice's contents are checked to be exactly the suffix from the cursor. |
| 2 | BLOCKING - the serialization-error fixture wrote one stream and read another, because each key was generated separately. | Accepted | Repaired. The key is bound once and used for both the write and the read. |
| 3 | BLOCKING - the cancellation case was not synchronized with submission: one pending poll does not establish that the actor received the work, and a fast actor could complete it first. | Accepted | Repaired. The actor is held behind a blocker closure, so the append's submission is observable and its pending state is deterministic. The blocker is released, the queued append runs, and the load - queued behind it - observes the committed event. |
| 4 | BLOCKING - the race fixtures did not deterministically prove that the write lock is taken before the head is observed; a task-start barrier is not a database-operation barrier. | Accepted | Repaired with a new case, `a_contender_waits_for_the_write_lock_and_sees_the_committed_head`. A holder owns the write lock and commits a row for the contested stream; the rival is observed still waiting while the holder owns the lock; once the holder commits, the rival observes the committed head and conflicts. The coverage manifest's exclusion row now points at this case instead of claiming equivalence with the PostgreSQL proof. |
| 5 | BLOCKING - the per-handle bound case checked the reported payload only, so an implementation that always installed one fixed bound would pass it. | Accepted | Repaired. The case now reads the connection's installed `busy_timeout` back after each operation, so it verifies the effective bound each handle installed, not only the duration each handle reported. |
| 6 | BLOCKING - the manifest's error coverage and exclusion reasons were inaccurate: the append cases produce the wrapper variant rather than the backend's own, and the shared batch suite does not exercise this backend's typed write or its error mapping. | Accepted | Repaired. A feed poll against a wedged writer now covers the backend's own `LockTimeout`, and two new cases cover the builder's `Duplicate` and `Encoding` errors. The manifest rows now name the assertions that exist. |
| 7 | BLOCKING - the duplicate-intent case did not prove the whole-batch rollback: the rejecting write came first, so a backend that stored writes one at a time would leave the same state. | Accepted | Repaired. The successful write now comes first and the rejected one second, and the case asserts that the first write was rolled back. It also asserts the rejected stream's full identity, not only its category. |

**Stage 2 Gate A, re-review of the repair commit.** `rust-reviewer`
(amazon-bedrock `us.openai.gpt-6.1-sol`): **FAIL**, 1 blocking, 1 minor.
Six of the seven findings were verified repaired; the lock-order case
was not.

| # | Finding | Disposition | Repair or reason |
|---|---|---|---|
| 1 | BLOCKING - the replacement lock-order proof still lacked synchronization. Polling the holder's future once queued its closure but did not prove it had taken the lock, and a rival that had not finished after 200ms might not have reached its lock attempt at all. | Accepted | Repaired. The holder now acknowledges the lock it holds. The order is checked at the statement level through rusqlite's trace API. The rival's head read must appear after the holder's commit. A backend that read the head before taking the lock shows that read first. The trace needs rusqlite's `trace` feature, which `tokio-rusqlite` now enables. |
| 2 | MINOR - the new feed-timeout case carried a doc comment about a zero bound it did not test. | Accepted | Repaired. The comment describes the assertion the case makes. |

**Stage 2 Gate A, second fix-commit re-review.** `rust-reviewer`
(amazon-bedrock `us.openai.gpt-6.1-sol`): **FAIL**, 1 blocking, 1
minor.

| # | Finding | Disposition | Repair or reason |
|---|---|---|---|
| 1 | BLOCKING - the lock-order proof still passed accidentally. The holder's acknowledgement is in place, but nothing observes the rival's database operation before the holder is released. The assertion compares the head read against the holder's commit rather than against the rival's own lock-taking statement, and temporal order alone cannot establish lock ownership. | Accepted | Repaired. Each traced connection now feeds its own sink, so the order is read per connection. The test waits until the rival's own trace shows its lock-taking statement before it releases the holder. The assertion is that the rival's head read comes after the rival's lock-taking statement. One connection runs one statement at a time, so that order means the head was read under the lock. The committed-head conflict assertion stays. |
| 2 | MINOR - the feed-timeout case still carried the zero-bound comment, and the ledger claimed it was repaired when it was not. | Accepted | Repaired. The comment now describes the assertion the case makes, and the ledger records it. |

**Stage 2 Gate A, third fix-commit re-review.** `rust-reviewer`
(amazon-bedrock `us.openai.gpt-6.1-sol`): **FAIL**, 1 blocking, 0
minor.

| # | Finding | Disposition | Repair or reason |
|---|---|---|---|
| 1 | BLOCKING - the acknowledgement that waits for the rival's lock-taking statement consumed the statement it matched and threw the vector away, so the ordering assertion saw only the later statements. A correct backend would have panicked on the missing lock statement, and a head read that preceded the lock would have been discarded rather than shown. | Accepted | Repaired. The vector the wait returns is kept and extended with the statements drained afterwards, so the assertion reads the rival's whole statement sequence. A head read before the lock attempt now shows up first, and the case fails. |

**Stage 2 Gate A, fourth fix-commit re-review.** `rust-reviewer`
(amazon-bedrock `us.openai.gpt-6.1-sol`): **PASS**, 0 findings. The
ordering assertion was verified as sound for the regression it targets:
a head read before the lock attempt stays in the retained prefix and
fails the comparison, and the separate connection sinks keep the
holder's statements from satisfying it by accident.

**Gate S after the repairs.** `cargo fmt --check` clean. The clippy
matrix against the Stage 0 baselines: default features 0 (baseline 0),
`--no-default-features` 15 (baseline 15), `--no-default-features
--features sqlite` 8 (baseline 9 - a reduction, because the module
now references the shared `expectation_satisfied`), `--all-features` 0
(baseline 0). No configuration gained a diagnostic.


**Fills Gate A.** `rust-reviewer` (amazon-bedrock
`us.openai.gpt-6.1-sol`) over the fill range: **FAIL**, 1 blocking.

| # | Finding | Disposition | Repair or reason |
|---|---|---|---|
| 1 | BLOCKING - the feed's `poll` updated the delivered watermark and committed before it decoded the payloads and envelopes. A payload the event type cannot decode left the group recorded as having received the whole page, so an ack inside that page could succeed instead of being rejected as undelivered, and unread events could be skipped. | Accepted | Repaired. The rows decode inside the transaction that holds them, before the watermark moves - what the PostgreSQL backend does. The feed's event type gained the `'static` bound the actor needs for decoded events to leave the closure. The regression case writes an undecodable payload, asserts the poll fails, the watermark never moved, and the ack is rejected. |

**Fills Gate A, fix-commit re-review.** `rust-reviewer`: **PASS**, 0
findings.

**Final feed review (frontier, pre-PR).** `frontier-reviewer`:
**PASS**, 0 blocking, 4 minor.

| # | Finding | Disposition | Repair or reason |
|---|---|---|---|
| 1 | MINOR - `SqliteEventFeed::new` takes a raw `Connection` and neither the store nor the batch handle exposes its own, so a consumer keeps and clones the connection themselves. Parity with PostgreSQL holds, but an in-memory store cannot hand its connection to a feed at all. | Deferred | Follow-up. The reviewer's repair is a `feed()` method on the store, mirroring `InMemoryDatabase::feed`. It is new public API after the final review, so it waits for the maintainer's call in the PR rather than landing unreviewed. |
| 2 | MINOR - no cancelled-poll or cancelled-ack case. A cancelled ack still commits, and the retried ack lands as the silent no-op at the cursor. Entailed by the actor property the append-cancellation case pins, but not itself pinned. | Deferred | Follow-up card. The behavior is documented in D2 and D2a; the missing case is coverage, not a defect. |
| 3 | MINOR - `ack`'s `UPDATE` row count is unasserted, where PostgreSQL asserts one row. The write lock makes a missing row unreachable, and the design note records the deviation. | Accepted, no change | The argument was walked: no row means `delivered_to` is zero, so every nonzero position is rejected before the update. Optional symmetry only. |
| 4 | MINOR - the `expect` calls on stored values are a panic-on-corrupt-row posture. | Accepted, no change | Identical to the PostgreSQL feed and recorded in the module docs as storage-corruption assertions. Corrupt-file resilience would be a cross-backend decision, not a SQLite-local one. |

**Gate S at the PR.** `cargo fmt --check` clean. Clippy: default 0,
`--no-default-features` 15, `--no-default-features --features sqlite` 5
(baseline 9), `--all-features` 0. The test suite is green under
`--features sqlite`, `--features in_memory,sqlite`, `--all-features`,
and `--doc`.

**Maintainer review round (Gate U), and the PR's open decision made.**
Reviewer: the maintainer's session (opencode-go glm-5.3), over the
whole PR range. Verdict: PASS to continue - five findings, none a
correctness defect in the shipped paths, every one dispositioned
below. The one decision the PR body left open is decided: the
feed-constructor seam closes **in this card**, as a root-handle
pattern, because the maintainer's consumer shape (one handle at a
web-server root, sqlite default with postgres for HA) needs exactly
that.

| # | Finding | Disposition | Repair or reason |
|---|---|---|---|
| 1 | `SqliteEventFeed::new` takes a raw `Connection` and no type exposes its own, so a feed cannot be built over a store's private in-memory connection; the conformance tests reached a private field to do it. | Accepted | Repaired in this round. `SqliteDatabase` gains `category`, `feed`, and `migrate` - the root handle mirroring `InMemoryDatabase`'s verbs - sharing one connection and propagating its lock bound. The maintainer decided it lands here, not on a follow-up card. |
| 2 | A poll is a writer on SQLite; the test drive opens a second connection for the consumer without ever saying why that matters. | Accepted | Repaired. The example's header carries a sixth bullet naming the write lock and the contention, and a final section demonstrates the root-handle shape over one in-memory connection. |
| 3 | The `load_stream_from` Rust-side `>= cursor` re-check was unexplained next to the SQL's own `>= ?3` filter. | Accepted, kept | The re-check is decisive where the SQL bind clamps a cursor beyond i64::MAX (a clamped bind would match a stored sequence of exactly i64::MAX). The comment now says so; removal would change that edge. |
| 4 | `insert_error` matched the intent index by message substring only, gated by the primary code that a CHECK violation shares. | Accepted | Repaired. The extended code `SQLITE_CONSTRAINT_UNIQUE` now gates the message test; a CHECK-violation case pins the fall-through; a classifier test drives the synthetic pair real SQLite cannot produce. |
| 5 | `applied_versions` classified its failures through the lock classifier with a hardcoded 5s bound that was never in effect. | Accepted | Repaired. The read runs under the write lock the migration transaction already holds, so it now classifies as a backend fault - the same classification the batch's locked head read gives its own reads. |

The fix range's authors: `rust-write` (fireworks `kimi-k3`) for
`batch.rs`, `tests.rs`, and `examples/test_drive.rs`, one file per
dispatch per the sizing rule; the board owner (opencode-go glm-5.3)
for `sqlite.rs`, `migrations.rs`, `CHANGELOG.md`, `README.md`, and
this record.

**Gate A on the fix range.** `rust-reviewer` (amazon-bedrock
`us.openai.gpt-6.1-sol`, a family different from every author above):
**PASS**, 0 blocking, 1 minor - the CHECK-violation case could not
discriminate the extended-code gate, because no real CHECK
constraint's message names the intent index, so the pre-hardening
message-only match already rejects that path. Accepted and repaired:
`insert_error` is `pub(super)` and
`only_a_unique_violation_maps_to_a_duplicate_intent` drives the
classifier with the synthetic pair. Re-review of the repair:
**PASS**, 0 findings, one accuracy note on the test's comment (the
two errors' messages differ as well; both satisfy the same index-name
predicate, so the extended code remains the only difference the
classifier can key on) - taken into the comment.

**Gate S over the fix range, measured on clippy 1.96.0.** fmt clean;
check clean under no-default, sqlite, and all-features, all targets;
tests green under `--features sqlite` (112), `--features
in_memory,sqlite` (112), `--all-features` (142), and `--doc`. The
clippy matrix was measured same-machine before and after (stash,
re-lint, restore, re-lint): default 1/1, no-default 23/23,
sqlite-only 7/7, all-features 3/3 - zero new diagnostics in every
configuration. The card's recorded baselines (0/15/5/0) are stale
under clippy 1.96.0: the pre-fix tree itself measures 1/23/7/3 on
this machine, the two extra sqlite-only diagnostics living in
`src/test_helpers/`. The operative rule - no increase against the
configuration's own baseline, measured like for like - passes.
