# Cross-stream invariants commit through an AtomicStreams batch capability, not a two-stream coordinator

- Status: accepted (2026-08-07 at the remote acceptance gate, after the five-round two-family review the ledger records)
- Date: 2026-08 (revised 2026-08-07 after adversarial review; ledger below)
- Deciders: Mike Shearer

## Context and problem statement

Chore-lottery's draw needs two facts made true in two streams: card
assigned, kid holding. The only append primitive is single-stream, so
the coordinator hand-orders version-checked appends behind a
five-case failure ledger: race losses, orphan and half-return
windows, stale-projection repair. This boundary machinery is roughly
a tenth of the coordinator, and the error mapping repeats ten times.
(Both counts verified 2026-08-07 against the current coordinator
source; the ledger and the source are unchanged since the cited
retro.) The question: should Epoch ship a two-stream primitive?

Two researched systems pointed to a different answer. Neither Fmodel
(Fraktalio) nor EvidentDB (Evident Systems) has a two-stream
coordinator abstraction. Deciders that share an invariant get one
atomic boundary: Fmodel's `combine` (one fetch/decide/save), and
EvidentDB's `transactBatch` with multi-stream `BatchConstraint`s.

When we did the research, Fmodel connected independent deciders
through stateless sagas with infrastructure-owned delivery; upstream
now also ships stateful process types alongside atomic batch
execution. That changes the saga half of the picture, but it leaves
the premise this record rests on: one atomic boundary per shared
invariant.

EvidentDB buys its batch with a Datomic-style single writer per
database. Postgres does not need that funnel. Transaction-scoped
advisory locks give per-batch mutual exclusion over exactly the
touched streams.

## Decision drivers

- A consumer with a shared invariant MUST be able to commit writes to
  several streams under one consistency check, so orphan windows are
  unrepresentable instead of repaired.
- Constraints MUST be assertable over streams a batch does not write
  (existence pins, basis pins for ADR 0008).
- The capability MUST be a separate trait. A backend that cannot
  provide it (ESDB) simply does not implement it, and the missing
  impl is the compatibility statement.
- The batch MUST accept events of different types (KidEvent plus
  ChoreEvent). This is a constraint of the ported consumers, not a
  preference: chore-lottery's kid and chore streams already exist as
  separately read streams in separate categories, so an invariant
  spanning them must span streams as they are. EvidentDB's single
  CloudEvent type never faces this.
- Blocking between contending batches MUST be bounded and
  deadlock-free. Sorted acquisition addresses deadlock; a wait bound
  addresses blocking. Both are specified in the outcome below.

## Considered options

1. An `AtomicStreams::transact(Batch)` capability trait, specified in
   the outcome below. Postgres implements it in one transaction:
   advisory locks over every touched and constrained stream in sorted
   order, constraints evaluated, then every row inserted or none. The
   in-memory backend implements it under its store mutex.
2. A blessed two-stream combinator: ordered reserve-then-accept with
   pluggable compensation.
3. Saga-first: no atomic capability; every cross-stream flow goes
   through the event feed (ADR 0007) with idempotent commands.
4. Status quo: consumers hand-order appends and own the repair paths.
5. Combine the deciders behind a sum event in one stream (the Fmodel
   `combine` shape, no new Epoch surface): the invariant's aggregates
   share one stream whose event type is the sum of both, and ordinary
   single-stream append is the atomic boundary.

## Decision outcome

We chose option 1.

Option 2 lost because compensation semantics - what an orphan means,
who releases it - are domain decisions, not library concerns. A
library primitive would swallow them, and no researched system has
the abstraction.

Option 3 lost as the default because it turns an invariant into an
eventual obligation plus an idempotency requirement on every command:
strictly more consumer machinery than the invariant needs. It stays
the right tool for reactions (ADR 0007).

Option 4 stays the fallback on backends without the capability, and
it keeps working; the failure-ledger analysis documents its cost (see
the design annexes).

Option 5 is the simplest researched shape and needs nothing from
Epoch; a consumer whose domain tolerates it can still use it. It lost
here because it moves the cost into the consumer's domain model: the
pre-existing kid and chore streams collapse into one stream and one
category - a rebuild of every projection and category read over
them - and stream identity from then on encodes the invariant
instead of the entity. The ported consumers' invariants span streams
that must stay separately readable - the mixed-event-type driver
above.

### The contract, pinned

E3's implementation and its gates rest on these points. They are
decided here, not left to the diff.

- **Lock identity.** A batch locks a stream with the same physical
  advisory lock a single append takes today: the two-key
  `pg_advisory_xact_lock(hashtext(category), hashtext(stream_key))`
  the landed backend uses. So batches and ordinary appends serialize
  against each other on every stream they share. A one-argument lock,
  or any second identity, would let a batch race an append on the
  same stream.
- **Lock order.** The batch derives the lock tuple for every touched
  and constrained stream. After deduplication, locks are acquired in
  lexicographic order over the two-integer tuples - an order that is
  total across categories, which makes cross-category deadlock cycles
  impossible. Sorting stream keys as text within one category would
  not be total.
- **Bounded waiting.** `transact` sets a transaction-scoped
  `lock_timeout` (`SET LOCAL`). Postgres applies the bound to each
  lock acquisition separately, so the worst-case wait is the bound
  times the number of distinct locks: finite, capped by the batch's
  own size, but never a single per-batch deadline - this record does
  not promise one. Expiry surfaces as a distinct, retryable timeout
  error, never as a version conflict. The transaction rolls back
  whole and the xact-scoped locks release with it. The default bound
  and its configuration surface are E3 implementation decisions;
  per-acquisition bounding and the classification of expiry are
  decided here.
