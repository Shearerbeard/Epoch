# E28 - the SQLite streams backend on the new surface

Status: implementation complete, under review. Card: E28. The board's
plan is `docs/board/plans/2026-11-02-e28-sqlite-streams-backend.md`, and
the module's own design record - the type-to-business-rule map and the
findings ledger - is `src/streams/sqlite/DESIGN.md`.

This note is the card's design note: the single-writer funnel argument,
the EventFeed answer with its reasoning, the divergences from the
PostgreSQL backend, and the decisions the implementation rests on.

## What this backend is

A SQLite backend for Epoch's **new** streams surface: `EventStreams`
(ADRs 0002-0005), `AtomicStreams` (ADR 0006), and `EventFeed` (ADR
0010). It is net-new code against those traits, not an adapter over the old
`SqliteEventRepository`: the trait shapes differ, and so do the version
types (`StreamSequence(NonZeroU64)` against `SqliteVersion(i64)`) and the
error types. The old surface under `src/repository/sqlite/` is read
here as a precedent and is not touched.

It compiles behind the existing `sqlite` feature, which already gated
`tokio-rusqlite` and `serde_json`. The backend adds no dependency. The
one dependency change is `rusqlite`'s `trace` feature, which the tests
use to observe the order two connections ran their statements in; the
crate's own statement-tracing API carries it, and the lock file does
not change.

## The single-writer funnel, and why it is structural here

ADR 0010 requires that every event transaction pass through one
serialized writer, so that insert order is commit order and the feed's
cursor is a plain maximum over the committed log. On PostgreSQL that
writer is a transaction-scoped advisory lock, and the property depends
on every writer honoring it, forever.

On SQLite the funnel is not a convention. SQLite serializes writers
database-wide, and every write path in this backend opens its
transaction with `BEGIN IMMEDIATE` - the write lock is taken before the
transaction's first read and is held until the transaction ends.
Exactly one event transaction is ever in flight, so insert order is
commit order by construction. The lock is coarser than PostgreSQL's
single global advisory key - database-wide rather than key-scoped -
and it is never weaker. There is no lock order to get wrong, no
deadlock cycle to form, and no cross-category parallelism, which ADR
0010's funnel design already gave up.

The rule that follows from the platform is the one that matters most
here - **every write path opens with `BEGIN IMMEDIATE`, never a deferred
`BEGIN`.** A deferred transaction that reads and then writes
fails with `database is locked` when another connection committed in
between. That was measured, not reasoned about:

```
A read (deferred BEGIN)   -> A sees 1 row
B BEGIN IMMEDIATE; INSERT; COMMIT
A INSERT                  -> OperationalError: database is locked
```

The read paths need no transaction at all - each load is a single
statement, and a statement is its own snapshot, so a rival append
committing mid-read cannot split a read across two views.

The busy timeout that bounds a lock wait is a property of the
connection, not of the operation, and cloned handles share one
connection. So the effective bound is installed inside each operation's
own actor closure, immediately before that operation takes the write
lock, and never once at connect. The bound in force is then always the
one the operation intends. A bound installed after the `BEGIN` would
bound nothing, because the `BEGIN` is what takes the lock.

## The EventFeed answer: yes, and why the cursor is honest

**The answer is yes. This backend implements `EventFeed` with an honest
cursor.**

The reasoning, in the order it has to hold:

1. The feed's cursor is the highest committed `global_sequence` a group
   has acknowledged, and it is honest only if insert order is commit
   order (ADR 0010).
2. SQLite serializes writers database-wide, and `BEGIN IMMEDIATE` takes
   the write lock before the transaction's first read. So exactly one
   event transaction is in flight, and it holds the lock until it
   commits. The funnel ADR 0010 requires is structural.
3. The row id is allocated at insert time, inside the write
   transaction, so allocation order is commit order. `AUTOINCREMENT` is not required for this. Measured: a plain `INTEGER PRIMARY KEY`
   (a rowid alias) and `INTEGER PRIMARY KEY AUTOINCREMENT` behave
   identically on a rolled-back id, because `AUTOINCREMENT`'s
   high-water mark reverts with the transaction. The keyword only
   prevents reusing an id that was committed and later deleted, which
   cannot happen in an append-only store. It is kept here only for
   symmetry with the old surface's schema, so a reader comparing the
   two schemas does not have to re-derive that the difference is
   immaterial. The old schema's own comment, "AUTOINCREMENT keeps ids
   monotonic and unreused", is inaccurate about the rollback case, and
   this note does not repeat it.
4. The one property PostgreSQL gets from a non-transactional sequence
   and SQLite does not: **SQLite reuses a rolled-back id.** That is
   safe here, and it is the opposite of PostgreSQL's behaviour. After
   the rollback the high-water mark reverts to the highest *committed*
   id, so the next allocation is always strictly greater than every
   committed id. No committed id is ever reused, and allocation order
   still equals commit order.
