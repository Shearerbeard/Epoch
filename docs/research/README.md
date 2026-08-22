# Research reports - 2026-08 redesign

Archived reports from the redesign's research pass, kept near-verbatim
with sources. Cited by ADR 0006 and 0007; the 2026-08-22 saga
delivery survey feeds the upcoming feed/saga split records.

- [saga-delivery-upstream.md](saga-delivery-upstream.md) - where the
  community puts external mutations in a saga loop (Axon inline
  effects vs the transactional outbox), the projection/event-carried
  state transfer distinction, compensation-not-rollback, and the
  Option A/B mapping from the 2026-08-22 grill (B chosen).
- [fmodel-multi-aggregate.md](fmodel-multi-aggregate.md) - how Fmodel
  (Fraktalio) handles decisions spanning aggregates: the decider
  monoid (`combine`), the stateless saga, where at-least-once delivery
  lives, and the shared-invariant rule of thumb.
- [evidentdb-atomic-batch.md](evidentdb-atomic-batch.md) - EvidentDB's
  (Evident Systems / Bobby Calderwood) multi-stream `transactBatch`
  with `BatchConstraint`s, the single-writer transactor that backs it,
  and why the "event router" description of it is inaccurate.
