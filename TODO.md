# Epoch TODO

> **Project Work Items and Planning**
> This file tracks current work items, planned features, and known issues for the Epoch project.

**Last Updated**: 2025-11-21

---

## Current Sprint

### In Progress
- [ ] None currently

### Ready to Start
- [ ] Update README.md to use Decider pattern (not EventContext)
- [ ] Create examples directory with compilable examples

---

## High Priority

### Documentation
- [ ] Update README.md examples to current Decider pattern API
- [ ] Add migration guide from alpha to 1.0.0
- [ ] Create examples directory with full working examples
- [ ] Add API documentation examples to all public traits
- [ ] Document each backend's specific features and limitations

### Code Quality
- [ ] Replace thread::sleep with tokio::sleep in retry logic
- [ ] Add clippy configuration for project-specific lints
- [ ] Add CI/CD pipeline (GitHub Actions)
- [ ] Add code coverage reporting

### Features
- [ ] Implement LoadDecideAppendWithSnapshot strategy
- [ ] Add projection pattern for read models
- [ ] Add event upcasting support for schema evolution
- [ ] Add saga pattern support for cross-aggregate workflows

---

## Medium Priority

### Testing
- [ ] Add integration tests for all three backends
- [ ] Add property-based tests for Decider invariants
- [ ] Add benchmarks for repository performance
- [ ] Add concurrency tests for version conflict resolution

### Developer Experience
- [ ] Create cargo-generate template for new Decider implementations
- [ ] Add detailed error messages with suggestions
- [ ] Create debug logging framework
- [ ] Add tracing support for observability

### Documentation
- [ ] Add architecture decision records (ADRs)
- [ ] Create video/tutorial series
- [ ] Add comparison with other Rust event sourcing frameworks
- [ ] Document common pitfalls and solutions

---

## Low Priority

### Nice to Have
- [ ] Add GraphQL subscription support for event streams
- [ ] Add event transformation utilities
- [ ] Create admin CLI for repository inspection
- [ ] Add metrics and monitoring integration
- [ ] Create web UI for event stream visualization

### Ecosystem
- [ ] Publish to crates.io
- [ ] Create Discord/community chat
- [ ] Set up project website
- [ ] Add OpenTelemetry integration

---

## Known Issues

### Critical
- None currently

### Major
- README.md uses old EventContext API instead of Decider pattern
- thread::sleep used in retry logic instead of tokio::sleep (not async-aware)

### Minor
- LoadDecideAppendWithSnapshot strategy declared but not implemented
- No CI/CD pipeline configured
- Missing CONTRIBUTING.md guidelines

---

## Ideas / Research

### Future Exploration
- Investigate async trait stabilization impact
- Research zero-copy deserialization for events
- Explore compile-time event schema validation
- Consider proc macros for reducing boilerplate
- Investigate alternative concurrency control (MVCC vs optimistic locking)

### Performance
- Benchmark different serialization formats (bincode, messagepack, etc.)
- Investigate event batching strategies
- Research snapshot frequency optimization
- Profile memory usage in long-running applications

---

## Backlog

### Deferred
- WebAssembly support for repositories
- Support for additional backends (PostgreSQL, DynamoDB, etc.)
- Event encryption at rest
- Multi-region replication patterns
- Time-travel debugging support

---

## Completed

### Recently Completed
- [x] Create comprehensive internal documentation structure
- [x] Add .claude/context.md for LLM-assisted development
- [x] Document Railway-Oriented Programming patterns
- [x] Add "Making Illegal States Unrepresentable" guide
- [x] Create coding style guide with anti-patterns
- [x] Add CHANGELOG.md
- [x] Add TODO.md (this file)

---

## How to Use This File

### Adding New Items

1. **Choose the right section**:
   - **Current Sprint**: Active work this week/sprint
   - **High Priority**: Next up, blocks other work
   - **Medium Priority**: Important but not urgent
   - **Low Priority**: Nice to have
   - **Known Issues**: Bugs or problems
   - **Ideas / Research**: Needs investigation
   - **Backlog**: Deferred for later

2. **Use clear, actionable descriptions**:
   ```markdown
   - [ ] Add validation helpers for Railway-Oriented Programming
   ```

3. **Include context when helpful**:
   ```markdown
   - [ ] Replace thread::sleep with tokio::sleep in retry logic
     - Location: src/strategies/mod.rs
     - Reason: Better async runtime integration
   ```

### Moving Items

- **Started work**: Move from "Ready to Start" to "In Progress"
- **Completed**: Move to "Completed" section and mark with [x]
- **No longer relevant**: Remove or move to Backlog with explanation

### Keeping It Current

- **Update "Last Updated" date** when making changes
- **Review weekly**: Adjust priorities based on project needs
- **Archive old completed items**: Move to separate COMPLETED.md after a month
- **Link to issues/PRs**: Reference GitHub issues when applicable

### For LLM Sessions

When starting a Claude Code session:
1. Review Current Sprint section
2. Check High Priority for next tasks
3. Update as work progresses
4. Mark completed items

### Relation to CHANGELOG

- **TODO.md**: Planning (what we will do)
- **CHANGELOG.md**: History (what we did)

When completing TODO items:
1. Mark as complete in TODO.md
2. Add entry to CHANGELOG.md Unreleased section
3. Move to Completed section after CHANGELOG update

---

## Priority Definitions

- **High**: Blocks other work, affects users, or critical for next release
- **Medium**: Important features or improvements, but not blocking
- **Low**: Nice to have, quality of life improvements
- **Backlog**: Future ideas, not currently planned

---

## References

- [Epoch Architecture Philosophy](docs/internal/planning/epoch-architecture-philosophy.md)
- [Coding Style Guide](docs/internal/planning/coding-style-guide.md)
- [CHANGELOG.md](CHANGELOG.md)
