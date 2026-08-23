# The event feed's cursor runs on an allocation ledger, not on sequence adjacency

- Status: proposed (accepted at the feed card's gate U, after its gate A
  revises this record with the measured spike numbers; supersedes
  nothing - it discharges ADR 0007's deferral by that record's own
  trigger, the pull having happened)
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

Option 4. The feed does not read `global_sequence` adjacency at all:
it reads a ledger of allocation ranges, and the cursor advances over
resolved ranges in `first_seq` order. Sequence values that appear in
no ledger row - values burned by an aborted allocation, and the
pre-existing gaps of the BIGSERIAL era - are not feed sequences and
the cursor never waits on them. The figure's failure mode is closed
by construction rather than by vigilance: an event cannot become
visible except through a ledger row that was already visible, in
order, before it.

### The contract, pinned

The feed card's implementation and its gates rest on these points;
each is decided here.

- **Ledger row scope.** One ledger row per event transaction, not
  per event: `(first_seq, last_seq, txid, state)`. A single-stream
  append and an E3 multi-stream batch are each one event transaction
  and take one allocation round-trip. The range itself joins the row
  to its events: every event row the transaction writes carries a
  `global_sequence` value from `[first_seq, last_seq]`.
- **One counter.** Ranges are drawn from the SAME sequence object
  that backs `global_sequence` - n `nextval` calls inside the
  allocation transaction, atomic with its commit. A second counter
  would diverge from the column and collide with it; the `setval`
  alternative is rejected because it rewinds visibility (a rewind
  re-issues values whose earlier owners may already be visible). The
  BIGSERIAL column default is retired by migration, `setval` to the
  current max first, and the event insert paths write explicit
  sequence values from the allocated range. The migration runs
  through the versioned-migration mechanism the schema-migration card
  ships; its content (the retirement) is the feed card's.
- **Serialized allocation.** The allocation critical section - draw,
  insert ledger row, commit - runs under one postgres advisory lock.
  Because all allocations serialize, a ledger row with a higher
  `first_seq` became visible strictly after every row with lower
  values committed. That is what makes the burn claim a theorem
  rather than an assumption: if a poll sees a visible row and no row
  covering some lower value, no allocation for that value can still
  be in flight - it must have committed (and be visible, a
  contradiction) or never have landed (the allocation transaction
  aborted before insert, or the value predates the ledger). A gap
  below a visible row is provably burned. Un-serialized allocation
  commits could reorder, reintroducing the silent skip one layer up:
  the cursor would treat a not-yet-committed lower range as burned,
  then deliver its events behind itself.
- **States and guarded transitions.** A row is born IN_FLIGHT with a
  NULL txid. The event transaction's first statement claims the row -
  it records its txid under the same IN_FLIGHT guard, so a reaped row
  rejects the claim before any event is written. COMMITTED is written
  only by the event transaction, as `UPDATE ... WHERE state =
  'IN_FLIGHT'`; zero rows updated aborts the append loudly rather than
  committing events no live row covers. ABORTED is written two ways:
  by the client on a caught rollback, and by the reaper below. No
  other transitions exist, so a resolved row's events are exactly the
  committed ones - reaping can never orphan a committed event. The
  stored txid is diagnostics, not protocol input.
- **The row-lock reaper.** Orphaned IN_FLIGHT rows - the client died
  between allocation and resolution - are reaped by row lock, not by
  transaction introspection: select candidate IN_FLIGHT/NULL rows
  older than the age bound `FOR UPDATE`; taking the lock proves no
  event transaction holds or is entering the claim, because the claim
  UPDATE blocks on the same lock; after a re-check that the state is
  still IN_FLIGHT, flip to ABORTED and commit. A late claimer then
  finds the guarded `WHERE state = 'IN_FLIGHT'` empty and aborts
  loudly. The race is constructive: whichever side takes the lock
  first wins a well-defined outcome. The reaper runs inside the
  feed's poll path, so feed liveness is reaper liveness by
  construction - an orphan is resolved within one age bound of the
  next poll, and a feed nobody polls holds no one hostage.
- **Cursor definition.** The cursor is defined over resolved ledger
  ranges in `first_seq` order. Contiguity means "the next ledger row
  after the cursor's range, resolved" - never integer adjacency of
  `global_sequence`. Delivery is at-least-once over the contiguous
  resolved prefix; redelivery after a crash is the consumer's
  idempotency obligation, as documented.
