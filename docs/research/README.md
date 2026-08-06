# Research reports - 2026-08 redesign

Archived reports from the redesign's research pass, kept near-verbatim
with sources. Cited by ADR 0006 and 0007.

- [fmodel-multi-aggregate.md](fmodel-multi-aggregate.md) - how Fmodel
  (Fraktalio) handles decisions spanning aggregates: the decider
  monoid (`combine`), the stateless saga, where at-least-once delivery
  lives, and the shared-invariant rule of thumb.
- [evidentdb-atomic-batch.md](evidentdb-atomic-batch.md) - EvidentDB's
  (Evident Systems / Bobby Calderwood) multi-stream `transactBatch`
  with `BatchConstraint`s, the single-writer transactor that backs it,
  and why the "event router" description of it is inaccurate.