5. The visible difference from PostgreSQL is that this backend's own
   write paths produce **no holes**. PostgreSQL burns a sequence value
   on a rollback, and ADR 0010 says the cursor passes those burned
   values without waiting. SQLite has none to pass. The qualifier
   matters: a statement that succeeds and is *ignored* can still
   consume a value. This backend never issues `INSERT OR IGNORE` - a
   conflicting insert fails the statement and the whole transaction
   rolls back, releasing the id - so no hole arises from the paths it
   actually takes.
6. The write lock is database-wide where PostgreSQL's is a single
   global advisory key, which is coarser and never weaker.

So a poll reading `WHERE global_sequence > cursor` can neither skip a
committed event nor wait on a value that will never appear, and the
cursor is an honest plain maximum.

### The feed's divergences from PostgreSQL

- **A poll is a writer.** The poll records the delivered watermark, so
  it takes the database write lock and contends with event writers.
  PostgreSQL's poll takes only a row lock and never does. Correctness
  is preserved - the watermark update and the entries read are one
  atomic transaction either way - but throughput against PostgreSQL is
  reduced. This is the divergence most likely to surprise a consumer
  moving between backends.
- **The cursor row's existence rests on the write lock.** PostgreSQL's
  `ack` relies on `FOR UPDATE` to lock the cursor row, so its
  `assert_eq!(advanced, 1)` after a plain `UPDATE` holds. SQLite has no
  `FOR UPDATE`: the substitute is the database-wide write lock from
  `BEGIN IMMEDIATE`, which is stronger but changes the reason the
  assertion holds. Under that lock the row cannot be created or removed
  between the read and the update, so the guarantee comes from the lock
  being held across both. The one-poller-per-group backstop is the
  write lock as well, not a row lock.
- **No holder eviction.** PostgreSQL evicts a wedged idle writer with
  `idle_in_transaction_session_timeout`, so a wedged writer's lock is
  released and the next writer proceeds. SQLite has no equivalent:
  `busy_timeout` bounds a waiter, it does not evict a holder. A wedged
  writer holds the database write lock until its closure finishes, and
  every other writer times out against it rather than displacing it.
  The actor's shape bounds this in practice: the lock is held only
  while the closure runs, and a caller that is cancelled does not leave
  the lock held any longer than the closure takes to finish. There is
  no server-side bound on a pathological closure.

## The divergences from the PostgreSQL backend

| # | Divergence | Why it is acceptable |
|---|---|---|
| D1 | A rolled-back `AUTOINCREMENT` id is reused; PostgreSQL burns a sequence value. | The reuse is always above the highest committed id, so commit order is preserved. Measured rather than assumed. |
| D2 | **Cancellation does not roll back a submitted transaction.** The tokio-rusqlite actor runs a submitted closure unconditionally and only then attempts to deliver the result, so dropping the awaiting future cannot stop the work. The transaction commits. | This is the sharpest divergence, and it comes from the actor rather than
from the design. The spec suites do not exercise cancellation, so a
SQLite-local test pins the actual behavior and makes it a documented fact. |
| D3 | Stored bytes differ. PostgreSQL's `JSONB` normalizes; SQLite's `TEXT` stores serde_json's exact output. | The stored value is re-serialized on read, so the observable event is identical. The difference is visible only to someone diffing the database file. |
| D4 | *A parity note, not a divergence.* `load_category` drops the envelope, because `CategoryEvent` has only `id` and `event` and no field for one. | The PostgreSQL backend reads only `event_data` too. This backend does the same, or E29's cross-backend diffing breaks. Do not "fix" it here. |
| D5 | The intent-key uniqueness index is implemented on SQLite. | Decided by the maintainer on 2026-11-02 and recorded on the card. The cost is that SQLite exposes no structured constraint name, so the mapping to `DuplicateIntent` must inspect the error code and the index identity in the message. That match is isolated in one helper with a comment naming the fragility. |
| D6 | The grouped head read is bounded by SQLite's variable limit. | The `VALUES` list is chunked and the chunk results are merged in Rust, inside the one transaction. A capacity note, not a correctness one. |
| D7 | No holder eviction. | An operational difference, not a correctness one. See the feed's divergences above. |
| D8 | A feed poll takes the database write lock. | An operational difference, not a correctness one. See the feed's divergences above. |
| D9 | The busy timeout is connection-scoped, not operation-scoped. | Mitigated by discipline rather than by the platform: each operation installs its effective bound inside its own actor closure, before it takes the lock. The bound must not be set once at connect. |
| D10 | **Corrected from the plan.** The plan pinned an events-outer slice query with the head in an uncorrelated scalar subquery, because it avoids the temporary B-tree that PostgreSQL's join shape costs. That shape was measured and it is wrong - it returns **no row** when no
event falls in the cursor's range, so it cannot report the stream's head
at all. The contract is that a cursor past the tail still observes the head
and a stream that does not exist observes none, and only the join shape
reports both from one snapshot. The join shape is used, and the temporary B-tree is the price. | A correctness requirement outranks a sort. The measurement that rejected the shape is in the plan's SQL evidence, and the case that would have caught it is in the conformance set. |
| D11 | `VARCHAR(255)` becomes `TEXT` with an explicit length check. | The check counts characters, as `VARCHAR(255)` does, so the two backends accept and reject the same keys. |

