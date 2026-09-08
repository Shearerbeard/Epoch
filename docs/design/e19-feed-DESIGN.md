# E19 feed surface - design record (post-pivot)

Status: WIP, under final review. ADR 0010 is revised with the spike's
FAIL verdict and the executed pivot; the contract section of that
record is the semantics this surface implements. The gate-S spike
failed the allocation ledger's pre-registered append threshold
(4.444x p99 vs 2.000x), so the surface implements ADR 0010's named
fallback - the single-writer funnel. Insert order is commit order by
construction, the cursor is a plain maximum over `global_sequence`,
and the ledger, reaper, and prefix machinery do not exist.

Contention, eviction, and recovery are live and tested. The writer's
lock wait and idle-in-transaction session are bounded; active SQL,
commit, and total client duration are not, so there is no
forward-progress guarantee.

## Trait type-to-business-rule map

| Type | One business rule | Invalid state it forbids |
| --- | --- | --- |
| `FeedPosition` | A committed event's place in the log is 1-based and nonzero. | Position 0 ("before the log") masquerading as a real event's place. |
| `FeedCursor` | A group's watermark is the highest position it acknowledged; 0 means nothing yet. | Nothing value-wise; the monotonic rule lives at `ack`, because a cursor value alone cannot know its own history. |
| `DeliveredWatermark` | A group's delivery watermark is the highest position it was delivered; 0 means nothing yet. Distinct from the ack cursor - the two diverge in the at-least-once window. | The ack watermark and the delivery watermark collapsing into one type, so a rejected ack reports the wrong one. |
| `ConsumerGroup` | A group has an identity, and the empty string is not one. | The anonymous group: unaddressable, indistinguishable from a forgotten argument. |
| `PollLimit` | A poll delivers at least one entry when it delivers at all. | The zero-entry poll: a no-op round trip the caller mistakes for "caught up". |
| `FeedEntry` | A delivery carries where (position), who (stream), and what (record with envelope) - one coherent unit. | An envelope delivered without its event's identity, or a position without its stream. |
| `Regression` (payload) | The cursor never moves backwards, and a rejection only exists for a backwards ack. | A regression report whose attempted position is at or above the cursor - unconstructible via `Regression::new`. |
| `Undelivered` (payload) | The cursor never advances past the delivered watermark, and a rejection only exists for an ack past the watermark. | An undelivered-ack report naming a position within delivery - unconstructible via `Undelivered::new`. |
| `AckError::Regression` / `AckError::NotDelivered` | The two protocol violations a caller fixes differently: rewind vs skip-ahead. | Backend faults masquerading as protocol violations (they are `Backend`). |
| `EventFeed` (trait) | Delivery is at-least-once over the committed log per group; ack is monotonic and delivery-bounded. | A feed that could represent exactly-once or regressing semantics. |

## Runtime repair surface (C3/C4 implementations)

| Configuration / error | One business rule | Unbound gap |
| --- | --- | --- |
| `PgEventFeed::with_lock_timeout(default 5s)` | Feed poll and ack lock wait bounded. Writer-funnel lock-wait separate. | Active statement, commit, client timeout, network expiry, pool acquisition unchanged. |
| `PgStreamsError::LockTimeout(Duration)` | Server lock-timeout expiry is retryable (feed poll error). Effective bound reported. | Automatic retry loop not added for ambiguous connection errors. |
| `AckError::Backend` (wrapping `PgStreamsError::LockTimeout`) | Feed ack surfaces lock timeout as a backend error, same as poll. | |
| `PgEventStreams::with_idle_transaction_timeout(default 30s)` `PgDatabase::with_idle_transaction_timeout(default 30s)` | Idle-writer eviction via server GUC `idle_in_transaction_session_timeout`. Writer lock-wait stays a separate 5s bound. | Active SQL exceeding the idle window is not terminated; commit overflow unaffected. |
| Clone preserves timeout configs | Builder-set bounds survive sharing. | Pool acquisition, statement deadlines outside scope. |

All timeout boundaries are configuration defaults; a zero or
sub-millisecond bound floors to 1 ms, a too-large bound fails as a
backend error, poll surfaces `PgStreamsError::LockTimeout(Duration)`,
ack wraps `AckError::Backend`. No automatic retries.

## Visibility and seams

| Item | Visibility | Reaches into |
| --- | --- | --- |
| `streams::feed` | public module, one file | `streams::batch::StreamRef` (the stream address type), `streams::RecordedEvent` (the envelope seam's record) |
| `feed::spec` | public | `streams::spec::under_deadline` (the shared deadline wrapper) |
| Feed impls | per backend: `in_memory::feed`, `postgres::feed` | in-memory root / pg pool + `epoch_feed_cursors` table |

Feed handles are per-category views over a log-global position space:
positions come from the shared `global_sequence`, delivery is scoped to
the handle's category at construction. A group's cursor may advance
past positions of other categories (they were never delivered to this
group); that is legal watermark movement, not a skip.

## Residual risks (named)

- **One-poller contract is documented, not enforced.** v1 assumes one
  active poller per group (Axon tracking-processor model). A violated
  assumption degrades to at-least-once duplication, never cursor
  corruption; a lease-based enforcement is a later, chartered
  extension.
- **Delivered-watermark durability.** `Undelivered` rejection needs
  the group's highest-delivered position (`DeliveredWatermark`);
  both backends persist it: postgres in the cursor row (a poll
  writes it in the same transaction as the entries read), in-memory
  in the root beside the log. The panel confirmed the watermark
  moves on poll, independent of acks.
- **Whole-log decoding.** Feed impls decode entries to `E`; a category
  whose events are not all `E` fails at decode. The per-category
  scoping above is the containment; multi-type categories are the
  consumer's to avoid in v1.
- **In-memory ordering.** The in-memory root appends under one mutex
  (already single-writer by construction); its positions are arrival
  order, which the conformance cases pin as commit order.
- **Intent-key uniqueness is postgres-storage-level only in v1.** The
  partial unique index rejects a duplicate intent in the
  `saga-outbox` category; the in-memory backend accepts duplicate
  intents. ADR 0010 scopes that uniqueness to the storage layer
  deliberately, and whether the backends must reach parity is the
  saga runner card's gate A decision.
- **Forward progress is not bounded.** The lock wait (5s) and idle
  session (30s) are bounded, but active SQL, the commit, and total
  client duration are not. A future `statement_timeout` or server
  transaction deadline (on a compatible version) or a client deadline
  would need cancellation, connection recovery, and a policy for an
  unknown commit outcome.

## WIP status under final review

The three production implementations are complete: the single-writer
funnel (append, atomic batch, feed) with bounded timeouts, the
event-metadata seam, and the per-group cursor table. The trait types
above are the backend-neutral surface; `PgEventFeed`, `PgEventStreams`,
and `PgDatabase` are the postgres runtime implementations.

Measurement is complete: the writer study and the paired feed
comparison ran, and the figures are recorded in ADR 0010. The status
stays WIP under final review; acceptance is still pending.
