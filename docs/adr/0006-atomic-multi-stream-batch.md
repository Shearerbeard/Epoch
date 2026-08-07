# Cross-stream invariants commit through an AtomicStreams batch capability, not a two-stream coordinator

- Status: proposed
- Date: 2026-08 (revised 2026-08-07 after adversarial review; ledger below)
- Deciders: Mike Shearer

## Context and problem statement

Chore-lottery's draw needs two facts made true in two streams: card
assigned, kid holding. Single-stream append is the only primitive, so
the coordinator hand-orders the version-checked appends behind a
five-case failure ledger: race losses, orphan and half-return
windows, stale-projection repair. Roughly a
tenth of the coordinator is this boundary machinery, and the error
mapping repeats ten times (recounted 2026-08-07 against the current
coordinator source; the ledger and the source are unchanged since the
cited retro). The question was whether Epoch should ship a two-stream
primitive.

Reference research (Fmodel by Fraktalio; EvidentDB by Evident Systems)
converged on a different framing: there is no two-stream coordinator
abstraction in either design. Deciders sharing an invariant get one
atomic boundary (Fmodel `combine`, one fetch/decide/save;
EvidentDB `transactBatch` with multi-stream `BatchConstraint`s).
At research time Fmodel connected independent deciders through
stateless sagas with infrastructure-owned delivery; current upstream
also ships stateful process types alongside atomic batch execution,
which changes the saga half of that picture but not the premise this
record rests on: one atomic boundary per shared invariant.
EvidentDB buys its batch with a
Datomic-style single writer per database. Postgres does not need that
funnel: transaction-scoped advisory locks give per-batch mutual
exclusion over exactly the touched streams.

## Decision drivers

- A consumer with a shared invariant MUST be able to commit
  writes to several streams under one consistency check, so the orphan
  windows are unrepresentable rather than repaired.
- Constraints MUST be assertable over streams a batch does not write
  (existence pins, basis pins for ADR 0008).
- The capability MUST be a separate trait: a backend that cannot
  provide it (ESDB) simply does not implement it, and the missing impl
  is the compatibility statement.
- The batch MUST accept events of different types (KidEvent plus
  ChoreEvent). This reflects a constraint of the ported consumers, not
  a preference: chore-lottery's kid and chore streams pre-exist as
  separately read streams in separate categories, so an invariant that
  spans them must span streams as they are. EvidentDB's single
  CloudEvent type never faces this.
- Blocking between contending batches MUST be bounded and
  deadlock-free. Sorted acquisition addresses deadlock; a wait bound
  addresses blocking, and both are specified in the outcome below.

## Considered options

1. `AtomicStreams::transact(Batch)` capability trait, specified in the
   outcome below. Postgres implements it with one transaction:
   advisory-lock every touched and constrained stream in sorted order,
   evaluate constraints, insert all rows or roll back. In-memory
   implements it under its store mutex.
2. A blessed two-stream combinator (ordered reserve-then-accept with
   pluggable compensation).
3. Saga-first: no atomic capability; all cross-stream flows go through
   the event feed (ADR 0007) with idempotent commands.
4. Status quo: consumers hand-order appends and own the repair paths.
5. Combine the deciders behind a sum event in one stream (the Fmodel
   `combine` shape with no new Epoch surface): the invariant's
   aggregates share a single stream whose event type is the sum of
   both, and ordinary single-stream append is the atomic boundary.

## Decision outcome

Option 1. Option 2 was rejected because compensation semantics (what an
orphan means, who releases it) proved to be domain-specific decisions
rather than library concerns, and a library primitive would swallow
them; the research showed no precedent for the abstraction. Option 3
was
rejected as the default because it converts an invariant into an
eventual obligation plus an idempotency requirement on every command,
strictly more consumer machinery than the invariant needs; it remains
the right tool for reactions (ADR 0007). Option 4 remains the fallback
shape on backends without the capability and keeps working; the
failure-ledger analysis documents its cost (see the design annexes).
Option 5 is the simplest researched shape and needs nothing from
Epoch, and it remains available to any consumer whose domain tolerates
it. It was rejected as this record's answer because it relocates the
cost into the consumer's domain model: the pre-existing kid and chore
streams collapse into one stream and one category - a rebuild of
every projection and category read over them - and stream identity
from then on encodes the invariant rather than the entity. The
ported consumers' invariants span streams that must remain separately
readable, which is the mixed-event-type driver above.

