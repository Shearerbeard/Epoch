# append takes &self; the version check, not the receiver, is the concurrency contract

- Status: proposed
- Date: 2026-08
- Deciders: Mike Shearer

## Context and problem statement

`append` takes `&mut self` today, yet no implementation needs exclusive
access: postgres appends through a connection pool and in-memory goes
through `Arc<Mutex>`. The `&mut` receiver forced chore-lottery into
`Clone` bounds on its facade and clone-with-shared-state semantics
subtle enough to cost a review round. The receiver
implies exclusivity is the concurrency story when the optimistic
version check is the actual story; the crate's own race tests prove
correctness does not depend on who holds `&mut`.

Historically, exclusive access also served aggregates that needed
roll-up read models at decide time. That need moved into
`DeciderWithContext`'s `Ctx` parameter (with its staleness rule set by
ADR 0008), so the historical justification is gone.

## Decision drivers

- Sharing a repository across services MUST NOT require `Clone` bounds
  or clone-semantics knowledge in consumers.
- The trait receiver MUST reflect what implementations need, so the
  compiler stops implying a false exclusivity guarantee.
- In-memory sharing semantics MUST be a documented contract on the
  type, testable from the type alone.

## Considered options

1. `&self` on `append`; in-memory holds one `Arc<Mutex<..>>` over the
   whole store, so `Clone` means same store, documented as the
   contract.
2. Keep `&mut self`; restructure in-memory to one shared `Arc` and
   document Clone-is-same-store (the doc-first route consumers first
   suggested).
3. Keep `&mut self` and per-stream shared state; document the subtle
   clone semantics as they are.

## Decision outcome

Option 1. Option 2 is cheaper in isolation, but that ranking assumes
a no-breakage regime; under ADR 0001 every signature is
already changing once, and doc-first would touch them a second time.
Option 3 documents a trap instead of removing it. Consumer-level
serialization decisions (chore-lottery's facade taking `&mut self` to
interlock draws and returns) are unaffected: that is their layer's
choice, made in their code.

## Consequences

- Positive: sharing a store is `&repo`; the facade `Clone` bounds
  and the review confusion they caused disappear.
- Positive: the trait works for any interior-locking backend without
  ceremony.
- Negative: implementations lose the option of lock-free `&mut`
  fast paths. No current or planned backend wants one.
- Negative: a consumer can no longer lean on `&mut` receivers for
  accidental serialization; consumers who need mutual exclusion must
  say so in their own types, as chore-lottery already does.

## Links

- Context-injection successor for roll-up state: ADR 0008
- Receiver shape consumed by ADR 0002's trait definition
