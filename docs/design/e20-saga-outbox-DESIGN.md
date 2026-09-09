# E20 saga runner and outbox executor - design record (skeleton)

Status: WIP, Gate S skeleton, design-panel repairs applied. The type
surface implements the E20 card's Deliverable - the four-round
adversarially reviewed spec - behind the in-memory feed. Naming is
indicative until gate A. ADR 0010's outbox-saga section and ADR 0007's
discharged-by-trigger mark land at gate A as those records' revisions,
per the card. The two-seat design panel ran on the skeleton commit;
its findings and dispositions are recorded on the card's review
ledger, and the repairs are noted in place below.

The skeleton runs against the shipped E19 surface: the runner polls an
`EventFeed`, folds reactions through the E3 `AtomicStreams` batch, and
re-reads the outbox stream through `EventStreams`. The in-memory
backend accepts duplicate intent keys (the documented v1 divergence),
so the duplicate-intent rejection protocol cannot be proven there; the
crash-table proofs are gate M's, on live postgres, by the card's own
design.

## Type-to-business-rule map - saga module

| Type | One business rule | Invalid state it forbids |
| --- | --- | --- |
| `SagaId` | A saga has an identity: it names the runner's consumer group and the saga's outbox stream, and it opens every intent key the runner mints for it. | The anonymous saga: unaddressable, indistinguishable from a forgotten argument. |
| `ReactionIndex` | Two reactions of one source event are distinguished by their position in the `react` output. | Nothing value-wise; the zero-based rule lives in the runner's mint, not the value. |
| `IntentKey` | Reaction identity is (saga, source stream, source position, reaction index), deterministic under redelivery. | Two different reactions rendering one key: the escaped canonical rendering is injective, so a duplicate rejection can never be a false positive. |
| `RenderedIntentKey` | The envelope and payload form of a key is one canonical string, compared bytewise and never parsed. | A key compared in a non-canonical rendering (a false mismatch). |
| `Reaction` / `Command` / `EffectRequest` | A reaction is exactly a command arm or an effect request; `react` stays pure. | A third arm (an inline effect) is unrepresentable. |
| `KeyedPayload` | The framework-minted envelope and the rendered intent key travel with the payload to the push. | Nothing structurally; the attach-unchanged rule is the fold's documented obligation. |
| `CommandGroup` | One stream gets one write per batch (ADR 0006), under the single expectation its commands agreed on. | A mixed-expectation group reaching the fold: the runner rejects it first (`ConflictingExpectations`). |
| `IntentGroup` | Intents accumulate on the saga's outbox stream; the framework fixes the write's expectation at `Any`. | A consumer-chosen expectation on the outbox write. |
| `Saga` (trait) | The reaction mapping is pure domain logic: same event, same reactions. | Nothing structurally; purity is the documented rule the runner's redelivery safety rests on. |
| `ReactionFold` (trait) | Erasure happens at push, under the backend's own bound (ADR 0006's erasure contract). | A backend-neutral erased event type imposed on the E1 surface. |
| `BackoffSchedule` | A backoff schedule is a base and a cap and nothing else; it carries no attempt count. | A base above its cap (a schedule whose first wait already exceeds its bound), rejected at construction. |
| `RetryPolicy` | Retryable aborts and indeterminate failures are retried with bounded exponential backoff, then propagated; classification never happens inside the window. | The zero-attempt policy: a policy that never tries is a misconfiguration, rejected at construction. |
| `SagaRunner` | One source event folds into ONE atomic batch; the ack follows the commit, never precedes it. | An ack-before-append flow is unwritable through `step`. |
| `RunnerStep` | A step reports the cursor's movement, nothing more. | A "processed but not acked" success state. |
| `SagaError` | The failure vocabulary: real conflicts surface and retryables stay retryable; an indeterminate failure propagates only after its retry budget. | A redelivery artifact reported as a conflict (the typed `DuplicateIntent` never reaches here). |

## Type-to-business-rule map - outbox module