### D2a - the cancellation phases, stated precisely

"A cancelled append still commits" is unqualified and wrong. Three
phases:

1. **Cancelled before enqueue.** The future is dropped before the
   closure reaches the actor's channel. Nothing is submitted, no
   transaction is opened, nothing commits.
2. **Cancelled after enqueue.** The closure is already in the actor's
   queue. The actor runs it to completion - the transaction opens, runs,
   and commits - and only then attempts to send the result into a
   dropped oneshot, which fails harmlessly. The append commits.
3. **PostgreSQL's contrast is also phase-dependent.** `tokio-postgres`
   rolls a transaction back on drop only while `done` is false, and
   `commit()` sets `done = true` before awaiting the `COMMIT`
   statement. A PostgreSQL transaction cancelled before the commit
   begins rolls back, and one cancelled after it begins does not.

The SQLite-local cancellation test is written against phase 2
explicitly: the work is enqueued, then the awaiting future is dropped,
and the transaction is asserted to have committed. Cancelling before
the enqueue would prove nothing.

## The decisions the implementation rests on

### The intent-key uniqueness index (D5)

ADR 0010 scopes intent-key uniqueness to PostgreSQL in v1. The shared
batch vocabulary holds that a backend whose store does not enforce the
key never produces `DuplicateIntent`. The maintainer decided on
2026-11-02 that SQLite implements the index anyway. The card records
that decision.

It is one partial unique index over
`json_extract(event_metadata, '$.intent')` on the `saga-outbox`
category, exactly the shape PostgreSQL uses. The mechanism it protects
is concrete: the saga runner's command arm carries
`ExpectedVersion::Any`, so on a replay only the uniqueness index can
abort the batch.

Two properties of the index matter. The indexed expression must be
deterministic, or SQLite refuses the index outright, so a later migration
that renames the `intent` key needs its own step. And SQLite
reports a unique violation without naming the index in a field the
driver exposes, so the mapping to `DuplicateIntent` checks the
constraint code first and the index identity in the message second,
inside one helper.

### The migration story

Numbered steps and a ledger, mirroring `src/streams/postgres/`, rather
than the old surface's `schema.sql` blob and its `CREATE TABLE IF NOT
EXISTS`. The card asks for the story E12 taught, a ledger gives the
backend a forward path when the schema changes, and it keeps the two
streams backends symmetrical. The cost is roughly 150 lines of
migration machinery against roughly 40 for the blob.

The whole run - ledger read, pending steps, ledger writes - is one
transaction under the database write lock, so concurrent callers
serialize and the later ones find nothing pending. DDL in SQLite is
transactional, so a failing step rolls everything back, the ledger
table included. Opening a store does not create the schema; the
explicit `migrate()` call does.

### The schema's type substitutions

| PostgreSQL | SQLite | Why |
|---|---|---|
| `BIGSERIAL PRIMARY KEY` | `INTEGER PRIMARY KEY AUTOINCREMENT` | The keyword is not needed for the feed's argument; it is kept for symmetry with the old surface's schema. |
| `JSONB` | `TEXT` with `CHECK (json_valid(...))` | The old surface's precedent. |
| `TIMESTAMPTZ DEFAULT NOW()` | `TEXT DEFAULT (strftime(...))` | The old surface's precedent. |
| `VARCHAR(255)` | `TEXT` with a length check | The check counts characters, so the two backends accept and reject the same keys. |
| `GREATEST(a, b)` | `max(a, b)` with `excluded.` | Keeps the delivered watermark monotonic. |
| `unnest($1[], $2[])` | a `VALUES` CTE | The measured shape, with a correlated scalar subquery over the CTE - it
returns a row with head 0 for a stream with no events, which the batch's head lookup needs, and it costs no temporary B-tree where a join and a grouping would. |

## What this backend does not do

- It is not a migration path from the old surface. The old
  `SqliteEventRepository` stays where it is, and the two coexist.
- It does not implement the saga runner or the outbox executor. The
  intent-key index is what a later saga card would need; the runner
  itself is not here.
- It does not compare its stored bytes with another backend's. That
  comparison is E29's, and the raw bytes differ by design (D3).
