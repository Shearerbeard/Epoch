# Architecture decision records

Decision records for Epoch, in MADR style. Each record is one decision
with its drivers, the options considered, and honest consequences.
Records are immutable once accepted; a reversal lands as a new record
that supersedes the old one, and both link to each other.

Statuses: proposed → accepted → deprecated or superseded-by. Each
record's own status line is authoritative and this index mirrors it;
proposed records gate the implementation work they describe. Rejected
options are recorded inside each record.

## Index

| ADR | Decision | Status |
| --- | --- | --- |
| [0001](0001-breaking-change-budget.md) | Break the trait surface freely; consumers repin deliberately | proposed |
| [0002](0002-native-async-fn-in-trait.md) | Native async fn in trait; drop async_trait and the lifetime parameter | proposed |
| [0003](0003-split-expected-and-stream-version.md) | Split ExpectedVersion/StreamVersion; unify on 1-based sequence semantics | accepted |
| [0004](0004-typed-two-way-stream-ids.md) | Stream ids are a two-way typed contract owned by the repository | proposed |
| [0005](0005-shared-reference-append.md) | append takes &self; the version check is the concurrency contract | proposed |
| [0006](0006-atomic-multi-stream-batch.md) | Cross-stream invariants commit through an AtomicStreams batch capability | accepted |
| [0007](0007-saga-and-feed-deferred-until-pulled.md) | Saga and event feed ship when a consumer pulls them | proposed |
| [0008](0008-decider-context-constrained-or-reified.md) | Injected decider context must be constrained or reified when it gates acceptance | proposed |
| [0009](0009-private-tooling-seam.md) | Development coordination lives outside this repo | proposed |

## Provenance

The redesign was driven by friction reported while consuming Epoch
from first-party applications (chore-lottery, trucker_buddy_rs) and a
research pass over Fmodel (Fraktalio) and EvidentDB (Evident Systems).
The five design annexes (failure atlas, type skeletons, runtime
flowcharts, producer scenarios, and a draft consumer guide on
coordinating streams) are checked in under
[`../design/`](../design/README.md), and the research reports under
[`../research/`](../research/README.md).