### The contract, pinned

E3's implementation and its gates rest on these points; they are
decided here, not left to the diff.

- Lock identity: a stream's batch lock is the same physical advisory
  lock its single append takes today, the two-key
  `pg_advisory_xact_lock(hashtext(category), hashtext(stream_key))`
  the landed backend uses. So batches and ordinary appends
  serialize against each other on every stream they share; a
  one-argument lock or any second identity would let a batch race an
  append on the same stream.
- Lock order: the batch derives the lock tuple for every touched and
  constrained stream, deduplicates, and acquires in lexicographic
  order over the two-integer tuples. That order is total across
  categories, which is what makes cross-category deadlock cycles
  impossible; sorting stream keys as text within one category would
  not be.
- Bounded waiting: `transact` sets a transaction-scoped
  `lock_timeout` (`SET LOCAL`). Postgres applies that bound to each
  lock acquisition separately, so the batch's worst-case wait is the
  bound times its number of distinct locks - finite, and capped by
  the batch's own size, but not a single per-batch deadline, and the
  record says so rather than promising one. Expiry surfaces as a
  distinct, retryable timeout error, never as a version conflict;
  the transaction rolls back whole, and the xact-scoped locks
  release with it. The default bound and its configuration surface
  are E3 implementation decisions; that waiting is bounded per
  acquisition and how expiry is classified are decided here.
- Ownership seam: `AtomicStreams` is implemented by the backend's
  database handle, the value that owns the pool; it is not
  implemented by a single per-category store, whose scope is too
  narrow for a cross-category batch. A batch write is a contribution
  naming a category and a typed stream id with its events - data,
  not a store value - collected by the handle's own batch builder.
  No store or pool rides along with a write, so the only pool a
  transact can touch is the handle's own, and mixing databases is
  structurally impossible without any branding of the existing
  per-category stores, which stay independently constructible for
  the append path exactly as landed. The in-memory equivalent is the
  shared store root (ADR 0005 clones share it).
- Erasure contract: `Batch` is backend-scoped, not backend-neutral.
  Each capable backend owns its batch builder, and the builder's push
  method is typed per write; erasure to the backend's wire form
  happens at push, under the backend's own bound (for postgres, the
  serde bound the E1 design record records for that backend, to JSONB
  rows), and a failed encoding is a build-time error before any
  transaction starts. No universal serialization contract is imposed
  on the E1 types, which keeps the design record's rule intact.
- Constraint semantics: `BatchConstraint` is `StreamExists`,
  `StreamDoesNotExist`, or `StreamAt(sequence)` with an exact 1-based
  `StreamSequence`; the wider `ExpectedVersion` vocabulary is not
  admitted, so a constraint cannot restate `Any`. Satisfaction is
  evaluated against the same observation single append uses, the
  stream's head `COALESCE(MAX(sequence), 0)` under the held lock:
  exists means a positive head, does-not-exist means a zero head,
  at(n) means the head is exactly n. A
  violated constraint rolls the whole batch back and reports which
  constraint failed against which observed `StreamVersion`, the same
  expected-versus-actual shape as the append conflict.
- Write-side check: every write in a batch carries its own
  `ExpectedVersion` - the full append vocabulary, unlike the
  constraint language - evaluated under the held lock against the
  stream's pre-batch head, exactly as single append evaluates it. A
  batch admits at most one write per stream; pushing a second write
  for a stream already in the batch is a build-time error, which is
  what makes "pre-batch head" the only head there is (no write can
  observe another write of the same batch). A batch carries at least
  one write: a constraints-only batch is not admitted, since an
  atomic assertion with nothing to commit is a read, and admitting
  writeless batches later would be an additive change rather than a
  breaking one. A failed expectation is
  the same `VersionConflict` shape as append, naming its stream, and
  rolls the whole batch back.
- Append parity: single-stream append is semantically the degenerate
  one-write batch, meaning identical lock identity and identical
  check semantics, and the spec suite pins that parity. It remains
  its own code path; the second-write-path cost is recorded in the
  consequences.

