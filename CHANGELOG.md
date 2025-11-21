# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Comprehensive internal documentation structure
  - Architecture and philosophy documentation (docs/internal/planning/epoch-architecture-philosophy.md)
  - Coding style guide with Railway-Oriented Programming patterns (docs/internal/planning/coding-style-guide.md)
  - Documentation guidelines for internal vs external docs (docs/internal/documentation-guidelines.md)
  - Auto-loaded .claude/context.md for LLM sessions
  - Session start checklist (.claude/session-start.md)
- TODO.md for tracking work items and project planning
  - Current Sprint section for active work visibility
  - Priority levels (High, Medium, Low, Backlog)
  - Known Issues tracking
  - Recently Completed history
- CHANGELOG.md following Keep a Changelog format
  - Unreleased section for ongoing work
  - Semantic versioning guidelines
  - Version 1.0.0 release criteria
- TODO and CHANGELOG workflow documentation (docs/internal/todo-changelog-workflow.md)
  - Daily workflow for feature development
  - Weekly workflow for sprint management
  - Release workflow with 10-step process
  - LLM-assisted development patterns
  - Templates for TODO items and CHANGELOG entries
- PostgreSQL repository implementation planning document (docs/internal/planning/postgres-repository-implementation.md)
  - Comprehensive 4-phase implementation plan (8-12 hours total)
  - Database schema design with JSONB event storage
  - Trait implementation patterns following ESDB and Redis
  - Connection pooling strategy with bb8
  - Optimistic concurrency control using sequence numbers
  - Generic spec test integration approach
  - Migration guides from in-memory, ESDB, and Redis backends
  - Railway-Oriented Programming error handling patterns
  - Performance considerations and indexing strategy

### Changed
- Enhanced coding style guide with trucker_buddy_rs patterns
  - Added Railway-Oriented Programming section with visual diagrams
  - Added "Making Illegal States Unrepresentable" principle
  - Added protected concrete types (smart constructors) pattern
  - Added validation helpers for error composition
  - Expanded anti-patterns section
- Updated architecture philosophy documentation
  - Added sections on illegal states and Railway-Oriented Programming
  - Added Scott Wlaschin's principles and influences
  - Enhanced with practical examples
- Updated .claude/context.md with Session Start Protocol
  - Added references to TODO.md and CHANGELOG.md
  - Added core philosophical principles section
  - Enhanced "When Working on Epoch" checklist
- Established NO EMOJIS as strict documentation standard across all files

### Deprecated
- None

### Removed
- None

### Fixed
- None

### Security
- None

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
