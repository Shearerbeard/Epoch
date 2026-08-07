# Can other stores carry the AtomicStreams model?

Side-quest research, 2026-08-07, run at the maintainer's direction
alongside the ADR 0006 acceptance. Question: which other backends
could implement the accepted `AtomicStreams::transact` contract -
multi-stream atomic writes, per-write `ExpectedVersion` against the
pre-batch head, constraints over unwritten streams, bounded blocking,
deadlock-free ordering. Two takes were produced independently and
then merged. The first is the board owner's (Claude), with live web
verification. The second is Kimi K3's, on the OpenCode fireworks
route; it was briefed to answer from model knowledge but verified the
volatile claims against current docs itself. The comparison section
at the end records where the takes diverged and which caught what.

## EventStoreDB / KurrentDB

Verdict: IMPLEMENTABLE, fully, on KurrentDB 26.1+; write-side only on
25.1-26.0; not at all on EventStoreDB classic. The ADR's "ESDB simply
does not implement it" premise was already dated on the day the
record was accepted.

The timeline, web-verified in both takes:

- KurrentDB 25.1 (GA 2025-10) shipped `multiStreamAppend`: atomic
  appends to several streams in one operation, an expected version
  per stream via the `StreamState` vocabulary (`any` / `noStream` /
  `streamExists` / `streamRevision(n)`), and each stream admitted at
  most once per request - the contract's one-write-per-stream rule
  verbatim. Expected state attaches only to written streams, so
  constraints over unwritten streams are not expressible: write-side
  PARTIAL.
- KurrentDB 26.1 (GA 2026-04) shipped `appendRecords`: atomic
  multi-stream writes with interleaved order preserved in the global
  log, plus decoupled consistency checks that validate the state of
  any stream, including streams not being written. All checks
  evaluate atomically; any failure rejects the whole operation with
  an exception listing every failing check against its observed
  state - the same expected-versus-actual shape the ADR pins. That is
  the full contract: writes, per-stream expectations, constraints
  over unwritten streams, and one write per stream.
- Blocking is settled vacuously: checks and commit are server-side,
  no client-held locks exist, and contention surfaces as an
  immediately retryable check violation. Deadlock-free by
  construction - a stronger property than the postgres shape's
  bounded waiting, at no cost.
- Version mapping is clean for an append-only consumer: KurrentDB
  revisions are 0-based, so `StreamSequence n` maps to
  `streamRevision(n - 1)`; `noStream` and `streamExists` map onto the
  constraint vocabulary directly.

For completeness, ESDB-classic offers no route: the deprecated TCP
transactions API was single-stream and died with the TCP protocol; a
meta-stream design (one event describing the cross-stream fact,
projected outward) is atomic only in the meta stream while the real
streams go eventually consistent - ADR 0006's option 5 wearing a
different hat, plus projection lag - and projections' `emit` is
asynchronous, never atomic with the source write.

Open items to verify before any KurrentDB card is cut:

- Whether the consistency-check API accepts a revision pin
  (`StreamAt(n)`) on an unwritten stream or only existence checks;
  the client docs' published example shows only `streamExists`.
  Check the 26.1 gRPC proto.
- Rust client coverage: a maintained `kurrentdb` crate exists and
  Python v1.3 exposes the multi-append surface, but confirm
  `appendRecords` landed in the Rust client; worst case is raw gRPC.
- License tier, Kurrent Cloud gating, and any server config flag;
  the release notes name none.

Long-term note: the maintainer was comfortable dropping ESDB or
leaving `AtomicStreams` unimplemented for it. The vendor closed the
capability gap from their side between the research era and the ADR's
acceptance; the trait seam absorbs that gracefully (a KurrentDB
backend can now implement the trait), but the ADR's ESDB statements
deserve a version qualifier whenever the record is next revised.

## Redis / Redis Streams

Verdict: IMPLEMENTABLE on a single node or one hash slot; PARTIAL in
cluster mode; durability caveats either way.

The mechanism is not postgres-shaped locking - it is EvidentDB-shaped
funneling. One server-side Lua script per `transact` executes
atomically on redis's single command thread: evaluate every
constraint and every per-write expectation against current heads
first, and only if all pass, `XADD` everything. Checks precede any
write, so no rollback machinery is needed - redis's own docs note
that MULTI/EXEC has no rollback at all, so a script rather
than a transaction is the right unit. The single-threaded executor is
the global writer funnel EvidentDB buys with its Datomic-style single
writer; redis gives it away for free, at the cost that every batch on
the instance serializes, disjoint streams included - the parallelism
property of the postgres shape is given up. Blocking is trivially
bounded (scripts run to completion; `lua-time-limit` governs when
other clients see BUSY) and deadlock is structurally impossible.