| Type | One business rule | Invalid state it forbids |
| --- | --- | --- |
| `OUTBOX_CATEGORY` / `INTENT_METADATA_KEY` | The storage contract's two names are framework-owned: the uniqueness index reads exactly them. | A consumer-configurable key name drifting from the index expression. |
| `OutboxEvent` | Intents and outcomes share one stream and one feed; every record carries the intent key in its payload, and `Intent` also carries it in the envelope for the uniqueness index. | An outcome record tripping the uniqueness index (it would, if the key rode its envelope); a parked record with zero attempts (`NonZeroU32`). |
| `RetryBudget` | The per-intent attempt bound is derived durably from the stream's own `Failed` records. | The zero budget. |
| `EffectPort` (trait) | Effects are performed outside the framework, under at-least-once delivery; the port dedupes by the intent key it is handed, never by payload. | A port asked to dedupe without the key: two distinct reactions may carry identical payloads. |
| `ParkedNotice` | The hook learns the parked intent with its durable attempt count and last failure. | A hook fired without the intent's identity; a zero attempt count (`NonZeroU32`). |
| `CompensationHook` (trait) | The hook fires at park time, in the executor process, idempotent under replay. | Nothing structurally; the once-per-park rule lives in the executor's flow. |
| `OutboxExecutor` | The cursor HOLDS inside a retry window; on exhaustion the intent parks and the group advances permanently. | A "skip and continue" state inside the retry window. |
| `ExecutorStep` | A step reports advance, idle, or a held cursor. | A held cursor reported as progress. |
| `ExecutorError` | Port failures are stream facts (`Failed` records), never executor errors; reads and appends fail differently, and the append error's own variants survive (`Append(AppendError<_>)`, so a lock timeout stays retryable). | A port error surfaced as infrastructure failure; an append failure flattened into a read error. |

## The classification rule (design-panel repair, seat 2 finding 3)

The runner's redelivery classification, as the card pins it and the
panel repaired it: retryable aborts (`TransactError::LockTimeout`) and
indeterminate failures (`Backend`) are retried under the runner's
policy and then propagated, never classified. Every other batch abort
sends the runner back to the outbox stream for the minted keys:

- Key present: this source event's reactions already committed, so
  the abort is a redelivery artifact. That abort arrives as the typed
  `DuplicateIntent` when the uniqueness index fired, and as a
  conflict or violated constraint when the original commit moved a
  command arm's stream past the arm's own expectation (the batch
  checks expectations before its inserts reach the index, so a replay
  of a `NoStream` command arm conflicts before the duplicate intent
  is ever seen). Either way the entry acks as a no-op.
- Key absent: a real command-arm failure, surfaced.

The panel's alternative - a batch-contract change giving the
uniqueness rejection precedence over write-side conflicts - was
considered and not taken: the card's own rule text keys the re-read
on any batch abort, and the runner-side rule leaves ADR 0006's
surface and non-saga batch users untouched. Gate A owns the final
call. Gate M's crash table gains an explicit scenario from this
finding: a replay where a command arm carries `NoStream` must ack as
a no-op through the conflict path.

## Batch-API additions (the scope note)

The card's Scope names the saga and outbox modules; its Deliverable -
the reviewed spec - pins the typed `DuplicateIntent` outcome on the E3
batch API. This record reads the Deliverable as governing, and the
board's log records the user's approval of that reading. The batch
surface changes are exactly two additions, both additive:

| Type | One business rule | Invalid state it forbids |
| --- | --- | --- |
| `DuplicateIntent` | A confirmed storage-level intent-key rejection is its own typed outcome, never a generic abort and never a conflict. | A consumer minting a redelivery signal: the constructor is crate-internal, so only a backend (or an in-crate test double) can build the outcome. |
| `BatchSource` (trait) | A handle hands out its own builder, so a generic consumer (the runner) assembles a batch without naming the wire form. | A builder/transact wire-form mismatch: the `Batch = Batch<Wire>` supertrait binding makes them agree. Handle identity - which database a batch commits against - is NOT pinned by the type; that stays the caller's discipline, as on E3's concrete path. |

No existing type or behavior changes. `TransactError` gains a variant,
which is a breaking change for exhaustive downstream matches; the
crate is pre-1.0 under ADR 0001's breaking-change budget.

## Visibility and seams

