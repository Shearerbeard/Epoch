# E1 core type surface - design record

Layer-1 typed-holes skeleton for the redesigned core stream surface,
`src/streams.rs`. Panel input: this record, the skeleton commit, and
ADRs 0001-0005. Co-located here rather than beside the module because
`src/streams.rs` is a file module and `docs/design/` is the repo's
annex home.

## Type-to-ADR and business-rule map

| Public item | ADR | Business rule | Invalid state it forbids |
| --- | --- | --- | --- |
| `StreamId` trait | 0004 | A stream id is a two-way contract: render to a storage key and parse back | An id type that can address a stream it cannot round-trip from; category reads returning untyped keys |
| `StreamId for String` | 0004 | `String` keeps working for tests and simple consumers | - (compatibility surface, no constraint) |
| `ExpectedVersion<V>` | 0003 | A writer's append-time assertion about stream state | A load reporting `Any`/`StreamExists`: those variants exist only on the write side |
| `StreamVersion<V>` | 0003 | The store's observed position: `NoStream` or a 1-based sequence | An observation of `Any` or `StreamExists`; a 0-based empty/one-event ambiguity |
| `From<StreamVersion> for ExpectedVersion` | 0003 | An observed position is a valid next-append expectation | Hand-mapping between the vocabularies at every call site |
| `AppendError<V, E>` | 0003 | Append fails as a version conflict or a backend error, nothing else | A conflict whose `actual` claims `Any`/`StreamExists` (payload is `StreamVersion`) |
| `EventStreams<E>` | 0002, 0003, 0004, 0005 | Versioned streams over a typed id; the version check is the concurrency contract | Lifetime/HRTB noise in consumer bounds; `&mut self` implying receiver exclusivity |

## Visibility and seams

- `streams` is a new top-level public module; it touches nothing else.
- Reaches into: `crate::decider::Event` (the `E` bound), `thiserror`,
  `trait_variant`, `std`. No repository code is referenced; existing
  modules and the old trait surface are untouched until the port cards.
- No test-only accessors; no `#[cfg(test)]` surface yet (E2 owns the
  spec tests).
- Hole inventory (grep baseline at skeleton commit): 4 `todo!()` sites,
  all in `src/streams.rs` (`String::stream_key`, `String::parse_key`,
  `From<StreamVersion> for ExpectedVersion::from`; trait methods carry
  no bodies). `clippy::todo` is not enabled: the repo has no clippy
  lint table yet, and enabling it at warn would fail the `-D warnings`
  fill gate; the grep inventory is the tracking route.

## Decisions the panel should weigh

- Card acceptance says behavior stays `todo!()` until the panel passes,
  so the trivial `String` impl and the `From` mapping are held open even
  though the typed-holes practice would land them; they fill first,
  post-panel.
- `load`/`load_from_version` return `Result<_, Self::Error>` (backend
  error only); `AppendError` is scoped to append per the card. Open
  question: does `load_from_version` need a version-mismatch failure
  mode, or is a position simply absent from results?
- `Option<&Self::Id>` for category loads carries over from the old
  surface. Open question: is a category read a distinct method (typed
  ids arriving via `parse_key`) rather than an `Option` parameter?
- No serde derives on `ExpectedVersion`/`StreamVersion` yet; the old
  `RepositoryVersion` derived them. Open question for the panel:
  which backend actually serializes a version?
- `StreamVersion` derives `Ord` (`NoStream < Exact(_)`); the old trait
  required `Version: Ord` and the derive order preserves "empty sorts
  first".

## Residual risks

- The repo-wide `cargo clippy -- -D warnings` acceptance cannot pass:
  18 pre-existing lib warnings exist on the base branch, all outside
  card scope. Evidence: the warning sets on `redesign/bootstrap` and
  `card/e1` are identical; the skeleton adds zero. Logged on the card
  as an explicit divergence.
- `#[trait_variant::make(Send)]` emits a second trait in rustdoc
  (ADR 0002 accepts this).
- The old surface (`RepositoryVersion`, `VersionedEventRepositoryWithStreams`,
  the orphaned traits ADR 0004 deletes) stays in place this card;
  removal belongs to the port/backend cards, so both vocabularies
  coexist in the crate until then.
