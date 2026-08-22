<!-- vale off -->
<!-- Research report from the 2026-08-22 saga design grill; sources kept
     verbatim-linked so a future ADR can refine against them directly. -->

# Saga delivery semantics — upstream survey for the feed/saga split

Research pass on 2026-08-22, feeding the E6 split (EventFeed vs
SagaRunner) and the outbox-saga decision recorded in the grill.
Companion report to [fmodel-multi-aggregate.md](fmodel-multi-aggregate.md),
which covers the Fmodel decider/saga algebra itself; this report covers
what the wider community says about the *delivery control plane* and
where external mutations belong.

## Sources

- Fmodel Rust saga module —
  [docs.rs/fmodel_rust::saga](https://docs.rs/fmodel-rust/latest/fmodel_rust/saga/index.html),
  documented as "Domain layer - pure mapper of action results/events
  into new actions/commands"; the crate pairs it with an
  application-layer saga manager. Site: [fraktalio.com/fmodel](https://fraktalio.com/fmodel/).
- Martin Fowler, [The Many Meanings of Event-Driven Architecture](https://www.youtube.com/watch?v=STKCRSUsyP0)
  (GOTO 2017) — the four-pattern taxonomy: event notification,
  event-carried state transfer, event sourcing, CQRS.
- Bobby Calderwood, [From Complexity to Simplicity](https://www.youtube.com/watch?v=V7vhSHqMxus)
  (interview on his event-driven design practice) and the EvidentDB
  material already surveyed in
  [evidentdb-atomic-batch.md](evidentdb-atomic-batch.md).
- Chris Richardson's saga taxonomy (microservices.io): orchestration
  vs choreography; **compensating transactions** as the only honest
  answer to a failed or mis-ordered cross-context mutation — there is
  no rollback spanning contexts.
- Axon Framework's saga runtime: tracking event processors with a
  token store (a durable cursor over the event stream), at-least-once
  redelivery, handlers that may issue commands *and* perform side
  effects, token advancing only after handler success.
- The transactional outbox/inbox pattern (various; standard reference
  practice): record intent to act as an event inside the database
  transaction, let a relay/executor perform the external mutation and
  record its outcome.

## What the community agrees on

1. The pure saga (events in → commands out) is a *domain-layer*
   concept (Fmodel). Side effects are never in it.
2. The delivery control plane — durable cursor, at-least-once,
   redeliver-until-acked — is *application/platform* layer (Axon
   tracking processors; Fmodel's saga manager; EvidentDB streams).
   Every production-grade implementation puts a persisted checkpoint
   under the reaction loop.
3. Cross-context state sync is a **projection** (Calderwood's
   event-carried state transfer): the downstream context subscribes,
   keeps local state, never calls back. A projector over a feed —
   epoch's Vikunja case — is this pattern and stays distinct from
   sagas even when both consume the same feed.
4. When a mutation lands at the wrong time or fails, upstream advice
   is uniform: **compensate, don't roll back**. vikunja-sync's
   `Revert(restore lane, bot comment)` is a textbook compensation.

## Where the schools split: who performs external mutations

- **Axon school (inline effects):** the reaction handler performs the
  external call under the cursor; the checkpoint advances only after
  success; idempotency absorbs redelivery. Less machinery; couples
  cursor progress to HTTP latency; opaque to replay (no record of
  what was attempted).
- **Outbox school (effects as facts):** the reaction appends
  commands *and effect intents* as events in one transaction; a
  separate executor consumes the intent stream, performs the
  mutation, appends done/failed/compensation events. Every guarantee
  is expressible in streams; the outbox is an auditable log of
  attempted mutations; cursor ack is trivial (the append *is* the
  transaction).

## Mapping to epoch (the grill's decision)

Two options were drawn during the 2026-08-22 grill:

- **Option A** — `react -> [Command | Effect]`, SagaRunner executes
  both arms, cursor acks last. Fmodel's command arm + Axon's effect
  discipline. Rejected.
- **Option B (chosen)** — `react -> [Command | EffectRequest]`, both
  appended transactionally (the AtomicStreams batch from ADR 0006 is
  exactly this append); an outbox executor stream performs external
  mutations and records outcomes; failures become compensation
  events (vikunja-sync's Revert machinery is the first compensation
  engine). Chosen because it keeps every guarantee in streams,
  survives crash-replay, and gives the wrong-time-sync failure mode a
  first-class record instead of ad hoc retries.

Note the composition: Option B needs no new transactional machinery —
the multi-stream atomic append already shipped as E3, and the feed
(E6a) is the only new primitive. First executor: chore-lottery's
Vikunja sync, migrating off its in-process `EventTap`
(crates/vikunja-sync, S16 demand signal of 2026-08-19).