Sorted lock acquisition makes deadlock cycles impossible; batches over
disjoint streams proceed in parallel, which the single-writer designs
give up. What is not offered: EvidentDB's database-wide basis revision.
Constraints stay per-stream, and that is stated in the docs.

## Consequences

- Positive: chore-lottery's draw becomes two loads, two decides, one
  transact; its orphan and half-return windows cease to exist on
  postgres and in-memory.
- Positive: set-validation patterns (claim streams, parent aggregates)
  and existence pins become one-line constraints.
- Negative: a second write path exists in each capable backend; the
  spec suite must cover transact with the same rigor as append and
  pin the append-parity point above (the implementation plan includes
  a raced-batches smoke test).
- Negative: consumers targeting ESDB cannot use the capability and
  keep the hand-ordered shape; portability across backends is now a
  design-time choice the type system surfaces.
- Negative: a batch holds every one of its locks for the life of its
  transaction, so contending writers on any shared stream serialize
  for longer than a single append would hold them; and the hashed
  lock identity means unrelated streams that collide under
  `hashtext` serialize with each other too, a correctness-preserving
  contention cost inherited from the landed append path.
- Negative: the lock timeout is a new failure mode consumers must
  handle: a busy system surfaces retryable timeouts where the
  hand-ordered shape surfaced version conflicts. Cancellation
  mid-transact is safe by construction (rollback releases the
  xact-scoped locks) but is also a path the smoke test exercises.
- Negative: building a batch moves one class of mistake (a write
  encoded for the wrong backend, a mispaired event and stream) from
  the trait's typed method signature to batch-build time. The push
  API is still typed per write; what is given up is the single
  method signature covering the whole batch shape at once.

## Links

- Design annexes, as research-era inputs drawn before the contract
  above was pinned - historical evidence for the decision, not
  implementation references. Where a sketch diverges from the pinned
  contract, this record governs; the integration skeletons in
  particular predate it on three points (a store value passed per
  write, `ExpectedVersion` admitted inside constraints, and a
  one-key advisory lock that would not serialize with the landed
  two-key append lock) and must not be implemented from:
  [failure atlas](../design/two-stream-failure-atlas.html),
  [integration skeletons](../design/epoch-integration-skeletons.html),
  [runtime flowcharts](../design/epoch-b-c-flowcharts.html),
  [producer scenarios](../design/epoch-producer-scenarios.html)
- Fmodel: fraktalio/fmodel discussions #37, #334; fmodel-rust;
  research report:
  [fmodel-multi-aggregate](../research/fmodel-multi-aggregate.md)
- EvidentDB: transactBatch and BatchConstraint (devdoshi/evident-db
  mirror of evidentsystems/evident-db); research report:
  [evidentdb-atomic-batch](../research/evidentdb-atomic-batch.md)
- Vocabulary from ADR 0003; ids from ADR 0004; reactions split off to
  ADR 0007; ctx rule in ADR 0008
- Landed lock identity and check semantics this record pins to:
  `src/streams/postgres.rs` (E10, the postgres event streams backend)

## Adversarial review ledger

Round 1, 2026-08-07. Author of the reviewed revision: Claude family
(drafted under the board owner). Reviewer: the codex route (GPT
family), fresh context, read-only. Verdict over the pre-revision text:
FAIL, six BLOCKING and five MINOR. Dispositions, applied in this
revision:

1. BLOCKING, bounded blocking unspecified: fixed; the outcome pins a
   transaction-scoped `lock_timeout` with expiry as a distinct
   retryable error.
2. BLOCKING, lock identity and cross-category order undefined: fixed;
   the outcome pins the landed two-key lock identity and a total
   lexicographic order over deduplicated lock tuples.
3. BLOCKING, ownership seam absent: fixed; the outcome pins
   `AtomicStreams` to the database handle with same-handle batch
   construction.
4. BLOCKING, erasure contract unresolved against the design record:
   fixed; batches are backend-scoped and erase under the backend's
   own bound at push, imposing nothing on the E1 types.
5. BLOCKING, constraint semantics undefined: fixed; the outcome pins
   the three-variant constraint language, append-identical
   satisfaction rules, and the conflict payload shape.
6. BLOCKING, the Fmodel combine alternative missing from the options:
   fixed; added as option 5 with an honest rejection tied to the
   mixed-event-type driver, which now states its business constraint.
