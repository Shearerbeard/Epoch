# Stream ids are a two-way typed contract owned by the repository

- Status: proposed
- Date: 2026-08
- Deciders: Mike Shearer

## Context and problem statement

Every repository implementation pins `type StreamId = String`.
Consumers who want typed ids (the `KidStreamId` and `ChoreStreamId`
types in chore-lottery, a chore-assignment application built on this
crate) must keep newtypes in a private module, convert with
`into_string()` at every call, and still carry
`StreamId = String` in their bounds. Two orphaned traits
(`WithFineGrainedStreamId`, `StreamIdFromEvent`) gesture at this need
without any implementation using them. Separately, chore-lottery bakes
a `kid:`/`chore:` prefix into the string on top of the repository's own
namespacing (`stream_type-id`), producing double-prefixed keys nobody
intended.

## Decision drivers

- A consumer MUST be able to bind `Id = KidStreamId` so a kid id can
  never address a chore stream, with no conversion shim.
- The contract MUST be two-way: render to a storage key and parse back,
  so typed ids survive a round trip through the store and category
  reads can return typed ids.
- The repository MUST own namespacing; consumer id types render only
  their own identity (no double prefix).
- `String` SHOULD keep working for tests and simple consumers via a
  blanket implementation.

## Considered options

1. A first-class `StreamId` trait in Epoch: `stream_key()` plus a
   fallible `parse_key()`, blanket-implemented for `String`;
   implementations become generic over `S: StreamId`.
2. Bound the implementations on `AsRef<str>` only (render-only).
3. Generic implementations with an `S = String` default type parameter
   to avoid consumer churn.

## Decision outcome

Option 1, absorbing both orphaned traits: `WithFineGrainedStreamId` is
deleted, and `StreamIdFromEvent`'s job (deriving an id from an event's
entity id) becomes a `From<E::EntityId>` recommendation on implementing
types. Option 2 was rejected because a one-way contract cannot return
typed ids from loads, which the second decision driver requires.
Option 3 was rejected under ADR 0001: the default parameter is a hedge
for absent users.

## Consequences

- Positive: chore-lottery deletes its stream-id conversion shim; the
  type system enforces stream addressing.
- Positive: stored keys become clean (`chore_lottery_kid-<uuid>`), with
  one namespacing authority.
- Negative: `parse_key` makes key format part of the public contract;
  changing the storage key layout later becomes a breaking change.
- Negative: existing stored stream names with consumer prefixes do not
  round-trip; chore-lottery's stores are re-seeded during its port,
  which is acceptable for that application's data.

## Links

- Consumed by ADR 0006 (batch writes take `impl StreamId`)
- Implemented by the consumer ports (chore-lottery, trucker_buddy_rs)
