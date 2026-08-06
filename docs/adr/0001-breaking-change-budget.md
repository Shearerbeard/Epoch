# Break the trait surface freely; consumers repin deliberately

- Status: proposed
- Date: 2026-08
- Deciders: Mike Shearer

## Context and problem statement

Epoch is at 1.0.0-alpha.18 with two consumers, both first-party:
chore-lottery and trucker_buddy_rs. Both pin git references rather
than published versions, and trucker_buddy_rs carries its own in-repo
implementation of `VersionedEventRepositoryWithStreams`. Friction
reported from consuming applications proposes changes that
range from documentation to trait redesign. Every design that follows
must know its compatibility budget first, because hedged designs
(default type parameters, deprecation shims) roughly double the surface
for users who do not exist.

## Decision drivers

- The redesign MUST be free to change trait signatures, associated
  types, and error shapes without shims.
- Both consumers MUST have a deliberate, self-controlled upgrade
  moment; nothing may break silently under them.
- The crate SHOULD move toward a production-credible 1.0, with these two
  applications as the proving consumers.

## Considered options

1. Pre-1.0 free breakage; consumers upgrade by repinning.
2. Compatibility hedges: `S = String` default type parameters, old
   traits kept alongside new ones, deprecation cycles.
3. Fork a v2 crate and leave v1 untouched.

## Decision outcome

Option 1. Both consumers pin git references, so no change lands on
them until they repin deliberately. Chore-lottery repins as part of
this redesign; trucker_buddy_rs repins when ported and replaces its
home-grown postgres repository with the crate's `postgres` feature.
Option 2 was rejected because every hedge protects a third-party user
the crate does not have and doubles the review surface of each design.
Option 3 was rejected because all v1 consumers are first-party; a
fork would preserve code with known defects (ADR 0003).

## Consequences

- Positive: each following ADR evaluates designs on merit, without a
  migration-cost term.
- Positive: trucker-buddy's duplicated repository implementation is
  retired instead of maintained in parallel.
- Negative: the pinned consumers accumulate drift until their ports
  run; the reported friction stays unfixed for them in the interim.
- Negative: any future third-party consumer arriving before 1.0 inherits
  a moving surface. Accepted; the crate is pre-1.0 and says so.

## Links

- Motivating input: friction reported by the first-party consumer
  applications; findings summarized across ADRs 0002-0006
- Enabled by this budget: ADR 0002, 0003, 0004, 0005