| Item | Visibility | Reaches into |
| --- | --- | --- |
| `streams::saga` | public module | `streams::batch` (`BatchBuilder`, `StreamRef`, the conflict payloads), `streams::feed` (`EventFeed`, `ConsumerGroup`, `FeedPosition`, `PollLimit`, `AckError`), `streams` root (`BatchSource`, `EventMetadata`, `EventStreams`, `ExpectedVersion`), `streams::outbox` (`OutboxEvent`, `OUTBOX_CATEGORY`) |
| `streams::outbox` | public module | `streams::feed`, `streams::saga` (`RenderedIntentKey`, `RetryPolicy`, `SagaId`), `streams` root (`EventStreams`), `decider::Event` |
| `batch::DuplicateIntent`, `batch::BatchSource` | public, re-exported at the streams root | `StreamRef`, `AtomicStreams`, `BatchBuilder` |
| `BatchSource` impls | `in_memory::batch`, `postgres::batch` | the handles' existing `batch()` constructors |

Neither module reaches into a backend's private parts; both backends
are wired through the public handle traits only.

## Inherited deferrals (gate A owns)

1. In-memory intent-key parity: the in-memory backend accepts
   duplicate intents (E19's documented divergence, pinned by a test);
   whether the backends must reach parity is a gate-A decision.
2. Final outbox category naming: migration 0004's `saga-outbox` is
   indicative by its own comment; a rename is its own migration step.
3. The `load_category` envelope-gap watch item: this design's key
   consumption is feed-only plus `load_stream` (both
   envelope-preserving); `load_category` is never used. Gate A
   confirms rather than discovers.

## Residual risks (named)

- **The fold seam was the panel's primary target.** Four pre-skeleton
  review rounds had pinned reaction identity, classification, and
  parking; the `CommandGroup`/`IntentGroup` push seam was this
  skeleton's proposal. The panel's repairs landed: the port now
  receives the intent key, the executor's error surface splits reads
  from appends, and the classification rule covers the
  expectation-check ordering. The seam's remaining judgment calls
  (group ergonomics, the fold's contract wording) belong to gate A.
- **Key rendering injectivity** rests on the documented escaping rule;
  the golden tests pin it at Layer 2.
- **One runner per saga and one executor group per saga** are
  conventions (group identity = saga id), documented, not enforced.
- **The executor polls the whole outbox category** and skips foreign
  streams: O(sagas x tail) per poll in v1.
- **The runner's retry values** (5 attempts, 50ms base, 2s cap) are
  indicative until gate A.
- **The executor's failure-count derivation reads the whole outbox
  stream** per failing intent; fine at lab scale, a candidate for a
  per-intent index if a consumer's stream grows long.
- **One shared outbox category means one shared effect payload type.**
  A category holds one event type, so every saga on the `saga-outbox`
  category shares `F`: a deployment with several sagas defines one
  consumer-wide effect enum. Documented on `OutboxEvent`; a
  routing/decoding seam is later work if a consumer needs per-saga
  payload types.
- **The intent key exists twice on intent records**: the payload copy
  is the executor's read source, the envelope copy is what the
  uniqueness index reads. The runner mints both from one `IntentKey`;
  a disagreement is possible only through a foreign write, which the
  funnel's operating assumptions already forbid.
- **`RenderedIntentKey` deserialization trusts storage.** Any stored
  string becomes the type at the read boundary (serde transparent);
  a non-canonical stored value would silently never match a fresh
  render. Stored payloads are trusted at the boundary like any stored
  event; gate A confirms this trust boundary explicitly (seat 1,
  finding 6).

## Hole inventory

Baseline at the skeleton commit:

```sh
grep -rn 'todo!(' src/streams/saga.rs src/streams/outbox.rs
```

Four holes: `SagaRunner::step`, `SagaRunner::run`,
`OutboxExecutor::step`, `OutboxExecutor::run`, each carrying its
`#[expect(unused_variables, reason = "todo!() body; filled by E20")]`
marker. The batch.rs additions are complete types and carry no holes;
`DuplicateIntent::new` is crate-internal and carries
`#[expect(dead_code, reason = "constructed by the E20 postgres fill")]`
until the fill constructs it. The module-level `#![allow(dead_code)]`
markers on both modules are removed slice by slice as the holes fill.

Fill order, per the typed-holes practice: the design panel runs on
this skeleton; golden tests from the card's spec land next and fail on
arrival; then fills - runner step, runner classification, executor
step, executor park path - each sweeping its own markers and
accounting for its inventory lines.
