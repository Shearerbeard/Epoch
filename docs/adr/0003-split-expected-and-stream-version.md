# Split RepositoryVersion into ExpectedVersion and StreamVersion, unified on 1-based sequence semantics

- Status: accepted (2026-08-06, at the E2 plan's Stage 0 user gate)
- Date: 2026-08
- Deciders: Mike Shearer

## Context and problem statement

`RepositoryVersion<V>` is one enum doing two jobs. `Any`, `NoStream`,
and `StreamExists` are assertions a writer makes on append;
`Exact`/`NoStream` are observations a load reports. The single type lets
`load` theoretically return `Any` and lets `append` receive
`StreamExists` as if it were a position.

Worse, the two live backends disagree, and the in-memory one is unsound.
Postgres uses a 1-based `sequence` with `NoStream` for an empty stream.
In-memory tracks `position = events.len() - 1`, so an empty stream and a
one-event stream are both version 0: two racing single-event appends at
`Exact(0)` both succeed, a silent lost update. Chore-lottery's deciders
emit single-event batches, so its in-memory race tests partly exercised
an OCC that was not there. Consumers asked for documented version
semantics; the code needs repair before documentation can be honest.

## Decision drivers

- The write-side and read-side version vocabularies MUST be separate
  types, so illegal states (loading `Any`, appending at `StreamExists`)
  are unrepresentable.
- All backends MUST share one observable semantics: empty stream loads
  as `NoStream`; a stream's version after N events is sequence N,
  1-based.
- The universal spec test MUST include a single-event OCC race, so the
  lost-update class cannot return.
- Semantics SHOULD stay mappable onto ESDB's expected-revision model,
  which the postgres backend already mirrors.

## Considered options

1. Split into `ExpectedVersion<V>` (Any | NoStream | StreamExists |
   Exact) for append and `StreamVersion<V>` (NoStream | Exact) for
   load/append results; rebuild in-memory on the postgres semantics.
2. Keep one enum; fix the in-memory arithmetic only.
3. Keep one enum; document the divergent semantics per backend
   (documentation alone, as consumers first requested).

## Decision outcome

Option 1. Option 2 fixes the bug but leaves the type permitting the
illegal states that made the bug hard to see. Option 3 was rejected
outright: it would document a lost-update defect as intended behavior.
The spec suite grows two cases that pin the semantics: a single-event
OCC race (exactly one of two racers wins) and an antagonistic
flash-sale case (N concurrent single-event appends against stock K,
exactly K winners).

## Consequences

- Positive: version conflicts become trustworthy in every backend that
  passes the spec suite, including the one tests rely on.
- Positive: `load` on a missing stream answers `NoStream` everywhere,
  removing a recurring source of reviewer confusion.
- Negative: every backend and every consumer call site changes shape.
  Covered by the ADR 0001 budget; both consumer ports absorb it.
- Negative: in-memory history written by older code has no migration
  path. Accepted: in-memory stores are ephemeral by nature.

## Links

- Version vocabulary consumed by ADR 0006 (batch constraints)
