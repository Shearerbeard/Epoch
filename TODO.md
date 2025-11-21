# Epoch TODO

> **Project Work Items and Planning**
> This file tracks current work items, planned features, and known issues for the Epoch project.

**Last Updated**: 2025-11-21

---

## Next Session Focus

**Recommended Starting Point**: Update README.md to use Decider pattern

**Context**: The README currently shows outdated EventContext API examples. This is a major known issue that affects user onboarding and public perception. Updating it to use the current Decider/Evolver pattern will:
- Fix the primary documentation issue
- Provide users with correct examples
- Demonstrate the Railway-Oriented Programming patterns we've documented
- Take approximately 2 hours

**Preparation**:
1. Review current README.md examples
2. Check src/test_helpers/deciders.rs for correct patterns
3. Reference coding-style-guide.md for conventions
4. Ensure all examples compile and test

**Alternative**: If you prefer to work on code rather than documentation, "Replace thread::sleep with tokio::sleep" is a 1-hour technical improvement that addresses a known issue in the retry logic.

---

## Current Sprint

### In Progress
- [ ] None currently

### Ready to Start (Recommended Next Steps)
1. [ ] Update README.md to use Decider pattern (not EventContext)
   - Location: README.md
   - Reason: Major known issue, users see outdated API examples
   - Blocks: Public perception, user onboarding
   - Effort: ~2 hours

2. [ ] Create examples directory with compilable examples
   - Location: Create examples/ directory
   - Reason: Helps users understand patterns in practice
   - Contents: User domain, Truck domain, Expense domain examples
   - Effort: ~3 hours

3. [ ] Replace thread::sleep with tokio::sleep in retry logic
   - Location: src/strategies/mod.rs
   - Reason: Better async runtime integration
   - Blocks: Production-grade async usage
   - Effort: ~1 hour

4. [ ] Implement PostgreSQL repository backend
   - Location: Create src/repository/postgres/
   - Reason: Add relational database backend for broader adoption
   - Planning: Complete planning doc available
   - Effort: ~8-12 hours (can be done in phases)
   - Phase 1: Foundation (2-3 hours) - Module structure, dependencies
   - Phase 2: Core implementation (3-4 hours) - Repository trait
   - Phase 3: Testing (2-3 hours) - Spec tests, integration tests
   - Phase 4: Documentation (1-2 hours) - README examples, migration guide

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
- [ ] Implement PostgreSQL repository backend
  - Location: Create src/repository/postgres/
  - Planning: docs/internal/planning/postgres-repository-implementation.md
  - Reason: Provide relational database option for event sourcing
  - Dependencies: tokio-postgres, bb8, bb8-postgres
  - Effort: ~8-12 hours (4 phases)
  - Includes: Connection pooling, optimistic concurrency, generic spec tests
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

### Recently Completed (2025-11-21 Session)
- [x] Create comprehensive internal documentation structure
  - Added docs/internal/planning/ directory
  - Created epoch-architecture-philosophy.md
  - Created coding-style-guide.md
  - Created documentation-guidelines.md
  - Created docs/README.md overview

- [x] Integrate trucker_buddy_rs coding philosophy
  - Added Railway-Oriented Programming patterns
  - Added "Making Illegal States Unrepresentable" principle
  - Added protected concrete types (smart constructors)
  - Added NO EMOJIS documentation standard
  - Enhanced error handling guidelines

- [x] Add TODO and CHANGELOG workflow system
  - Created TODO.md for work item tracking
  - Created CHANGELOG.md following Keep a Changelog format
  - Created docs/internal/todo-changelog-workflow.md
  - Created .claude/session-start.md checklist
  - Updated .claude/context.md with Session Start Protocol

- [x] Set up automatic LLM session support
  - Session start protocol in context.md
  - Workflow documentation for daily/weekly/release processes
  - Templates for TODO items and CHANGELOG entries
  - Integration with git workflow

- [x] Plan PostgreSQL repository implementation
  - Created comprehensive planning document
  - Researched ESDB and Redis patterns
  - Reviewed Thalo PostgreSQL implementation
  - Defined schema, trait implementation, testing strategy
  - Documented 4-phase implementation plan
  - Ready for development

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