- **Ack scope.** Delivery is per-event within a resolved range
  (a range may span many events, and pagination inside a range is
  allowed), but a group's cursor advances only by whole-range ack:
  monotonic, non-regressing, and an ack naming a range not yet fully
  delivered to the group is rejected. V1 pins ONE active poller per
  group - the cursor row is locked for the poll's duration, the Axon
  tracking-processor model; concurrent pollers per group are a later,
  separately chartered extension.
- **Aborted ranges are skippable exactly once.** A resolved ABORTED
  range advances the cursor past itself in exactly one poll - never
  re-delivered, never stalled on.
- **Ledger GC and dead groups.** Terminal rows are deleted only
  behind `min(all group cursors)`; IN_FLIGHT rows are never GC'd,
  only reaped. A consumer group with no ack activity beyond a
  configured horizon is excluded from the min() by an
  operator-invoked, logged reclaim - never silently.
- **The keyed envelope seam.** The streams write path and the feed's
  delivery carry an opaque event-metadata map, so reaction keys ride
  events end to end for the runner/outbox cards that consume them.
  The shipped event interfaces widen for this as feed-card scope, and
  the widening is implemented on BOTH backends before the schema
  lands, so the in-memory and generic paths enforce the same
  protocol. The intent-key uniqueness constraint on the outbox stream
  category and the metadata column itself are feed-card schema
  deliverables.
- **Latency.** The feed keeps the LISTEN/NOTIFY push optimization
  with polling as the degradation path; the reaper rides the poll
  path either way.

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
now so nobody invents them under pressure:
the feed keeps its trait, consumer groups, and cursor semantics; the
ledger and prefix machinery drop; the runner card is unchanged except
that this record's cursor section rewrites; the migration card is
unchanged. The numbers do not exist yet as of this draft - they land
on the feed card either way, and this record is revised with them at
its gate A before anything is accepted.

## Consequences

- Positive: a committed event cannot be skipped. The burn theorem
  plus the guarded transitions make the figure's failure mode
  unrepresentable rather than merely defended against.
- Positive: every delivery guarantee is expressible in streams - the
  ledger is itself an auditable log of attempted event transactions,
  including the aborted ones, which no adjacency cursor offers.
- Positive: no new transactional machinery. The event transaction is
  an ordinary append or E3 batch; the allocation is a short
  transaction over an existing sequence object and the advisory-lock
  discipline ADR 0006 already pinned.
- Negative: one extra transaction per append (two commits, three
  writes). Unmeasured in this draft; the spike measures it and gate A
  cites the numbers here, or option 3 takes over per the
  pre-registered verdict.
- Negative: the allocation critical section is a funnel - all
  writers serialize across it, briefly, where before they did not
  serialize at all. This is the honest shape of the design: the
  concurrency question moved from "everywhere" to "one short
  section", and the spike prices exactly that.
- Negative: the write path is rewritten. `append` and `transact` draw
  ranges and insert explicit sequence values; the column default
  retires via migration. Consumers see no surface change, but the
  backend's failure modes grow by one - the allocation can commit
  and the append then fail, leaving a durable ABORTED marker and a
  loud error rather than a silent hole.
- Negative: feed liveness now owns reaper liveness. A feed nobody
  polls leaves orphans unresolved until polling resumes; resolution
  is bounded by one age bound after the next poll, not by a
  background daemon. Operators who need unconditional liveness run a
  poller.
- Open at gate A, each owned: the age bound's value; the advisory
  lock key space; final naming (the saga-outbox stream category, the
  Reaction variants); the in-memory backend's reaper analogue. The
  outbox-saga section of this record - reaction format, intent
  identity, the executor's retry and compensation contract - is the
  runner card's gate-A deliverable and lands as a revision here.

## Links

- [ADR 0007 - the deferral and the BIGSERIAL erratum](0007-saga-and-feed-deferred-until-pulled.md)
- [ADR 0006 - the atomic batch whose read side this is](0006-atomic-multi-stream-batch.md)
- [Saga delivery semantics - the upstream survey](../research/saga-delivery-upstream.md)
  (Axon token stores, one poller per group, the outbox school)
- The BIGSERIAL column and the write paths the rewrite touches:
  `src/streams/postgres/schema.sql`, `src/streams/postgres.rs`,
  `src/streams/postgres/batch.rs`
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
