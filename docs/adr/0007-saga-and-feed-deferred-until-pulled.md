# The saga and event feed ship when a consumer pulls them, not with the foundation

- Status: accepted (the deferral is discharged by its own trigger:
  the pull happened, and the feed and the saga runtime shipped as
  ADR 0010 and its outbox-saga section; marked at the runner card's
  gate A, 2026-09)
- Date: 2026-08
- Deciders: Mike Shearer

## Context and problem statement

The redesign identifies two cross-stream tools: the atomic batch for
shared invariants (ADR 0006) and the saga for reactions. The saga's
consumer surface is small (a pure `react: fn(&Event) -> Vec<Command>`),
which makes it look cheap. Its real cost is the delivery control plane
Epoch must ship around it: an `EventFeed` with consumer groups and
cursors over the committed log, at-least-once redelivery, a
`SagaRunner`, and a new obligation on consumers to make every
saga-issued command idempotent. Both reference designs place retries in
this infrastructure layer, never in the saga.

No current consumer needs it yet. Chore-lottery's draw is
invariant-shaped (batch territory); its reaction-shaped work (an
export to an external task tracker, read-model maintenance,
notifications) is not yet built. trucker_buddy_rs has no
reaction-shaped work on its port path.

## Decision drivers

- Machinery with delivery guarantees MUST be designed against a real
  consumer, not speculatively; a feed with no consumer cannot validate
  its cursor and redelivery semantics.
- The foundation MUST NOT block on it: nothing in ADRs 0002-0006
  depends on the feed.
- The committed-log schema SHOULD already carry what a feed needs, so
  adding one later attaches without reworking the store.

## Considered options

1. Defer: specify the shape now (Saga, EventFeed, SagaRunner as
   sketched in the design annexes), build when the first
   reaction-shaped consumer work pulls it.
2. Ship it with the initial release.
3. Never ship it; consumers bring their own delivery loop.

## Decision outcome

Option 1. Option 2 was rejected because it front-loads the largest
runtime surface of the redesign ahead of any consumer able to exercise
redelivery, offsets, or idempotency for real. The saga's small
`react` surface understates its cost; the delivery control plane
behind it is the bulk of the work. Option 3 was rejected because the
poll/ack/offset loop is exactly the kind of subtle, repeated
infrastructure a library should own once, and both reference designs
treat it as platform.

Discharge note (2026-09, the runner card's gate A): the trigger this
record waits on - a real consumer pulling the machinery - fired in
two steps. chore-lottery's vikunja-sync integration shipped and
logged the demand signal (the pull ADR 0010 opens with), and the
event feed landed as ADR 0010's single-writer funnel with the saga
runner and outbox executor completing its outbox-saga section. The
deferral is discharged by its own terms; the maintenance-tracking
note below is historical. The wave plan that sequenced the work
lives on the maintainer's private board (ADR 0009's seam); this
public record links only to ADR 0010.

The postgres schema already satisfies the forward-compatibility
driver: `global_sequence` is commit-ordered and is the feed cursor
unchanged. Erratum (2026-08-07, from ADR 0006's adversarial review):
the landed E10 schema implements `global_sequence` as `BIGSERIAL`,
which allocates before commit, so concurrent transactions can commit
in the opposite order of their sequence values. The column is not
commit-ordered as written, and the feed card must define visibility
and cursor semantics (gap and reorder handling) before adopting it.
The deferred work is tracked on the maintainer's
redesign board with a do-not-pull-until-needed note.

## Consequences

- Positive: the initial release stays type-surface work plus one
  capability; the riskiest runtime code waits for a driving consumer.
- Positive: the first saga will be a real integration (an export to
  an external task tracker), a better semantics proof than a
  retrofitted example.
- Negative: until then, reaction-shaped needs in consumers use ad hoc
  loops; chore-lottery's ready-pool projection keeps its current
  in-process update path.
- Negative: deferral risks the classic second-half slip; this
  record keeps the deferral explicit and revisitable.

## Links

- [ADR 0010 - the discharge: the feed's cursor semantics and the outbox-saga section](0010-allocation-ledger-honest-cursor.md)
- Split decided in ADR 0006; design annexes carry the sketched types
- First consumer in fact: chore-lottery's vikunja-sync integration
  (its migration onto the shipped runtime is tracked on the
  consuming repo's own board, per ADR 0009's seam)
