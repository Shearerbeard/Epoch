# Epoch Architecture & Philosophy

> **Internal Planning Document**
> This document serves as context for LLM-assisted development sessions and captures the core architectural decisions, patterns, and philosophy behind the Epoch event sourcing framework.

## Overview

**Epoch** is an Event Sourcing + CQRS Framework for Rust, designed to provide composable, type-safe abstractions for building domain-driven applications with event sourcing patterns. The library is in alpha (v1.0.0-alpha.18) and follows a functional programming philosophy heavily influenced by established frameworks in other languages.

## Core Philosophy

### 1. Functional Purity at the Core

The central philosophy of Epoch is **separation of pure domain logic from side effects**:

- **Decision Logic**: Pure functions that validate commands and return events
- **Evolution Logic**: Pure functions that apply events to rebuild state
- **Side Effects**: Isolated to repository boundaries (persistence, I/O)

This separation enables:
- Easy testing of business logic without mocking
- Deterministic state reconstruction
- Clear reasoning about domain behavior
- Type-safe composition

### 2. The Decider Pattern

Epoch centers on the **Decider Pattern**, which breaks event sourcing into three core responsibilities:

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

**Key Insight**: These are pure, stateless functions. The framework handles:
- Loading state from events
- Persistence
- Concurrency control
- Retry logic

The domain developer only implements pure business logic.

### 3. Backend Agnosticism

One set of traits, multiple implementations:

- **In-Memory**: Fast, zero-dependency testing
- **EventStoreDB**: Production-grade event store
- **Redis**: Lightweight alternative with Redis Streams + JSON

Swap backends by changing a repository constructor. Business logic stays identical.

### 4. Type Safety Over Convenience

Epoch makes extensive use of **associated types** to encode domain relationships at compile time:

```rust
pub trait Event {
    type EntityId;
    fn event_type(&self) -> String;
    fn get_id(&self) -> Self::EntityId;
}
```

This creates complex trait bounds but provides:
- Compile-time validation of domain invariants
- No runtime string parsing or configuration errors
- IDE autocomplete for domain relationships
- Refactoring safety

**Trade-off Acknowledged**: More verbose generics for stronger guarantees.

### 5. Optimistic Concurrency by Default

All versioned repositories support optimistic locking:

```rust
pub enum RepositoryVersion<V> {
    Any,                    // Accept any version
    Exact(V),              // Require specific version
    NoStream,              // Stream doesn't exist yet
    StreamExists,          // Stream must already exist
}
```

Strategies automatically retry with exponential backoff on version conflicts (up to 20 retries). This enables:
- Conflict-free concurrent writes to different entities
- Automatic resolution of transient conflicts
- Manual handling of persistent conflicts

### 6. Making Illegal States Unrepresentable

**Core Mantra**: "Make impossible states unrepresentable" - Yaron Minsky

Epoch uses Rust's type system to prevent invalid states at compile time:

**Protected Concrete Types** (Smart Constructors):
- All domain types have private fields
- Validation happens in constructors (returns `Result`)
- Once created, types are guaranteed valid
- No primitive obsession - wrap all domain concepts

```rust
// Protected type with validation
pub struct StreamId {
    uuid: Uuid,  // Private - cannot be set directly
}

impl StreamId {
    pub fn new() -> Self { /* validates */ }
    pub fn from(str: &str) -> Result<Self, Error> { /* validates */ }
    pub fn value(&self) -> Uuid { /* controlled access */ }
}
```

**State Machines with Enums**:
- Use enums to model explicit state transitions
- Invalid transitions are impossible at compile time
- Pattern matching ensures all cases handled

**Benefits**:
- Encapsulation: Validation logic centralized
- Invariants: Types are always valid
- Type Safety: Can't mix UserId with StreamId
- Refactoring: Internal changes don't break API

### 7. Railway-Oriented Programming

Heavily influenced by Scott Wlaschin's error handling approach, Epoch uses **Railway-Oriented Programming**:

**The Railway Metaphor**:
- **Success track**: `Result::Ok` path
- **Failure track**: `Result::Err` path
- **Switches**: Functions that can fail (using `?` operator)
- **Composition**: Chain operations cleanly

