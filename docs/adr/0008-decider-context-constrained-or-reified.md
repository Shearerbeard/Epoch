# Injected decider context must be constrained or reified when it gates acceptance

- Status: proposed
- Date: 2026-08
- Deciders: Mike Shearer

## Context and problem statement

`DeciderWithContext::decide(ctx, state, cmd)` injects roll-up state (a
users read model for uniqueness, a fleet view for caps) into an
otherwise pure decide. The defect: `ctx` has no
optimistic-concurrency handle. The append's version check covers only
the target stream; the store verifies the
data a decider folds, never the data it is handed. So a decision that
`ctx` can veto is made against unverifiable staleness, and a
racing write that invalidates `ctx` commits without any party
noticing. Trucker-buddy uses this shape today for uniqueness bounds.

## Decision drivers

- A `ctx` that can turn an accept into a reject MUST have its
  freshness checked at commit time, or MUST be restructured so the
  invariant lives in streams.
- Enrichment context (ids, clocks, configuration, data that shapes
  event payloads but never gates acceptance) SHOULD stay free-floating;
  it carries no staleness risk worth machinery.
- The fix for wide-basis contexts SHOULD push toward better aggregate
  design rather than coarser locking.

## Considered options

1. Two sanctioned patterns, chosen by invariant weight:
   (a) `Versioned<Ctx>` carrying a basis (the stream keys and versions
   the ctx was read at), re-asserted by the committing batch as
   `StreamAt` constraints, so stale ctx becomes a typed conflict;
   (b) reify the invariant into streams: a claim stream per claimed
   value, or a parent aggregate stream for group invariants, making
   the batch constraints exactly the invariant's true participants.
2. Versioned ctx only, applied uniformly.
3. Leave `ctx` free-floating and document the risk.

## Decision outcome

Option 1. Pattern (a) is honest but has a scaling cliff: a ctx derived
from a whole category makes the basis a high-water mark over it, so
any unrelated write conflicts the command. That cliff is a signal the
invariant was never about the whole category, and the remedy is
pattern (b), which shrinks contention to real contention. Option 2 was
rejected for institutionalizing the cliff. Option 3 was rejected
because the failure is silent invariant corruption, the worst class to
document around; trucker_buddy_rs migrates its uniqueness bound to a
claim stream during its port.

The rule, stated for the docs: ctx is for enrichment; the moment it
gates acceptance it must be constrained (pattern a) or reified
(pattern b).

## Consequences

- Positive: the silent-staleness class is closed with a typed,
  testable failure mode; the two patterns are teachable and appear in
  the producer-scenarios annex with code.
- Positive: pressure toward claim streams and parent aggregates
  improves domain models independently of the safety win.
- Negative: `DeciderWithContext` alone no longer tells the whole
  story; soundness depends on the committing side honoring the rule.
  A lint cannot enforce it; code review of the committing side must.
- Negative: pattern (a) needs read models that report their basis;
  view builders grow a versioned-read surface where used.

## Links

- Worked example:
  [producer scenarios](../design/epoch-producer-scenarios.html),
  scenario 8
- Depends on ADR 0006 for `StreamAt` constraints
- trucker_buddy_rs migrates its uniqueness bound during its port
