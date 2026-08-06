# Cross-stream invariants commit through an AtomicStreams batch capability, not a two-stream coordinator

- Status: proposed
- Date: 2026-08
- Deciders: Mike Shearer

## Context and problem statement

Chore-lottery's draw must make two facts true in two streams (card
assigned, kid holding). With single-stream append as the only
primitive, its coordinator hand-orders the version-checked appends and
carries a five-case failure ledger covering race losses, orphan and
half-return windows, and stale-projection repair. Roughly a
tenth of the coordinator is this boundary machinery, and the error
mapping repeats eight times. The question was whether Epoch should
ship a two-stream primitive.

Reference research (Fmodel by Fraktalio; EvidentDB by Evident Systems)
converged on a different framing: there is no two-stream coordinator
abstraction in either design. Deciders sharing an invariant get one
atomic boundary (Fmodel `combine`, one fetch/decide/save;
EvidentDB `transactBatch` with multi-stream `BatchConstraint`s);
independent deciders connect through stateless sagas with
infrastructure-owned delivery. EvidentDB buys its batch with a
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
  ChoreEvent), which EvidentDB's single CloudEvent type never faces.
- Blocking between contending batches MUST be bounded and deadlock-free.

## Considered options

1. `AtomicStreams::transact(Batch)` capability trait. `Batch` collects
   type-erased writes (events serialize at the boundary, where they
   become rows regardless) plus `BatchConstraint`s (StreamExists,
   StreamDoesNotExist, StreamAt). Postgres implements it with one
   transaction: advisory-lock every touched and constrained stream key
   in sorted order, evaluate constraints against MAX(sequence), insert
   all rows or roll back. In-memory implements it under its store
   mutex. Single-stream append remains the degenerate one-write batch.
2. A blessed two-stream combinator (ordered reserve-then-accept with
   pluggable compensation).
3. Saga-first: no atomic capability; all cross-stream flows go through
   the event feed (ADR 0007) with idempotent commands.
4. Status quo: consumers hand-order appends and own the repair paths.

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
  spec suite must cover transact with the same rigor as append
  (the implementation plan includes a raced-batches smoke test).
- Negative: consumers targeting ESDB cannot use the capability and
  keep the hand-ordered shape; portability across backends is now a
  design-time choice the type system surfaces.
- Negative: type erasure in `Batch` moves a class of mistakes
  (mismatched event/stream pairing) from compile time to batch-build
  time errors. Accepted: the write method is still typed at its
  entrance.

## Links

- Design annexes:
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
