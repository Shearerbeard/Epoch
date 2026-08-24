# The event feed's cursor runs on an allocation ledger, not on sequence adjacency

- Status: proposed (accepted at the feed card's gate U; revised at
  that card's gate A, 2026-08-23, with the measured spike numbers and
  the executed pivot to option 3 - supersedes nothing, and discharges
  ADR 0007's deferral by that record's own trigger, the pull having
  happened)
- Date: 2026-08 (drafted 2026-08-23 ahead of implementation, per the
  wave plan's session-order ruling; the gate-S spike is chartered below
  as this record's validation gate)
- Deciders: Mike Shearer

## Context and problem statement

ADR 0007 deferred the event feed until a consumer pulled it. The pull
has happened: chore-lottery shipped its vikunja-sync integration, built
its own in-process delivery loop rather than waiting for epoch, and
then logged a dated demand signal for checkpointed read models over
the committed log (2026-08-19). ADR 0007's deferral is discharged by
its own trigger, and this record owns the semantics that record said
the feed card must define before adopting `global_sequence`: visibility,
gap handling, and reorder handling.

The erratum inside ADR 0007 is the starting problem. The landed schema
implements `global_sequence` as `BIGSERIAL`
(`src/streams/postgres/schema.sql`), which allocates at insert time,
so concurrent transactions can commit in the opposite order of their
sequence values. The column is not commit-ordered as written. A naive
cursor over it silently skips committed events:

```
 1  T_A inserts an event, global_sequence = 100    (not yet visible)
 2  T_B inserts an event, global_sequence = 101    (not yet visible)
 3  T_B commits                                    (101 visible)
 4  feed polls  WHERE global_sequence > 99         (sees only 101)
 5  feed advances its cursor to 101
 6  T_A commits                                    (100 now visible)
 7  feed polls  WHERE global_sequence > 101        (100 never returned)
```

Event 100 is never delivered: a committed event skipped permanently by
a cursor that treated "highest sequence seen" as progress. A rolled
back append is worse - its hole carries no marker at all, so a waiting
cursor cannot distinguish "will appear" from "will never appear".

This reaches beyond the feed. ADR 0006's atomic batch commits
multi-stream invariants as ordinary event rows and assumes the
committed log can be read back as history; the feed is that read
side. Every reaction-shaped consumer built behind the feed - the
saga runner and the outbox executor this wave ships after it -
inherits whatever cursor semantics land here.

## Decision drivers

- The cursor MUST NOT skip a committed event, ever. Every
  gap-tolerance-by-timeout design fails this: a timed-out in-flight
  transaction might still commit.
- The cursor MUST NOT wait on a hole that will never fill. An aborted
  append or a burned sequence value MUST be provably behind the
  cursor, not heuristically assumed to be.
- Delivery MUST be at-least-once over a durable per-group cursor;
  idempotency stays the consumer's documented obligation. Every
  production-grade system surveyed (Axon tracking processors, Fmodel's
  saga manager, EvidentDB streams) puts a persisted checkpoint under
  the reaction loop.
- The event write path MUST stay concurrent. Postgres does not need a
  single-writer funnel for event transactions; ADR 0006's premise
  stands unless measurement says otherwise.
- Adoption MUST be measured before it is forced on consumers: the
  ledger's write amplification gets a spike with a pre-registered
  verdict, not a post-hoc rationalization.
- The mechanism SHOULD reuse shipped machinery - the same sequence
  object, the same advisory-lock discipline, the same atomic batch -
  rather than inventing a second counter or a new coordinator.

## Considered options

1. **Gap-tolerant cursor, no schema change.** Read `global_sequence`
   in order and step past holes after a timeout, on the theory that an
   old gap belongs to a dead transaction. Rejected as unsound: a
   timed-out in-flight gap can be a committed event not yet visible,
   and stepping past it is exactly the silent skip in the figure
   above. There is no timeout value that makes this safe, only one
   that makes the window rare.
2. **Allocation ledger with a `txid_status` reaper.** Record each
   event transaction's txid in its ledger row and let a reaper ask
   postgres whether that transaction is still alive. Rejected as
   MVCC-impossible: the claim that writes the txid is part of the
   event transaction itself, so an uncommitted claim is invisible to
   every other session, and a committed one is already COMMITTED in
   the same transaction - the claimed-but-live state the reaper
   needed to observe can never be seen. The reaper would flip live
   rows or wedge dead ones, and no amount of care fixes an
   unobservable state.
3. **Single-writer transactor (the EvidentDB model).** Funnel every
   append through one serialized writer, so allocation order and
   commit order coincide by construction and the cursor becomes a
   plain maximum. Named fallback, not rejected: it trades the
   concurrency ADR 0006 claimed for the simplest possible cursor. On
   this design's terms it is "serialized everything" where the chosen
   option is "serialized allocation, concurrent event transactions" -
   adjacent designs, and the pre-registered spike below adjudicates
   between exactly these two. The wave's cards use a coarser
   shorthand for that pair - their Option 1 is the ledger, their
   Option 2 the single writer - while this record keeps its own
   four-option numbering; the mapping is stated here once.
4. **Allocation ledger with serialized allocation and a row-lock
   reaper (chosen).** Per-transaction sequence ranges drawn from the
   shared sequence object in a short serialized allocation
   transaction; guarded state transitions; a reaper built on row
   locks rather than transaction introspection.

## Decision outcome

Option 4 at design time; option 3 as shipped. The chartered spike
measured option 4's cost and failed its pre-registered append
threshold, so the pivot this record named in advance executed. The
feed does not read `global_sequence` adjacency with tolerance, and it
no longer reads a ledger either. One serialized writer fronts every
event transaction, so insert order is commit order by construction;
the cursor is a plain maximum over the committed log. The figure's failure mode is closed by construction on the write
side - no event transaction is ever in flight below a committed
value, so a poll reading `WHERE global_sequence > cursor` can neither
skip a committed event nor wait on a value that will never appear.

### The contract, pinned (revised at the feed card's gate A)

- **The single-writer funnel.** One transaction-scoped advisory lock
  fronts every event transaction - single append and atomic batch
  alike. Exactly one writer runs at a time; the version checks and
  head observations inside the funnel are unchanged from ADR 0006.
  The lock's wait is bounded by the same `lock_timeout` discipline,
  so a wedged writer surfaces as the retryable
  `TransactError::LockTimeout`, never a hang and never a conflict.
- **Cursor definition.** The cursor is the highest committed
  `global_sequence` a group has acknowledged. Contiguity is not
  integer adjacency: values burned by rolled-back appends appear in
  no committed row and the cursor passes them without waiting -
  they are not feed sequences. The old BIGSERIAL-era gaps pass the
  same way.
- **Delivery.** At-least-once per consumer group over the committed
  log; a crash between delivery and ack replays, and idempotency is
  the consumer's documented obligation.
- **Ack scope.** Delivery is per-entry and may paginate; a group's
  cursor advances only by whole acks, monotonic and non-regressing.
  An ack at the cursor's own position is a silent no-op; below it is
  rejected; past the group's delivered watermark is rejected - the
  delivered watermark is a separate value from the ack cursor and
  moves on poll.
- **One active poller per group in v1** - the Axon
  tracking-processor model; the cursor row lock is the backstop.
  Concurrent pollers per group are a later, separately chartered
  extension.
- **Group storage.** Per-group progress (ack cursor, delivered
  watermark) persists per (category, group). A dead group's
  reclamation is deleting its row - a restart from any position is
  an operator decision, logged by whoever makes it.
- **The keyed envelope seam.** The write path and the feed's
  delivery carry an opaque event-metadata map; reaction keys ride it
  end to end. The outbox category's intent-key uniqueness index is
  storage-level and landed with the feed's migrations.
- **Latency.** v1 ships polling. The LISTEN/NOTIFY push optimization
  is deferred: the feed trait's poll is pull-shaped, so the
  optimization belongs to a wait-capable poll variant, chartered
  separately when a consumer needs sub-poll-interval latency.

### The validation gate (chartered here, not assumed away)

The ledger's cost is one extra transaction per append - two commits,
three writes (the ledger insert, the event inserts, the COMMITTED
update). This record is not accepted on that cost's reputation. The
feed card's gate S runs a spike against live postgres measuring BOTH
paths - the single append and the E3 multi-stream batch under
contended lock-wait - with the verdict pre-registered against two
thresholds:

- p99 append latency overhead above 2x the unledgered path, at 8
  concurrent writers over 10k appends
- a batch lock-wait p99 regression beyond 50%, under 4 concurrent
  multi-stream batches

Either threshold broken is a FAIL, and the wave moves to the
single-writer fallback (option 3). The pivot's consequences are named
above so nobody invents them under pressure.

**The measured verdict (2026-08-23, the feed card's gate S, live
compose postgres, harness committed as `tests/spike_ledger.rs`):**
FAIL on the append threshold, and the pivot executed. Run of record:
append path, 8 concurrent writers over 10k appends each (80k samples
per path) - unledgered p99 8,960us, ledgered p99 39,814us, ratio
4.444 against the 2.000 limit; batch path, 4 concurrent multi-stream
batches over a shared 8-stream pool - unledgered p99 29,486us,
ledgered p99 31,820us, ratio 1.079, inside the 1.500 limit. An
earlier partial run on cold tables measured the append ratio at
1.598, but its baseline was 3x slower than the warm run while the
ledgered path held steady across both (41.6ms then 39.8ms p99) - the
disagreement was baseline cold-start variance, not ledger variance.
The stable signal is the mechanism's own cost: the allocation
transaction's second commit fsync roughly quadruples steady-state
per-append p50 on this database (3.8ms to 17.3ms). The full numbers
live on the feed card either way; this record now describes the
single-writer design that shipped in its place.

## Consequences

Revised at the feed card's gate A alongside the pivot; the ledger
design's consequences remain as the design-time record above and in
the review ledger below.

- Positive: a committed event cannot be skipped. The funnel makes
  insert order commit order on the write side, so the figure's
  failure mode is unrepresentable rather than defended against.
- Positive: the append path keeps one transaction and gains no new
  machinery to feed: no ledger table, no reaper, and no second
  commit per append. The funnel is one advisory lock statement inside
  the transaction, the same discipline ADR 0006 already pinned.
- Positive: the advisory-lock key space shrinks. One global writer
  key replaces the per-stream two-key tuples; no acquisition order
  needs maintaining because there is nothing to order.
- Negative: writes serialize completely. Every event transaction,
  whatever its streams, waits on one lock; throughput is bounded by
  one writer's commit latency. The spike priced the alternative at
  4.4x p99 per append and the trade was taken with eyes open - a
  single-postgres deployment's append throughput is now structurally
  serial, and any future that needs concurrent writers must reopen
  this record.
- Negative: a wedged writer stalls all writes until its
  `lock_timeout` expires. The retryable LockTimeout outcome is the
  contract for riding that out; consumers must treat it as retry,
  which ADR 0006 already required.
- Deferred: the LISTEN/NOTIFY latency optimization. The shipped poll
  is pull-shaped; a wait-capable poll variant owns the optimization
  when a consumer needs it.
- Open, each owned: final naming (the saga-outbox stream category
  and its intent-key index are indicative until the runner card's
  gate A). The outbox-saga section of this record - reaction format,
  intent identity, the executor's retry and compensation contract -
  is the runner card's gate-A deliverable and lands as a revision
  here.

## Links

- [ADR 0007 - the deferral and the BIGSERIAL erratum](0007-saga-and-feed-deferred-until-pulled.md)
- [ADR 0006 - the atomic batch whose read side this is](0006-atomic-multi-stream-batch.md)
- [Saga delivery semantics - the upstream survey](../research/saga-delivery-upstream.md)
  (Axon token stores, one poller per group, the outbox school)
- The BIGSERIAL column and the write paths the funnel now fronts:
  `src/streams/postgres/migrations/0001-create-stream-events.sql`,
  `src/streams/postgres.rs`, `src/streams/postgres/batch.rs`,
  and the feed surface `src/streams/feed.rs`
- The race figure's original:
  [epoch-saga-ledger-infographic](https://shearerbeard.github.io/artifacts/epoch-saga-ledger-infographic)
  (rev 2, 2026-08-22)
- The maintainer's private redesign board holds the wave plan, the
  feed and runner cards, and the raw adversarial-review packets
  summarized in the ledger below; this public record is
  self-contained on purpose.

## Design provenance and adversarial review ledger

The design below this line was adversarially reviewed four times on
2026-08-22, before this record was drafted; the findings shaped it
directly and are recorded here with their dispositions. Authoring
model throughout: GLM-5.3 (the board owner's class).

1. Round 1 (kimi-k3 via the opencode route, fresh context): FAIL, 5
   blocking, 9 minor. Applied: orphaned IN_FLIGHT
   rows need an author for ABORTED (the client rollback path and a
   reaper, with guarded transitions - without the guard, the reaper
   reintroduces the silent skip); sequence issuance pinned to one
   counter with explicit inserts; the spike given a pre-registered
   numeric verdict and the batch path added to its scope; "skippable
   exactly once" defined.
2. Round 2 (same reviewer route, fresh context): FAIL, 1 blocking,
   7 minor. Values burned by an aborted ALLOCATION transaction had
   no place in the contiguity rule - disposition: the cursor is
   defined over ledger ranges, and burned values are not feed
   sequences; the allocation-abort scenario joined the live proofs.
   Also pinned: the stored txid is the event transaction's via a
   first-statement claim; the reaper runs in the poll path.
3. Round 3 (same reviewer route, fresh context, three-attempt cap):
   FAIL, 1 blocking, 4 minor. Un-serialized allocation commits could
   reorder, making the burn rule unsound - the silent skip
   reintroduced one layer up. Disposition: the allocation critical
   section is serialized under one advisory lock; visibility order
   equals `first_seq` order; the allocation-reorder scenario joined
   the live proofs; and the two spike candidates were renamed to
   their honest shapes - "serialized everything" vs "serialized
   allocation, concurrent event transactions" - so the spike
   adjudicates between named designs, not slogans.
4. Final cross-family review (the codex route, fresh context):
   confirmed the serialization sound, then FAIL, 3 blocking,
   3 minor. Dispositions: the `txid_status` reaper replaced by the
   row-lock
   protocol above (MVCC makes the claimed-live state unobservable);
   the keyed envelope seam pinned on both backends; ack scope
   defined (per-event delivery, whole-range ack, one poller per
   group in v1).
5. The user approved the ledger design conditionally - on confidence
   that the record could be produced from a cold start - and the
   condition was audited and closed the same day against a section-
   by-section input map naming every source this record drew from.
6. This record's own text, 2026-08-23, before commit: a fresh-context
   reviewer from a different family (DeepSeek V4 Pro via the opencode
   route; author GLM-5.3). Verdict: PASS, 2 MINOR. Dispositions:
   (1) this record's local option numbering diverged from the cards'
   spike shorthand - fixed by stating the mapping once, at option 3
   above; (2) the per-round minor-finding counts were recorded
   unevenly - fixed by counting uniformly from the round packets.
   The reviewer's fidelity, premise-freshness, and validation-gate
   checks passed; the `.rs` write-path links were taken on the round
   packets' evidence, as the source files were not staged for it.
7. The gate-A revision, 2026-08-23: the spike's FAIL verdict and the
   pivot it triggered, applied to this record by the feed card's
   board owner (GLM-5.3) as the revision this record itself chartered
   at drafting. The design-panel and gate reviews of the shipped
   surface live on the feed card's record; this entry closes the loop
   between the chartered validation gate and the decision it decided.