```
Input
  │
  ├─[parse]──────┐
  │              ↓ Error track
  ├─[validate]───┐
  │              ↓ Error track
  ├─[process]────┐
  │              ↓ Error track
  ↓
Output (Success)
```

**Key Principles**:
- All fallible operations return `Result<T, E>`
- Never panic in business logic (except Evolvers where events are guaranteed valid)
- Use `?` operator for clean error propagation
- Custom error types with `thiserror`
- Validation helpers that compose with `?`

**Example**:
```rust
fn decide(
    context: &MyContext,
    state: &MyState,
    cmd: &MyCommand,
) -> Result<Vec<MyEvent>, MyError> {
    // Each ? is a "switch" to error track
    let id = EntityId::from(&cmd.id)
        .map_err(MyError::InvalidId)?;  // Switch point

    Self::validate_exists(state, &id)?;  // Switch point
    Self::validate_unique(context, &cmd.name)?;  // Switch point

    // Only reaches here if all validations passed
    Ok(vec![MyEvent::Created { /* ... */ }])
}
```

This pattern creates self-documenting, composable, and safe validation chains.

## Architectural Layers

### Layer 1: Core Traits (Pure Domain)

**Location**: `src/decider.rs`

Defines the pure functional interface:
- `Evolver`: State evolution from events
- `Decider`: Command decision without context
- `DeciderWithContext`: Command decision with dependency injection

**No I/O, No Side Effects**: These traits contain only pure functions.

### Layer 2: Repository Abstractions

**Location**: `src/repository/`

Two flavors of persistence:

**Event-Based** (`event.rs`):
- `EventRepository<E, Err>`: Simple append/load
- `VersionedEventRepository<E, Err>`: With optimistic locking
- `VersionedEventRepositoryWithStreams<'a, E, Err>`: Multi-stream support

**State-Based** (`state.rs`):
- `StateRepository<State, Err>`: Simple reify/save
- `VersionedStateRepository<'a, State, Err>`: Snapshots with versioning
- `VersionedStreamSnapshotRepository<State>`: Per-stream snapshots

**Key Pattern**: Generic `Err` parameter allows each backend to define its own error types while sharing trait implementations.

### Layer 3: Strategies (Composition)

**Location**: `src/strategies/`

High-level patterns composing Decider + Repository:

- **StateFromEventRepository**: Rebuild state by replaying events
- **LoadDecideAppend**: Load → Decide → Append with retry
- **ReifyDecideSave**: Load snapshot → Decide → Save with retry
- **DecideEvolveWithCommandResponse**: Return command result + new state

These encode the most common event sourcing workflows as reusable abstractions.

### Layer 4: Backend Implementations

**Location**: `src/repository/{in_memory, esdb, redis}/`

Each backend implements the repository traits:

| Backend | Version Type | Notes |
|---------|--------------|-------|
| In-Memory | `usize` | Position-based, uses `Arc<Mutex<Vec<E>>>` |
| ESDB | `usize` | Revision numbers, wraps `eventstore` crate |
| Redis | `RedisVersion` | Composite `{timestamp, version}`, uses redis-om |

**Feature Flags**: All backends are optional and can be compiled independently.

## Key Design Patterns

### Associated Type Cascades

Traits build on each other's associated types:

```rust
pub trait LoadDecideAppend {
    type Decide: DeciderWithContext + Send + Sync;
    type Repository: VersionedEventRepositoryWithStreams<
        <Self::Decide as Evolver>::Evt,
        Self::RepoErr,
    >;
    // ...
}
```

This creates deeply nested generics but ensures type safety across composition boundaries.

### StreamId Abstraction

Bridges Event Entity IDs to Repository Stream IDs:

```rust
pub trait StreamIdFromEvent<Evt: Event>: Sized {
    fn from(e: Evt) -> Self;
    fn event_entity_id_into(id: <Evt as Event>::EntityId) -> Self;
}
```

Allows backends to customize stream naming without changing domain code.

### Lifetime Management

The `'a` lifetime in `VersionedEventRepositoryWithStreams<'a, E, Err>` exists solely to work around async-trait compiler limitations with generic lifetimes. This is a pragmatic compromise.

### Error Layering

Three levels of errors:

