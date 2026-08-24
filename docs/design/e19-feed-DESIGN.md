# E19 feed surface - design record (post-pivot)

Status: skeleton, pending the design panel. The gate-S spike failed
the allocation ledger's pre-registered append threshold (4.444x p99
vs 2.000x; numbers on the E19 card), so this surface implements ADR
0010's named fallback: the single-writer funnel. Insert order is
commit order by construction, the cursor is a plain maximum over
`global_sequence`, and the ledger, reaper, and prefix machinery do not
exist. The ADR's decision-outcome section rewrites to match at the
card's gate A.

## Type-to-business-rule map

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

## Visibility and seams

| Item | Visibility | Reaches into |
| --- | --- | --- |
| `streams::feed` | public module, one file | `streams::batch::StreamRef` (the stream address type), `streams::RecordedEvent` (the envelope seam's record) |
| `feed::spec` | public | `streams::spec::under_deadline` (the shared deadline wrapper) |
| Feed impls (fills) | per backend: `in_memory::feed`, `postgres::feed` | in-memory root / pg pool + `epoch_feed_cursors` table |

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
  whether it persists in the cursor row or derives at poll time is a
  fills decision (crash semantics differ). The panel confirmed the watermark
  moves on poll, independent of acks.
- **Whole-log decoding.** Feed impls decode entries to `E`; a category
  whose events are not all `E` fails at decode. The per-category
  scoping above is the containment; multi-type categories are the
  consumer's to avoid in v1.
- **In-memory ordering.** The in-memory root appends under one mutex
  (already single-writer by construction); its positions are arrival
  order, which the conformance cases pin as commit order.

## Hole inventory

Zero `todo!()` holes: the feed module is a pure type surface plus a
trait declaration - every body that exists is a parse or an accessor.
The behavior lives in the per-backend impls (fill units) and in the
conformance cases in `feed/spec.rs`, which are written from the
contract and fail on arrival until the impls exist (layer 2). The
single-writer funnel for the pg write path is fills scope: it changes
no type surface, only which lock `append` and `transact` take.