7. MINOR, consequences omitted operational costs and contradicted the
   degenerate-batch line: fixed; lock-hold, collision, timeout, and
   cancellation costs added, and append parity restated as semantic
   with its own code path.
8. MINOR, stale error-mapping count: fixed; recounted at ten.
9. MINOR, stale Fmodel saga framing: fixed; the context now dates the
   research view and states the surviving premise.
10. MINOR, ADR 0007's commit-ordered `global_sequence` claim vs the
    landed BIGSERIAL: recorded as an erratum note on ADR 0007 this
    revision; the feed card owns visibility and cursor semantics
    before adopting the column.
11. MINOR, index status drift: the README index row for ADR 0003 now
    matches its accepted file. ADR 0005's file still reads proposed
    while its surface landed through E1's accepted gates; flipping it
    is surfaced at this record's acceptance gate rather than done
    silently.

Round 2, 2026-08-07. Same author and reviewer families, fresh
context, over the round-1 revision. Verdict: FAIL. Eight of eleven
dispositions verified; three stood, joined by one new BLOCKING and
one new MINOR. Dispositions, applied in this second revision:

1. Round-1 item 1 NOT RESOLVED as worded: `lock_timeout` bounds each
   acquisition, not the batch. Fixed: the bounded-waiting point now
   states the per-acquisition bound and the finite worst case, and
   promises no single per-batch deadline.
2. Round-1 item 3 NOT RESOLVED as worded: the unrepresentability
   claim did not hold against the landed public store constructor.
   Fixed: the ownership seam now makes writes data-only
   contributions (category plus typed id plus events) to the
   handle's builder, so no foreign pool can enter a transact and the
   landed stores stay as they are.
3. Round-1 item 11 stays open by design: flipping ADR 0005 is the
   user's lifecycle decision, surfaced at this record's acceptance
   gate; the reviewer is correct that it is unresolved until then.
4. New BLOCKING, the write-side OCC contract was unpinned: fixed;
   the outcome now pins per-write `ExpectedVersion` against the
   pre-batch head, at most one write per stream with the duplicate a
   build-time error, and the append-shaped conflict payload.
5. New MINOR, head versus count wording: fixed; constraint
   satisfaction now speaks only of the stream head.

Round 3, 2026-08-07. Same author and reviewer families, fresh
context. Verdict: FAIL. Every round-2 disposition verified (the ADR
0005 lifecycle item confirmed correctly open for the acceptance
gate), one new BLOCKING: the linked integration-skeletons annex,
drawn before the contract was pinned, contradicts it on the
ownership seam, the constraint vocabulary, and the lock identity -
implementing from it would defeat the contract. Disposition, applied
in this third
revision: the annex links are re-scoped as research-era inputs with
the three divergences named inline and an explicit
must-not-implement-from instruction; the record governs. The annexes
themselves are historical evidence and stay unedited.

Round 4, 2026-08-07. Same author and reviewer families, fresh
context. Verdict: PASS, zero new findings. The round-3 annex scoping
verified against the skeleton file itself (the three named
divergences match its actual contents, and the annex stayed
unedited), and the ADR 0005 lifecycle item confirmed correctly open
for the acceptance gate. The reviewer's remaining unverified checks
are external-repo counts and runtime behavior of the not-yet-built
implementation, which E3's own gates will execute.

Round 5, 2026-08-07, second reviewer family at the user's direction.
Reviewer: Kimi K3 on the OpenCode fireworks route (Moonshot family),
fresh context, over a staged packet including the landed backend
source; author Claude family throughout. Verdict: PASS, one MINOR.
The reviewer attacked every pinned concurrency claim against actual
postgres advisory-lock and READ COMMITTED behavior and reported each
one sound (lock identity, total lock order and its deadlock-freedom
argument including the append and unique-index paths, per-acquisition
timeout semantics, pre-batch-head evaluation, the ownership seam, and
the conflict payload described as a shape rather than the type).
Finding and disposition: (1) MINOR, the contract admitted neither a
minimum batch content nor a constraints-only batch, leaving E3 to
invent the admission rule - fixed; the write-side point now requires
at least one write and records that admitting writeless batches later
is additive, not breaking.
