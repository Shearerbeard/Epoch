# AtomicStreams batch - design record

Companion to [ADR 0006](../adr/0006-atomic-multi-stream-batch.md),
which owns the decision and the behavioral contract. This record owns
the concrete shapes: the type surface, the postgres mechanics, the
in-memory store design, and the future-backend scoping. The ADR stays
readable; the details live here.

Anchor stamp: claims verified against `card/e3` commit `4165ab2`
(2026-08-07, the E3 implementation range, in review). Re-verifying
bumps this stamp.

## Type surface (backend-neutral, `src/streams/batch.rs`)

```rust
/// A whole stream address: category plus the id's rendered key
/// (ADR 0004 - the repository owns namespacing).
pub struct StreamRef { /* category, key */ }
impl StreamRef {
    pub fn new<Id: StreamId>(category: &str, id: &Id) -> Self;
}

/// Exactly three constraints; `Any` is not admitted.
pub enum BatchConstraint {
    StreamExists,
    StreamDoesNotExist,
    StreamAt(StreamSequence),   // exact 1-based head pin
}
impl BatchConstraint {
    /// Evaluated against the locked head, append-identical rules.
    pub fn satisfied_by(self, observed: StreamVersion) -> bool;
}

/// One write: stream + full ExpectedVersion vocabulary + events.
pub struct BatchWrite<W> { /* stream, expected, events */ }

/// Nonempty, one write per stream - both structural: the only way
/// to obtain a Batch is BatchBuilder::build, which rejects
/// WritelessBatch, after push rejected DuplicateWrite.
pub struct Batch<W> { /* writes, constraints */ }
pub struct BatchBuilder<W>;
impl<W> BatchBuilder<W> {
    pub fn require<Id: StreamId>(
        &mut self, category: &str, id: &Id, constraint: BatchConstraint,
    ) -> &mut Self;
    pub fn build(self) -> Result<Batch<W>, WritelessBatch>;
}

/// Lock-timeout expiry is its own arm, never a conflict.
pub enum TransactError<E> {
    Conflict(BatchConflict),            // expected-vs-actual, per stream
    ConstraintViolated(ConstraintViolation),
    LockTimeout(Duration),              // retryable
    Backend(E),
}

pub trait AtomicStreams {              // #[trait_variant::make(Send)]
    type Batch: Send;
    type Error;
    async fn transact(&self, batch: Self::Batch)
        -> Result<(), TransactError<Self::Error>>;
}
```

`W` is each backend's encoded-write type, so the erasure to a wire
form happens at the backend's typed `write` push - fallibly, before
any transaction exists - and a `Batch` can never carry a store or a
pool (the ADR's ownership seam). `BatchConflict::as_version_conflict`
reproduces the append conflict payload exactly.

## Postgres implementation (`src/streams/postgres/batch.rs`)

```rust
pub struct PgDatabase { /* pool, lock_timeout, idle_timeout */ }
impl PgDatabase {
    pub fn new(pool: PgPool) -> Self;              // default lock bound 5s, idle 30s
    pub fn with_lock_timeout(self, bound: Duration) -> Self;
    pub fn with_idle_transaction_timeout(self, bound: Duration) -> Self;
    pub fn batch(&self) -> PgBatchBuilder;         // Batch<EncodedEvent>
}
impl PgBatchBuilder {
    /// serde_json encoding here, at push - PgWriteError::Encoding
    /// before any transaction.
    pub fn write<Id, E: Serialize>(...) -> Result<&mut Self, PgWriteError>;
}
```

`transact`, one transaction end to end. The pre-pivot design ordered
per-stream advisory locks (hashtext hashing, then client-side sort and
dedupe via a `lock_order` function); ADR 0010's pivot replaced that
with one global writer lock, so the sequence is now:

1. `SET LOCAL lock_timeout = '<n>ms'; SET LOCAL
   idle_in_transaction_session_timeout = '<n>ms'` - one command, two
   statements. Both values derive from a `Duration` (milliseconds,
   floored at 1 so they can never mean "wait forever" or "no limit");
   nothing caller-controlled is interpolated, and a value the server
   rejects as out of range fails loudly rather than saturating.
2. One global writer lock: `SELECT pg_advisory_xact_lock($1)` with
   `WRITER_LOCK`, the same funnel single append takes, so exactly one
   event transaction runs at a time and insert order is commit order.
3. Every addressed head in one query under the held lock:
   `COALESCE(MAX(sequence), 0)` grouped over the addressed pairs.
4. Constraints checked, then write expectations; any failure returns
   before the first insert and the transaction drops (rollback).
5. Inserts at head+1.. per stream into the landed `stream_events`
   table (no schema change; the
   `UNIQUE (category, stream_key, sequence)` index is the storage
   backstop), then commit.

SQLSTATE `55P03` (lock_not_available) maps to
`TransactError::LockTimeout`; other database errors map to
`Backend`. The 5-second default lock bound and 30-second default idle
bound are E3/C4 implementation choices the ADR delegated; revisit
alongside consumer experience.

Round-trip cost:

- 1 to configure the two timeout GUCs
- 1 for the global writer lock
- 1 for the heads query
- 1 per event insert
- 1 commit

## In-memory store design (`src/streams/in_memory.rs`)

The ADR's seam ("the shared store root") required a root that did not
exist: the pre-E3 store was a private per-category typed log, so a
cross-category mixed-type batch had nothing to address. The store is
now a category view onto an `InMemoryDatabase` root:

- One oldest-first log behind one mutex holding every category;
  payloads erased as `Arc<dyn Any + Send + Sync>`.
- Each category carries a `TypeId` claim, set on first open or write
  (`CategoryTypeMismatch` on contradiction), so the read-side
  downcast is a corruption assertion rather than a hazard.
- `InMemoryDatabase::category::<E>(name)` derives a typed store view;
  `InMemoryEventStreams::<E>::new()` still works by opening a private
  root, so existing tests and consumers are untouched.
- `transact` takes the one guard over the root: every check and every
  write is a single critical section, so there is no acquisition
  order to get wrong. Append routes through the same
  `expectation_satisfied` as the batch write-side check, making the
  ADR's append parity structural.
- Cost of the design: the `EventStreams` impl bound gained
  `E: 'static` (required by `Any` erasure).

## Future backends (scoped, not current work)

The [backend survey](../research/atomic-batch-backend-survey.md)
(2026-08-07, two models compared) measured other stores against this
contract. Two are implementable; both are scoped as future work on
the maintainer's redesign board and stay out of every current card:

- KurrentDB: 25.1 ships atomic multi-stream appends with per-stream
  expected versions; 26.1 adds consistency checks over streams a
  batch does not write - the full contract, natively, with no lock
  vocabulary. Before any card: verify revision-pin checks on
  unwritten streams in the 26.1 gRPC proto, and Rust client coverage
  of `appendRecords`.
- Redis Streams: one atomic server-side Lua script per transact (the
  EvidentDB single-writer funnel, not postgres locking), synthetic
  1-based `XADD` ids, hash-slot co-location and durability trades
  stated on the card that picks it up.
- ClickHouse: not a write-model target (no advisory locks, no unique
  indexes, no write-write conflict detection); it belongs on the
  ADR 0007 feed's read side.