- **Ownership seam.** The backend's database handle - the value that
  owns the pool - implements `AtomicStreams`. A per-category store
  cannot: its scope is too narrow for a cross-category batch. A batch
  write is data (a category, a typed stream id, and its events),
  collected by the handle's own batch builder. No store or pool rides
  along with a write, so the only pool a transact can touch is the
  handle's own, and mixing databases is structurally impossible. The
  existing per-category stores need no branding and stay
  independently constructible for the append path. The in-memory
  equivalent is the shared store root (ADR 0005 clones share it).
- **Erasure contract.** Each capable backend owns its own `Batch`;
  there is no backend-neutral batch type. The builder's push method
  is typed per write, and erasure to the backend's wire form happens
  at push, under the backend's own bound - for postgres, the serde
  bound the E1 design record records, to JSONB rows. A failed
  encoding is a build-time error raised before any transaction
  starts. Nothing new is imposed on the E1 types, so the design
  record's rule stays intact.
- **Constraint semantics.** `BatchConstraint` is `StreamExists`,
  `StreamDoesNotExist`, or `StreamAt(sequence)` with an exact 1-based
  `StreamSequence`. The wider `ExpectedVersion` vocabulary is not
  admitted, so a constraint cannot restate `Any`. Satisfaction is
  evaluated against the same observation single append uses: the
  stream's head, `COALESCE(MAX(sequence), 0)`, read under the held
  lock. Exists means a positive head; does-not-exist means a zero
  head; at(n) means the head is exactly n. A violated constraint
  rolls the whole batch back and reports the failing constraint
  against the observed `StreamVersion`, the same expected-versus-
  actual shape as the append conflict.
- **Write-side check.** Every write in a batch carries its own
  `ExpectedVersion` - the full append vocabulary, unlike the
  constraint language - evaluated under the held lock against the
  stream's pre-batch head, exactly as single append evaluates it. A
  batch admits at most one write per stream; pushing a second write
  for the same stream is a build-time error. That is what makes the
  pre-batch head the only head there is: no write can observe another
  write of the same batch. A batch carries at least one write - an
  atomic assertion with nothing to commit is a read - and admitting
  writeless batches later would be an additive change. A failed
  expectation is the same `VersionConflict` shape as append, naming
  its stream, and rolls the whole batch back.
- **Append parity.** Single-stream append is semantically the
  degenerate one-write batch: same lock identity, same check
  semantics, and the spec suite pins that parity. Append keeps its
  own code path; the cost of the second write path is recorded in
  the consequences.

Sorted lock acquisition makes deadlock cycles impossible, and batches
over disjoint streams proceed in parallel - the property the
single-writer designs give up. What is not offered: EvidentDB's
database-wide basis revision. Constraints stay per-stream, and the
docs say so.

## Consequences

- Positive: chore-lottery's draw becomes two loads, two decides, one
  transact. Its orphan and half-return windows cease to exist on
  postgres and in-memory.
- Positive: set-validation patterns (claim streams, parent
  aggregates) and existence pins become one-line constraints.
- Negative: each capable backend now has a second write path. The
  spec suite must cover transact with the same rigor as append and
  pin the append-parity point above; the implementation plan includes
  a raced-batches smoke test.
- Negative: consumers targeting ESDB cannot use the capability and
  keep the hand-ordered shape. Portability across backends becomes a
  design-time choice the type system surfaces.
- Negative: a batch holds every one of its locks for the life of its
  transaction, so contending writers on a shared stream serialize for
  longer than a single append would hold them. Unrelated streams that
  collide under `hashtext` also serialize with each other - a
  correctness-preserving contention cost inherited from the landed
  append path.
- Negative: the lock timeout is a new failure mode consumers must
  handle. A busy system surfaces retryable timeouts where the
  hand-ordered shape surfaced version conflicts. Cancellation
  mid-transact is safe by construction (rollback releases the
  xact-scoped locks), and the smoke test exercises it anyway.
- Negative: building a batch moves one class of mistake (a write
  encoded for the wrong backend, a mispaired event and stream) from
  the trait's typed method signature to batch-build time. The push
  API stays typed per write; what is given up is one method signature
  covering the whole batch shape at once.

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
- Companion design record carrying the type surface, schema, and
  backend mechanics:
  [atomic-batch-DESIGN](../design/atomic-batch-DESIGN.md)
- Other stores measured against this contract:
  [backend survey](../research/atomic-batch-backend-survey.md).
  KurrentDB (full contract, 26.1+) and redis streams (Lua-script
  funnel) are implementable and are scoped as future work on the
  maintainer's redesign board; neither is part of any current card.

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

Round 6, 2026-08-07, prose only, at the maintainer's direction. Four
models rewrote the record's prose in one wide round - codex (GPT
family), Kimi K3 (Moonshot), DeepSeek V4 Pro, and Gemini 3.6 Pro (the
agy route, spend user-approved) - and the board owner merged the
strongest phrasing. No technical content changed: every decision,
number, RFC-2119 keyword, and cross-reference of the accepted
revision survives in meaning, and the Links and ledger sections were
out of the brief's scope. The same revision adds two links: the
companion design record (types, schema, mechanics - added today so
this record stays readable) and the backend survey with its
future-work scoping.
