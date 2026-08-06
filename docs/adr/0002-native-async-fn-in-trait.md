# Repository traits use native async fn, dropping async_trait and the lifetime parameter

- Status: proposed
- Date: 2026-08
- Deciders: Mike Shearer

## Context and problem statement

`VersionedEventRepositoryWithStreams<'a, E, Err>` carries a lifetime
parameter that exists only as a 2022-era `async_trait` codegen
workaround (see the comment block in `src/repository/event.rs`). The
cost lands on consumers: every bound becomes
`for<'a> VersionedEventRepositoryWithStreams<'a, ...>` plus
`'a: 'async_trait` clauses. Chore-lottery's coordinator repeats that
HRTB block six times, and the consumer reported it as the highest
API-ergonomics cost per line of consumer code. Rust has supported
async fn in trait definitions natively since 1.75.

## Decision drivers

- Consumer bounds MUST NOT carry a lifetime parameter or HRTB noise.
- Generic consumers that spawn futures MUST retain a `Send` guarantee
  on trait-method futures.
- The crate SHOULD shed dependencies whose reason to exist has expired.

## Considered options

1. Native async fn in trait, with `#[trait_variant::make(Send)]`
   supplying the Send bound for generic consumers.
2. Keep `async_trait`, delete only the `'a` parameter (modern macro
   versions no longer need it).
3. Hand-desugared methods returning `impl Future + Send`.

## Decision outcome

Option 1. Neither consumer uses the repository traits as `dyn`
objects, so the macro's boxed-future dyn-compatibility buys nothing
here. Option 2 was rejected as a half
measure: it keeps the macro attribute on every trait and impl and keeps
boxing every future. Option 3 was rejected because it hand-writes what
`trait_variant` generates, with more room for signature drift. The
`async-trait` dependency is removed; `trait-variant` is added. The
edition bumps to 2024 in the same release.

## Consequences

- Positive: consumer bounds shrink to
  `KS: EventStreams<KidEvent, Id = KidStreamId>` shape; a common
  class of implementer error, mis-transcribed HRTB bounds, disappears.
- Positive: no boxed futures on the hot path.
- Negative: the traits stop being dyn-compatible. Any future need for
  `dyn EventStreams` requires a follow-up decision (an erased wrapper
  type is compatible with this design and would attach without
  reworking callers).
- Negative: `trait_variant` is a new, small dependency; it emits a
  second trait per definition, which shows in rustdoc.

## Links

- Depends on ADR 0001 for the breakage budget
- The trait this reshapes is specified with ADR 0005 (receiver) and
  ADR 0003 (version vocabulary)
