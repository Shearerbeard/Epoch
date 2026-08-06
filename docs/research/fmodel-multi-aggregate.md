<!-- vale off -->
<!-- Archived research report, kept near-verbatim (quotes maintainer
     statements and upstream docs); style lint suppressed on purpose. -->

# Fmodel and multi-aggregate decisions — research report

Research pass for the 2026-08 redesign (feeds ADR 0006 and 0007).
Sources: Rust port [fraktalio/fmodel-rust](https://github.com/fraktalio/fmodel-rust)
(src/decider.rs, saga.rs, aggregate.rs, saga_manager.rs), Kotlin original
[fraktalio/fmodel](https://github.com/fraktalio/fmodel), demo
[fraktalio/fmodel-rust-demo](https://github.com/fraktalio/fmodel-rust-demo),
crate docs [docs.rs/fmodel-rust](https://docs.rs/fmodel-rust/latest/fmodel_rust/),
and maintainer discussions
[#37 "Scaling beyond two aggregates"](https://github.com/fraktalio/fmodel/discussions/37)
and [#334 "Stateful Saga or event-handling aggregate"](https://github.com/fraktalio/fmodel/discussions/334).

## 1. Core algebra

Rust port (`src/decider.rs`, `src/saga.rs`):

```rust
pub struct Decider<'a, C: 'a, S: 'a, E: 'a, Error: 'a = ()> {
    pub decide: DecideFunction<'a, C, S, E, Error>,   // Fn(&C, &S) -> Result<Vec<E>, Error>
    pub evolve: EvolveFunction<'a, S, E>,             // Fn(&S, &E) -> S
    pub initial_state: InitialStateFunction<'a, S>,
}

pub struct View<'a, S: 'a, E: 'a> {
    pub evolve: EvolveFunction<'a, S, E>,
    pub initial_state: InitialStateFunction<'a, S>,
}

pub struct Saga<'a, AR: 'a, A: 'a> {
    pub react: ReactFunction<'a, AR, A>,              // Fn(&AR) -> Vec<A>
}
```

Kotlin originals for comparison:
`Decider<in C, S, E>(decide: (C, S) -> Flow<E>, evolve: (S, E) -> S, initialState: S)`;
`View<S, in E>(evolve, initialState)`; `Saga<AR, A>(react: (AR) -> Flow<A>)`.
Decider is a pure command→events function over folded state; View is Decider
minus `decide`; Saga is a pure, **stateless** event→commands mapping. All
three are domain-layer values with no I/O; the application layer glues them
to repositories/publishers.

## 2. `combine` on deciders — the monoid

Rust signature (`decider.rs`):

```rust
pub fn combine<C2, S2, E2>(self, decider2: Decider<'a, C2, S2, E2, Error>)
    -> Decider<'a, Sum<C, C2>, (S, S2), Sum<E, E2>, Error>
where S: Clone, S2: Clone
```

State becomes the **tuple** `(S, S2)`; commands and events become
`Sum<T1, T2>` (`First`/`Second`). `decide` routes on the Sum variant to one
inner decider and lifts its events back into `Sum`; `evolve` updates only
the matching tuple component. In discussion #37 Dugalic describes `combine`
as an associative semigroup operation ("one could compare this operation to
`add`ing Numbers in math"), with `Decider<Nothing, Unit, Nothing>` as
identity — a formal **monoid**, and the operation is documented as
commutative up to isomorphism. He recommends `dimapOnState` to map nested
tuples/Pairs onto a real domain state type rather than exposing `Pair`.

**Consistency semantics of a combined decider:** the combined decider is
just one bigger pure function, so whatever aggregate wraps it handles a
command in **one `handle` call: one fetch, one decide, one save**. If the
`EventRepository` implementation writes all produced events in one DB
transaction (as the demo does — see section 4), a combined decider gives
you **strong, single-transaction consistency across both former
aggregates**. That is precisely Dugalic's stated criterion in #37: combine
only "deciders that have some invariant/relationship"; deciders without
shared invariants should stay separate aggregates connected by a Saga and
"deploy them separately to achieve better scalability." So: combine =
strong consistency, saga = eventual — that's the design axis Fmodel gives
you. (Fraktalio's newer TypeScript material on `handleBatch` makes the same
point explicitly: when commands share an event store, a batch executes "in
a single atomic commit… no compensating transactions needed" —
[fmodel-decider](https://github.com/fraktalio/fmodel-decider).)

Note the stream question is *not* answered by the library: `combine` is
domain-layer. Whether the combined events land in one stream or two is
entirely the `EventRepository`'s business (see section 4).

## 3. Saga

Kotlin: `react: (AR) -> Flow<A>`; Rust: `react: Fn(&AR) -> Vec<A>`. It
"represents the central point of control, deciding what to execute next
(`A`), based on the action result (`AR`)" — in practice AR = an event from
aggregate X, A = a command for aggregate Y. Rust composition: `merge`
combines two sagas over the **same** AR into `Saga<'a, AR, Sum<A2, A>>`
(plus `merge3`..`merge6`); the old `combine` (Sum over both AR and A) is
deprecated since 0.8.0 so that "all your sagas can subscribe to all
`Event`/`E` in the system."

Sagas are deliberately dumb and stateless. From #334: "There is no
important state in Saga, all important stuff is in Decider. Logic does not
leak to Saga. Saga is a dumb pipe." **Retries/at-least-once do not live in
the Saga or SagaManager at all** — they live in the event-streaming
infrastructure feeding it. Dugalic: "AtLeastOnce is a reality!", pointing
at the event store's streaming/delivery guarantees. In fmodel-rust-demo
this is `fstore-sql`: a Postgres poll-based stream per subscriber with a
locks table, `ack_event` advancing `last_offset` on success and
`nack_event` leaving it in place on failure (redelivery ⇒ at-least-once;
commands must be idempotent downstream).

## 4. Application layer persistence

`EventSourcedAggregate` (Rust `aggregate.rs`) = `Decider` +
`EventRepository`:

```rust
trait EventRepository<C, E, Version, Error> {
    async fn fetch_events(&self, command: &C) -> Result<Vec<(E, Version)>, Error>;
    async fn save(&self, events: &[E]) -> Result<Vec<(E, Version)>, Error>;
    async fn version_provider(&self, event: &E) -> Result<Option<Version>, Error>;
    // "Optimistic locking is using this version to check if the event is already saved."
}
```

`handle`: fetch_events(command) → fold via evolve → decide → save(new
events). `StateStoredAggregate`/`StateRepository` is the state-stored twin.
`SagaManager` = `Saga` + `ActionPublisher<A, Error>`
(`publish(&self, &[A]) -> Result<Vec<A>, Error>`); its
`handle(action_result)` computes actions and publishes — no persistence, no
retry logic of its own.

**Streams and atomicity in practice** (fmodel-rust-demo,
`src/adapter/repository/event_repository.rs` + `database/queries.rs`):
events go into one append-only table keyed by `decider_id`
(per-aggregate-instance stream) with a per-stream `previous_id` version
chain for optimistic locking (per-decider latest versions tracked in a
`HashMap<String, Uuid>`). `append_events` opens **one sqlx transaction,
inserts every event, commits once** — so even events belonging to different
decider_ids/streams produced by one command flow are committed atomically.
The demo notably does **not** deploy the combined decider: it wires
`OrderAggregate` and `RestaurantAggregate` as separate
`EventSourcedAggregate`s plus
`OrderSagaManager: SagaManager<OrderCommand, RestaurantEvent, …>`
(RestaurantEvent → OrderCommand), i.e. eventual consistency via the
fstore-sql stream. The Kotlin demos show the opposite wiring too — one
combined decider behind a single aggregate — and since Kotlin's
`EventSourcingAggregate` also does fetch/decide/save in one call, a
same-database repository gives one transaction there as well.

## 5. Fraktalio guidance on cross-aggregate invariants

- **Rule of thumb (discussion #37):** combine deciders **only when they
  share an invariant/relationship**; the combined decider is then one
  consistency boundary handled in a single transaction. Independent
  deciders stay separate aggregates, connected by Sagas, "deployed
  separately" — eventual consistency accepted for better scaling.
- **Process managers (stateful sagas) are intentionally absent** (#334):
  Fmodel removed them — "modeling workflows and processes…
  Decider/Aggregate is the component that should be used for that." If a
  cross-aggregate process needs state (e.g., a reservation workflow), model
  it as its own Decider/Aggregate; the Saga stays a stateless pipe that
  subscribes to events and issues commands. Dugalic sketched a formal
  `Automation<C, E, S, A, AR>` type (evolve/ack/pendingActions/
  pendingActionResults/initialState) for Event Modeling's Automation
  pattern — pending actions accumulate in a View-like state that is polled.
- **Delivery**: cross-aggregate flows are at-least-once by assumption;
  ordering and delivery guarantees are delegated to the event store's
  streaming layer (fstore-sql ack/nack offsets, or a real event store), not
  the domain types.
- No dedicated Fraktalio doc on unique-id/set-validation specifically was
  found; the applicable guidance is the above dichotomy: shared invariant ⇒
  same decider (possibly via `combine`, one transaction); otherwise ⇒
  saga/automation + eventual consistency, with the newer batch-execution
  work ([fmodel-decider](https://github.com/fraktalio/fmodel-decider),
  TypeScript) as a third option that runs an ordered command batch against
  a shared event store in one atomic commit, with earlier commands' events
  visible to later ones.

**Consistency model per mechanism, summarized:** `Decider.combine` → one
decide/save cycle; atomic iff the repository commits in one transaction
(demo does; streams may still be per-decider_id). `Saga` + `SagaManager` →
eventual, at-least-once, stateless routing; retries owned by the
event-streaming adapter. Stateful process → its own Decider/Aggregate
(strong within itself), fed and drained by dumb sagas.
