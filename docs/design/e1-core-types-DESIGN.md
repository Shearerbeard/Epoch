# E1 core type surface - design record

Layer-1 typed-holes skeleton for the redesigned core stream surface,
`src/streams.rs`. Panel input: this record, the skeleton commits, and
ADRs 0001-0005. Revision 3, after the two-seat design panel (ledger in
the board's `reviews/e1/`; Gate A PASS over d9d532b..abe1c82).

Anchor stamp: claims verified against `card/e10` commit `29bcaab`
(2026-08-07 serde-bound amendment at the E10 Gate U; previously
`card/e2` `694c31c`, 2026-08-06). Re-verifying bumps this stamp.

## Type-to-ADR and business-rule map

| Public item | ADR | Business rule | Invalid state it forbids |
| --- | --- | --- | --- |
| `StreamId` trait | 0004 | A stream id is a two-way contract: render to a storage key and parse back | Category reads returning untyped keys. Round-tripping itself is a behavioral law pinned by the spec suite (E2), not a type guarantee |
| `StreamId for String` | 0004 | `String` keeps working for tests and simple consumers | - (compatibility surface required by the ADR; concrete impl, not blanket) |
| `StreamSequence` | 0003 | A stream position is a 1-based sequence | `Exact(0)`, negative, or non-numeric positions; the 0-based empty/one-event ambiguity behind the lost-update bug |
| `ExpectedVersion` | 0003 | A writer's append-time assertion about stream state | A load reporting `Any`/`StreamExists`; a zero position in `Exact` |
| `StreamVersion` | 0003 | The store's observed position | An observation of `Any`/`StreamExists`; zero positions |
| `EventBatch<E>` | 0003, 0005 | An append carries at least one event | The well-typed empty append |
| `StreamState<E>` | 0003 | A full load's version IS its event count; `version()` derives it | Present-but-empty; missing-with-a-position; a history whose length disagrees with its position |
| `StreamSlice<E>` | 0003 | An incremental read returns events at-or-after a cursor plus the observed position; construction validates the pair | Conflating "no events in range" with "no stream"; events claimed from a `NoStream` observation |
| `CategoryEvent<Id, E>` | 0004 | A category read pairs each event with its typed stream id (bounds on the type) | Category results a consumer cannot address by typed id; `CategoryEvent<(), E>` |
| `ZeroSequence` | 0003 | Zero is not a 1-based position | (error carrier for `StreamSequence::new`) |
| `EmptyBatch` | 0003, 0005 | An append carries at least one event | (error carrier for `EventBatch::new`) |
| `NotAConflict` | 0003 | Only failed checks are conflicts | (error carrier for `VersionConflict::new`) |
| `MisshapenSlice` | 0003 | A slice's events must be consistent with its observation | (error carrier for `StreamSlice::new`) |
| `From<StreamVersion> for ExpectedVersion` | 0003 | An observed position is a valid next-append expectation | Hand-mapping between the vocabularies at call sites |
| `VersionConflict` | 0003 | A conflict is an assertion that failed against an observation | `Any` as a failed expectation; pairs the check would satisfy |
| `AppendError<E>` | 0003 | Append fails as a version conflict or a backend error, nothing else | A third failure class; conflict payloads in impossible shapes |
| `LoadError<B, P>` | 0004 | Category reads fail as backend faults or unparseable stored keys, which callers treat differently | Collapsing a data defect into a retryable backend error |
| `EventStreams<E>` | 0002, 0003, 0004, 0005 | Versioned streams over a typed id; the version check is the concurrency contract | Lifetime/HRTB noise in consumer bounds; `&mut self` implying receiver exclusivity |

## Visibility and seams

- `streams` is a new top-level public module; it touches nothing else.
- Reaches into: `crate::decider::Event` (the `E` bound), `thiserror`,
  `trait_variant`, `std`. No repository code is referenced; existing
  modules and the old trait surface are untouched this card.
- No test-only accessors; no `#[cfg(test)]` surface yet (E2 owns the
  spec tests).
- Hole inventory (`grep -n 'todo!()' src/streams.rs`, code sites only):
  16 holes at the skeleton commit - `String::stream_key`,
  `String::parse_key`, `StreamSequence::{new, get}`,
  `From<StreamVersion>::from`, `EventBatch::{new, as_slice, into_vec}`,
  `StreamState::version`, `StreamSlice::{new, events, at, into_parts}`,
  `VersionConflict::{new, expected, actual}`. Trait methods carry no
  bodies. Trivial accessors are held open with the rest: the card's
  acceptance says no filled bodies until the panel passes, which
  overrides the typed-holes preference for landing them.
- E2 fills (stage 1, per its hole-fill bound; `#[expect]` markers swept
  with each fill): `String::stream_key`, `String::parse_key`,
  `StreamSequence::new`, `StreamSequence::get`. 12 holes remain -
  `From<StreamVersion>::from`, `EventBatch::{new, as_slice, into_vec}`,
  `StreamState::version`, `StreamSlice::{new, events, at, into_parts}`,
  `VersionConflict::{new, expected, actual}`. Where the E2 in-memory
  implementation and spec suite (child modules of `streams`) need a
  still-open constructor or accessor, they use crate-internal field
  access with the invariant upheld at the site and a comment naming the
  hole, rather than filling outside the bound.
  `clippy::todo` is not enabled (no lint table in this repo; warn would
  fail the `-D warnings` fill gate); the grep inventory is the route.

## Decisions from panel round 1

- `#[trait_variant::make(Send)]` in this form rewrites the trait in
  place (futures get `Send`); it does not emit a second trait. ADR
  0002's rustdoc-second-trait consequence is an erratum to surface at
  the design-panel user gate.
- `load_from_version` semantics are inclusive (at-or-after), matching
  the existing PostgreSQL and ESDB backends; a cursor past the tail
  returns an empty slice with the observed position, not an error. ADR
  0003 should say this explicitly - flagged for the user gate.
- serde derives are intentionally absent from every type here. A
  backend that needs a wire form constrains its own impl instead:
  the postgres backend (E10) bounds `E: Serialize + DeserializeOwned`
  at its `EventStreams` impl and stores payloads as JSONB directly.
  This amends the original line, which required a backend-owned DTO
  with fallible conversion; the E10 Gate A review escalated the
  departure and the user accepted it at that card's Gate U
  (2026-08-07): a DTO over `serde_json::Value` here would convert
  nothing. The E1 core types themselves still carry no derives, and a
  backend whose wire form differs from the event's serde output still
  owns a DTO.
- `StreamVersion` no longer derives `Ord`; `StreamSequence` keeps it
  (positions in one stream are ordered by definition). No cross-variant
  ordering is invented.
- `append` returns the new `StreamSequence` only; echoing the caller's
  events back was redundant surface.

## ADR-outcome ownership

| Deferred outcome | Owner |
| --- | --- |
| In-memory rebuild on 1-based semantics; OCC and flash-sale spec cases; round-trip conformance suite | E2 (version semantics spec tests) |
| Backend ports to `EventStreams`; `AppendError` wiring | E3 (atomic streams batch) and the port cards E4/E5 |
| Category pagination / feed cursors | E6 (event feed saga runner) |
| Removing `async-trait`, deleting the old trait surface and orphaned traits (`WithFineGrainedStreamId`, `StreamIdFromEvent`, `RepositoryVersion`), edition 2024 bump | Unassigned - no card owns teardown; raised at the design-panel user gate as a candidate card |
| In-memory shared-store `Clone` contract (ADR 0005) | E2 (in-memory rebuild) |

## Residual risks

- The repo-wide `cargo clippy -- -D warnings` acceptance cannot pass:
  18 pre-existing lib warnings on the base branch, outside card scope.
  Warning sets on `redesign/bootstrap` and `card/e1` are identical;
  the skeleton adds zero. Logged on the card as an explicit divergence.
- `StreamSequence` fixes positions to `u64` crate-wide. PostgreSQL's
  `i64` sequence and in-memory counters map losslessly; ESDB's 0-based
  revisions map with +1; the old `RedisVersion` (timestamp-version
  pair) cannot map and stays on the old surface until its port card
  decides its fate. Flagged at the design-panel user gate.
- The `String` `StreamId` impl keeps a raw-string ingress the panel's
  Seat 1 graded blocking; retained because ADR 0004 requires it. The
  either-or (keep, constrain, or revise the ADR) is presented at the
  design-panel user gate.
- The old and new vocabularies coexist in the crate until teardown is
  assigned (see ownership table).
