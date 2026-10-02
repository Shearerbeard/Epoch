# Epoch Development Context

> **Automatically loaded in Claude Code sessions**
> Start here for Epoch's development conventions and source map.

## Quick Reference

- **Repository**: Event Sourcing + CQRS Framework (Rust)
- **Release candidate**: 0.1.0 (`epoch-journal` package, `epoch` library)
- **Core Pattern**: Decider Pattern (pure functional event sourcing)
- **Rust Edition**: 2021
- **Minimum Rust version**: 1.88

## Session Start Protocol

**At the start of each session, review**:

1. **[TODO.md](../TODO.md)** - Archived 2025 planning notes, not the
   current release backlog. Use the README and changelog for the
   shipped API and version.

2. **[CHANGELOG.md](../CHANGELOG.md)** - Recent changes
   - Review "Unreleased" section for latest updates
   - Understand what changed since last session

3. **[Session Start Checklist](session-start.md)** - Detailed session setup
   - Verify development environment
   - Review core principles
   - Set session goals

**Throughout the session**: Update CHANGELOG.md for user-facing changes.

## Essential Reading

Before making changes, review these internal documents:

1. **[Architecture & Philosophy](../docs/internal/planning/epoch-architecture-philosophy.md)**
   - Core design principles and patterns
   - Decider pattern explanation
   - Backend architecture
   - Event sourcing principles

2. **[Coding Style Guide](../docs/internal/planning/coding-style-guide.md)**
   - Naming conventions
   - Trait patterns
   - Error handling strategy
   - Testing patterns

## Core Philosophical Principles

**Influenced by Scott Wlaschin's "Domain Modeling Made Functional"**

### 1. Make Illegal States Unrepresentable

**Core Mantra**: "Make impossible states unrepresentable" - Yaron Minsky

- Use **protected concrete types** (smart constructors) with private fields
- All domain types validate in constructors, return `Result`
- **No primitive obsession**: Wrap all domain concepts (no raw String, Uuid, usize)
- State machines with enums for explicit transitions

### 2. Railway-Oriented Programming

**Error Handling Philosophy**:
- All fallible operations return `Result<T, E>`
- **Never panic** in business logic (except Evolvers where events are guaranteed valid)
- Use `?` operator for clean error propagation
- Chain validations for self-documenting code
- Custom error types with `thiserror`

### 3. NO EMOJIS

**Strict Rule**: Never use emojis in code, comments, commit messages, or documentation.
- Professional codebase
- Consistent tone
- Avoids encoding issues

## Core Architecture Summary

### The Decider Pattern (Central Concept)

```rust
pub trait Evolver {
    type State;
    type Evt: Event;
    fn evolve(state: Self::State, event: &Self::Evt) -> Self::State;
}

pub trait Decider: Evolver {
    type Cmd: Send + Sync;
    type Err;
    fn decide(state: &Self::State, cmd: &Self::Cmd) -> Result<Vec<Self::Evt>, Self::Err>;
}
```

**Critical**: `decide` and `evolve` must be **pure functions** (no I/O, no side effects)

### Layer Architecture

1. **Core Traits** (`src/decider.rs`) - Pure domain interfaces
2. **Repository Abstractions** (`src/repository/`) - Persistence traits
3. **Strategies** (`src/strategies/`) - Composition patterns (LoadDecideAppend, ReifyDecideSave)
4. **Backend Implementations** (`src/repository/{in_memory, esdb, redis}/`) - Feature-gated implementations

## Development Principles

### Type Safety First

- Use **associated types** for domain relationships (EntityId, Command, Event, State)
- Prefer compile-time guarantees over runtime validation
- Generic error parameters for backend flexibility

### Functional Purity

- **Domain logic** (`decide`, `evolve`): Pure functions only
- **Side effects**: Isolated to repository implementations
- **Testing**: No mocking required (use in-memory implementations)

### Backend Agnostic

- Single trait definitions, multiple implementations
- Feature flags: `in_memory`, `esdb`, `redis`
- Swap backends without changing business logic

## Common Tasks

### Adding a New Repository Backend

1. Create module under `src/repository/your_backend/`
2. Add feature flag to `Cargo.toml`
3. Implement repository traits with backend-specific types
4. Add conditional compilation: `#[cfg(feature = "your_backend")]`
5. Write tests using generic spec tests from `test_helpers/repository.rs`

### Adding a New Strategy

1. Create trait in `src/strategies/mod.rs`
2. Use cascading associated types (reference existing patterns)
3. Implement retry logic for version conflicts (exponential backoff)
4. Document in architecture doc

### Modifying Domain Logic

1. Ensure `decide` and `evolve` remain pure (no side effects)
2. Update event types with `#[derive(Serialize, Deserialize)]`
3. Implement `Event` trait with `event_type()` and `get_id()`
4. Update test fixtures in `test_helpers/deciders.rs`

## Error Handling

Three layers:

1. **Domain Errors**: Business rule violations (`UserDeciderError`)
2. **Repository Errors**: Backend-specific (generic `Err` parameter)
3. **Version Conflicts**: `VersionConflict(VersionDiff<V>)` - auto-retry in strategies

## Testing