1. **Domain Errors**: User-defined enums (e.g., `UserDeciderError`)
2. **Repository Errors**: Backend-specific (generic `Err` parameter)
3. **Version Conflicts**: `VersionConflict(VersionDiff<V>)` wraps optimistic locking failures

Strategies handle version conflicts with retry. Domain errors propagate immediately.

## Testing Strategy

### Universal Spec Tests

Generic test functions work across all backends:

```rust
pub async fn versioned_event_repository_with_streams_spec<'a, Err, V>()
where
    // Generic constraints
{
    // Test implementation
}
```

This ensures consistent behavior across in-memory, ESDB, and Redis.

### Example Domain

`test_helpers/deciders.rs` contains a full UserDecider implementation with guitar collections. This serves as:
- Documentation by example
- Integration test fixture
- Demonstration of patterns

**No Mocking**: In-memory implementations serve as test doubles.

## Coding Conventions

### Naming

- **Traits**: Descriptive nouns (Evolver, Decider, LoadDecideAppend)
- **Associated Types**: CapitalCase (Cmd, Evt, State, Err, Ctx, Version)
- **Functions**: snake_case (event_type, get_id)
- **Domain Types**: Enums as ADTs (UserCommand, UserEvent)

### Dependencies

- **`async-trait`**: Required for async trait methods
- **`serde`**: Universal serialization boundary
- **`thiserror`**: Derive-based error handling
- **Backend-specific**: Behind feature flags

### Code Organization

- **Traits**: Separate from implementations
- **Feature flags**: Clean separation of backends
- **Test helpers**: Shared fixtures and utilities
- **Examples**: Embedded in README and test code

## Current Limitations & Future Work

### Known Issues

1. **README Outdated**: Still references old `EventContext` API instead of Decider pattern
2. **Manual Sleep**: Retry logic uses `thread::sleep` instead of `tokio::sleep` (pragmatic alpha choice)
3. **Limited Snapshot Strategy**: `LoadDecideAppendWithSnapshot` not yet implemented

### Alpha Status

Version 1.0.0-alpha.18 indicates:
- Core API is stable but may change
- Production use possible but monitor for updates
- Documentation may lag implementation
- Feature set focused on core patterns

## Event Sourcing Principles

### Event Immutability

Once persisted, events never change. This enables:
- Complete audit trail
- Time travel debugging
- Replay to any point in history
- Safe distributed replay

### State as Projection

State is **derived** from events, not stored directly:
- Rebuild state by replaying events through `evolve`
- Snapshots are performance optimizations, not source of truth
- Multiple projections from same event stream

### Command-Query Separation (CQRS)

- **Commands**: Validated by `decide`, produce events
- **Queries**: Read from projections (state repositories)
- Separate paths enable independent scaling

## Influences & Inspiration

Epoch synthesizes ideas from:

- **[Thalo](https://github.com/thalo-rs/thalo)** (Rust): Core structure and Rust patterns
- **[Eventful](https://github.com/jdreaver/eventful)** (Haskell): Functional purity
- **[Equinox](https://github.com/jet/equinox)** (F#): Strategy patterns
- **[f(model)](https://github.com/fraktalio/fmodel)** (Kotlin): Decider pattern formalization

The goal is to bring the best ideas from functional event sourcing to Rust's type system.

## Development Guidelines for LLM Sessions

When working on Epoch:

1. **Preserve Purity**: Keep `decide` and `evolve` functions pure (no I/O, no side effects)
2. **Test Generically**: Use spec test pattern for repository implementations
3. **Feature Flag**: Backend-specific code belongs behind feature flags
4. **Associated Types**: Prefer associated types over generic parameters for domain relationships
5. **Error Layering**: Domain errors separate from repository errors
6. **Async-First**: All repository operations are async
7. **Document Lifetimes**: Explain non-obvious lifetime annotations
8. **Update README**: Keep public docs in sync with implementation changes

## Summary

Epoch is a **production-grade alpha library** that prioritizes:
- Correctness over convenience
- Type safety over simplicity
- Functional purity over pragmatism (at domain boundaries)
- Backend flexibility over vendor lock-in

The heavy use of associated types and trait composition reflects deep Rust expertise and a commitment to compile-time guarantees. The Decider pattern provides a clean mental model: pure business logic surrounded by compositional infrastructure.
