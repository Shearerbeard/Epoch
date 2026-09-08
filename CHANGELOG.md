# Changelog

<!-- vale ai-tells.OverusedVocabulary = NO -->
<!-- Reason: "notable changes" is the Keep a Changelog boilerplate's own
     wording, not AI-generated prose. -->
All notable changes to this project will be documented in this file.
<!-- vale ai-tells.OverusedVocabulary = YES -->

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Versioned schema migrations (`PgEventStreams::migrate`): numbered,
  ordered, immutable step files (0001-0005) applied under one
  transaction behind an advisory lock, recorded in an applied ledger,
  idempotent and safe under concurrent callers
- Event feed surface (`EventFeed` trait): at-least-once poll/ack delivery
  over the committed log, per consumer group (ADR 0010)
- PostgreSQL feed implementation (`PgEventFeed`) with per-group cursor
  storage (`epoch_feed_cursors`, migrations 0004/0005)
- Consumer metadata: feed entries deliver each event as a
  `RecordedEvent` carrying its `EventMetadata` envelope, the same keyed
  shape the write path stores

### Changed
- Single-writer funnel: every event transaction (single append and atomic
  batch) now takes one global advisory lock, making insert order commit
  order (ADR 0010 pivot)
- Breaking: new error arms `AppendError::LockTimeout`,
  `TransactError::LockTimeout`, and `PgStreamsError::LockTimeout`; feed
  ack surfaces backend faults via `AckError::Backend`
- Breaking: the envelope rework removed the `EventBatch` slice
  accessors `as_slice()` and `into_vec()`; batch contents are read as
  `RecordedEvent<E>` records via `records()` (consumed via
  `into_records()`), with the domain event at `RecordedEvent::event()`
  and its envelope at `metadata()`
- Breaking: `StreamSlice` holds `RecordedEvent<E>` records instead of
  bare events; `events()` is renamed to `records()` and `into_parts()`
  returns the records, so a slice read surfaces each event's stored
  envelope
- Load paths (`load_stream`, `load_stream_from`) now return each event
  with the envelope its append stored instead of rebuilding every
  record with an empty envelope
- New timeout builders with defaults: `with_lock_timeout` (5s) on
  `PgEventFeed`; `with_lock_timeout` (5s) and
  `with_idle_transaction_timeout` (30s) on the write handles
  `PgEventStreams` and `PgDatabase`
- Deployment assumption: run `migrate()` before any feed or write; the
  funnel requires `CACHE 1` on the backing sequence, no sequence
  rewinds, and no pre-funnel writers during a mixed-version deploy

### Known Issues
- Work in progress: not released, final review pending
- Performance characterization (lab hardware): the writer and feed
  paths are measured - append ~300-405 ops/s depending on caller
  count, atomic batch ~1,524 events/s at 4 callers, feed drain lag
  ~5-6ms, and the single-writer funnel bounds append throughput by
  design; these are shared-lab numbers, not production capacity
  claims

## [1.0.0-alpha.18] - Prior to Documentation

### Added
- Decider pattern traits (Evolver, Decider, DeciderWithContext)
- Repository abstractions (Event and State repositories)
- Versioned repositories with optimistic concurrency
- Strategy patterns (LoadDecideAppend, ReifyDecideSave, StateFromEventRepository)
- In-memory repository implementation
- EventStoreDB backend (feature: esdb)
- Redis backend (feature: redis)
- Generic spec tests for repository implementations
- Example UserDecider with guitar collection domain
- Support for multi-stream repositories
- Snapshot repository patterns
- Retry logic with exponential backoff for version conflicts

### Changed
- Evolved from EventContext pattern to Decider pattern
- Improved error handling with VersionedRepositoryError

### Known Issues
- README.md references old EventContext API instead of Decider pattern
- Retry logic uses thread::sleep instead of tokio::sleep
- LoadDecideAppendWithSnapshot strategy not yet implemented

## Versioning Guidelines

This project uses [Semantic Versioning](https://semver.org/):

- **MAJOR** version: Incompatible API changes
- **MINOR** version: Add functionality in a backwards compatible manner
- **PATCH** version: Backwards compatible bug fixes
- **Alpha/Beta** suffix: Pre-release versions (current: alpha)

### Alpha Status (1.0.0-alpha.x)

During alpha:
- API may change without notice
- Breaking changes increment alpha number
- New features increment alpha number
- Bug fixes increment alpha number

### Version 1.0.0 Criteria

To move from alpha to 1.0.0 stable:
- [ ] API is stable and documented
- [ ] README examples use current Decider pattern
- [ ] All three backends (in-memory, ESDB, Redis) fully tested
- [ ] Comprehensive documentation complete
- [ ] LoadDecideAppendWithSnapshot implemented
- [ ] Migration guide from alpha to 1.0
- [ ] Performance benchmarks established
- [ ] Production usage validated

## How to Update This File

### For Each Change

1. **Add to Unreleased section** under appropriate category:
   - **Added**: New features
   - **Changed**: Changes in existing functionality
   - **Deprecated**: Soon-to-be removed features
   - **Removed**: Removed features
   - **Fixed**: Bug fixes
   - **Security**: Security fixes

2. **Use Present Tense**: "Add feature" not "Added feature"

3. **Include Context**: Link to issues/PRs when relevant

4. **Group Related Changes**: Keep related changes together

### For Each Release

1. **Create Release Section**:
   ```markdown
   ## [X.Y.Z] - YYYY-MM-DD
   ```

2. **Move Unreleased Items**: Move items from Unreleased to new release section

3. **Update Links**: Add comparison links at bottom of file

4. **Tag in Git**:
   ```bash
   git tag -a vX.Y.Z -m "Release version X.Y.Z"
   git push origin vX.Y.Z
   ```

### Examples

#### Good Changelog Entries

```markdown
### Added
- Validation helpers for Railway-Oriented Programming composition
- StreamState enum for explicit state machine transitions
- Generic error parameter for backend flexibility

### Changed
- Improved error messages to include entity IDs for debugging
- Updated retry logic to use exponential backoff with configurable max retries

### Fixed
- Version conflict resolution now correctly handles NoStream case
- Memory leak in in-memory repository when streams are deleted
```

#### Bad Changelog Entries

```markdown
### Added
- Stuff
- Made it better
- Fixed things
```

## References

- [Keep a Changelog](https://keepachangelog.com/en/1.0.0/)
- [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
- [Conventional Commits](https://www.conventionalcommits.org/)

---

[Unreleased]: https://github.com/Shearerbeard/Epoch/compare/v1.0.0-alpha.18...HEAD
[1.0.0-alpha.18]: https://github.com/Shearerbeard/Epoch/releases/tag/v1.0.0-alpha.18