### Pattern: Generic Spec Tests

```rust
pub async fn versioned_event_repository_with_streams_spec<'a, Err, V>()
where
    V: Eq + std::fmt::Debug,
    Err: std::fmt::Debug,
{
    // Test implementation works for all backends
}
```

Use in concrete backend tests to ensure consistency.

### Example Domain

`test_helpers/deciders.rs` contains `UserDecider` with guitar collections - use as reference implementation.

## Common Patterns

### Event Definition

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UserEvent {
    UserAdded { user_id: UserId, name: UserName },
    UserUpdated { user_id: UserId, name: Option<UserName> },
}

impl Event for UserEvent {
    type EntityId = UserId;

    fn event_type(&self) -> String {
        match self {
            UserEvent::UserAdded { .. } => "UserAdded".to_string(),
            UserEvent::UserUpdated { .. } => "UserUpdated".to_string(),
        }
    }

    fn get_id(&self) -> Self::EntityId {
        match self {
            UserEvent::UserAdded { user_id, .. } => user_id.clone(),
            UserEvent::UserUpdated { user_id, .. } => user_id.clone(),
        }
    }
}
```

### Repository Version Enum

```rust
pub enum RepositoryVersion<V> {
    Any,             // Accept any version
    Exact(V),       // Optimistic locking
    NoStream,       // Must not exist
    StreamExists,   // Must exist
}
```

## Known Issues & TODOs

1. **Retry Logic**: Uses `thread::sleep` instead of `tokio::sleep`
2. **Missing Feature**: `LoadDecideAppendWithSnapshot` not yet implemented

## File Locations

```
/home/user/Epoch/
├── src/
│   ├── decider.rs                  # Core trait definitions
│   ├── strategies/                 # Composition patterns
│   │   └── mod.rs
│   ├── repository/                 # Repository layer
│   │   ├── mod.rs                  # Version types & traits
│   │   ├── event.rs                # Event repository traits
│   │   ├── state.rs                # State repository traits
│   │   ├── in_memory/              # In-memory backend
│   │   ├── esdb/                   # EventStoreDB backend
│   │   ├── redis/                  # Redis backend
│   │   └── postgres/               # PostgreSQL backend
│   └── test_helpers/
│       ├── deciders.rs             # Example UserDecider
│       └── repository.rs           # Generic spec tests
├── docs/
│   └── internal/
│       └── planning/
│           ├── epoch-architecture-philosophy.md   # Detailed architecture doc
│           └── coding-style-guide.md              # Style conventions
├── .claude/
│   └── context.md                  # This file
├── Cargo.toml
└── README.md                       # User-facing documentation
```

## Documentation Philosophy

Following guidelines from [claude-skills](https://github.com/Shearerbeard/claude-skills):

### Internal vs. External Documentation

- **Internal** (`docs/internal/`): Design decisions and coding conventions for maintainers
  - Target audience: Developers and LLM assistants
  - Focus: WHY decisions were made, HOW patterns work
  - Examples: Architecture and style guides, plus implementation plans

- **External** (README, doc comments): User-facing API documentation
  - Target audience: Library users
  - Focus: WHAT the library does, HOW to use it
  - Examples: API docs, usage examples, tutorials

### LLM Session Context

Files in `.claude/` and `docs/internal/` are designed to provide LLM assistants with:
- Architectural context and design philosophy
- Coding patterns and conventions
- Common tasks and their implementations
- Known issues and limitations

The source tree and README remain authoritative when these notes fall behind.

## Quick Start Commands

```bash
# Run tests
cargo test

# Run tests for specific backend
cargo test --features esdb
cargo test --features redis

# Check all features
cargo check --all-features

# Build with specific backend
cargo build --features in_memory,esdb

# Format code
cargo fmt

# Lint
cargo clippy -- -D warnings
```

## Dependencies Quick Reference

- **async-trait**: Async trait methods
- **serde**: Serialization boundary
- **thiserror**: Error derivation
- **eventstore**: EventStoreDB backend (optional)
- **redis-om**: Redis backend (optional)
- **actix-rt**: Test runtime (dev)

## When Working on Epoch

Before making changes:
1. ✓ Read relevant sections of architecture doc
2. ✓ Review coding style guide
3. ✓ Check if domain logic remains pure (no side effects in `decide`/`evolve`)
4. ✓ Use protected concrete types (smart constructors, private fields)
5. ✓ No primitive obsession (wrap String, Uuid, usize in domain types)
6. ✓ Railway-oriented error handling (Result everywhere, no unwrap in business logic)
7. ✓ NO EMOJIS in code, comments, or commits
8. ✓ Ensure changes work with all backends (or add feature gates)
9. ✓ Update tests (use generic spec test pattern)
10. ✓ Update documentation if changing public API
11. ✓ Check README examples still work

## Getting Help

- **Architecture questions**: See `docs/internal/planning/epoch-architecture-philosophy.md`
- **Style questions**: See `docs/internal/planning/coding-style-guide.md`
- **Example implementation**: See `test_helpers/deciders.rs`
- **Testing patterns**: See `test_helpers/repository.rs`

---

*This context file is automatically loaded in Claude Code sessions to provide consistent development assistance.*
