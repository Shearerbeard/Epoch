# Design annexes - 2026-08 redesign

Self-contained HTML pages from the 2026-08 redesign. Each is a single file with inline styles and SVG; serve
them from any static host or open them straight from the repo. They are
the design annexes the ADRs (`../adr/`) cite.

Reading order:

1. [two-stream-failure-atlas.html](two-stream-failure-atlas.html) -
   every failure mode of chore-lottery's hand-ordered two-stream
   draw/return, the five-case failure ledger, and the three candidate
   homes for the cross-stream invariant. Includes the Fmodel and
   EvidentDB findings.
2. [epoch-integration-skeletons.html](epoch-integration-skeletons.html) -
   type skeletons for the three integration surfaces (coordinator,
   saga + feed, atomic batch), Haskell-like model lines beside the
   post-redesign Rust, and the postgres control-plane design (advisory
   locks in place of a Datomic-style transactor).
3. [epoch-b-c-flowcharts.html](epoch-b-c-flowcharts.html) - the saga's
   at-least-once loop and the batch's commit path as runtime
   flowcharts.
4. [epoch-producer-scenarios.html](epoch-producer-scenarios.html) -
   producer-side Rust for eight scenarios (unique bounds, the draw,
   cross-stream existence pins, group invariants, transfers, an
   antagonistic flash sale, blocking OCC, and the decider-context
   staleness hazard with its two disarms).
5. [epoch-docs-coordinating-streams.html](epoch-docs-coordinating-streams.html) -
   a draft consumer guide: how a user of the crate would
   learn the batch and the saga and choose between them. Feeds the redesign's
   documentation work.

Supporting research reports live in [../research/](../research/).