Version semantics need one decision, and K3's take sharpened it:
`XADD` auto-ids are timestamp-sequence pairs, not 1-based sequences.
The correct mapping is explicit synthetic ids (`N-0`, with N the
1-based sequence - redis requires only that ids increase), which
makes the stream's last-entry id the head and `StreamAt(n)` a parse
of it. The tempting alternative, `XLEN` as the head, breaks under
`MAXLEN` trimming and `XDEL`. Append parity then means single appends
route through the same script, which is also what enforces the
synthetic-id discipline. The old prototype's actual choice should be
dug out of its source before this is carded.

Where it falls short of the contract:

- Cluster mode: every key one script touches must share a hash slot,
  else CROSSSLOT. Hash-tagging all epoch keys into one slot preserves
  correctness and abandons horizontal write scaling; per-category
  tags restore scaling and structurally forbid cross-category
  batches. Any future redis card states that trade on its face.
- Durability: default AOF-everysec plus async replication means an
  acknowledged batch can vanish on failover; `WAIT`/`WAITAOF` and
  `appendfsync always` narrow the window at the latency cost that
  makes redis attractive in the first place.
- Version regression hazard: `XDEL` of a head entry moves the last id
  backwards, so the contract holds only under an append-only
  discipline the backend itself enforces.

## ClickHouse

Verdict: NOT IMPLEMENTABLE as the write model - and the maintainer's
suspicion that it "would lock the same way postgres does" is the
inverse of the actual situation. ClickHouse has no advisory locks, no
client-facing lock protocol, no conditional insert, and no unique
index that could backstop a duplicate position.

What it actually guarantees, verified against the current
transactional-support docs in both takes: an `INSERT` is atomic only
as one data part into one partition of one MergeTree table (an insert
spanning partitions commits per partition; block-size thresholds
split large inserts into independently committing parts). Nothing is
atomic across tables. The experimental transactions work
(`allow_experimental_transactions`, MVCC snapshots tracked in Keeper)
is still experimental, non-replicated-MergeTree only - excluding the
replicated engines production actually runs - absent from ClickHouse
Cloud, and even where it works it provides snapshot isolation without
write-write conflict detection: two concurrent batches both observing
head 5 both commit. K3's take added the storage-layer half my take
missed: MergeTree has no unique indexes, so duplicate
(stream, sequence) rows cannot be rejected by storage at all -
ReplacingMergeTree dedups eventually at merge time and never rejects
an insert - meaning the postgres implementation's backstop leg has no
analogue either. All three legs of the postgres shape (advisory
locks, unique index, `lock_timeout`) are missing, not merely
different.

The honest placement: ClickHouse is a read-side target. ADR 0007's
event feed projecting into ClickHouse for analytics is the natural
fit; ClickHouse as the `EventStreams`/`AtomicStreams` system of
record is not, and under the trait seam it would simply not implement
the capability - here permanently, absent an engine roadmap change.
If a future card wants ClickHouse, it is a feed-consumer card, not a
backend card.

## The two takes compared

Agreement was total on verdicts and mechanisms: KurrentDB
implementable (premise stale), redis implementable-with-trades via
the Lua funnel, ClickHouse not implementable and the locking
suspicion inverted. That agreement is what makes the conclusions
trustworthy.

What each caught that the other missed:

- K3 found KurrentDB 26.1's `appendRecords` with decoupled
  consistency checks over unwritten streams - the full contract,
  where the board owner's take had only 25.1's write-side
  `multiStreamAppend` and had flagged unwritten-stream constraints as
  an open question. K3 also pinned the one-stream-once rule in the
  client docs and the 0-based revision mapping.
- K3 supplied the redis synthetic-id mapping and the `XDEL`
  regression hazard where the board owner's take had left the
  version-representation question open against the old prototype;
  and the no-unique-index point on MergeTree that completes the
  ClickHouse argument.
- The board owner's take contributed the framing that carried into
  this document (the EvidentDB-funnel reading of redis, ClickHouse as
  the feed's read side) and the explicit before-carding checklists.
- One method deviation to record: K3 was briefed to answer from
  model knowledge for a clean comparison, and instead verified
  volatile claims against live docs. The deviation made its
  answer better and the comparison less clean; a future two-model
  survey should either grant both sides the web or deny it to both.

Sources: kurrent.io 25.1 release notes and 26.1 release notes;
KurrentDB Java client docs (appending events: `multiStreamAppend`,
`appendRecords`, consistency checks); clickhouse.com
transactional-support documentation; redis.io transactions
documentation. Fetched 2026-08-07 by both takes independently.
