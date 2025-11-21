# Epoch Coding Style Guide

> **Internal Reference for Development**
> This guide captures the coding conventions, patterns, and style preferences for the Epoch repository. These practices are heavily influenced by Scott Wlaschin's "Domain Modeling Made Functional" and functional programming principles.

**Last Updated**: 2025-11-21

---

## Philosophy & Guiding Principles

Our approach is grounded in:

1. **Scott Wlaschin's "Domain Modeling Made Functional"**
   - Type-driven design
   - Making illegal states unrepresentable
   - Railway-Oriented Programming for error handling

2. **Functional Programming Principles**
   - Immutability by default
   - Pure functions where possible
   - Composition over inheritance
   - Explicit error handling

3. **Event Sourcing & CQRS**
   - Decider pattern for command validation
   - Evolver pattern for state reconstruction
   - Pure domain logic separated from side effects

**Core Mantra**: "Make impossible states unrepresentable" - Yaron Minsky

---

## 1. Type-Driven Development

### 1.1 Making Illegal States Unrepresentable

**Principle**: Use Rust's type system to prevent invalid states at compile time, not runtime.

**Pattern - Protected Concrete Types (Smart Constructors)**:

```rust
// CORRECT: Protected type with validation
#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub struct EventId {
    uuid: Uuid,  // Private field - cannot be set directly
}

impl EventId {
    // Smart constructor with validation
    pub fn new() -> Self {
        Self {
            uuid: Uuid::new_v4(),
        }
    }

    pub fn from(str: &str) -> Result<Self, ValidationError> {
        Ok(Self {
            uuid: Uuid::from_str(str)
                .map_err(|_| ValidationError::InvalidUUID(str.to_owned()))?,
        })
    }

    // Controlled access to inner value
    pub fn value(&self) -> Uuid {
        self.uuid
    }
}

// WRONG: Public fields allow invalid states
pub struct EventId {
    pub uuid: Uuid,  // Anyone can set to nil UUID
}
```

**Why This Matters**:
- **Encapsulation**: Validation logic is centralized in constructor
- **Invariants**: Once created, the type is guaranteed valid
- **Type Safety**: Can't accidentally mix UserId with StreamId
- **Refactoring**: Internal representation can change without breaking API

**Requirements for Value Objects**:
- Private inner field(s)
- `new()` for generation (if applicable)
- `from()` or `try_from()` for parsing with validation
- `value()` for controlled access
- Implement `Display` for string representation
- Derive `Hash`, `PartialEq`, `Eq` for collections
- Derive `Serialize`, `Deserialize` for persistence

### 1.2 No Primitive Obsession

**Principle**: Never pass `String`, `usize`, `Uuid`, `u64` directly when it represents a domain concept.

```rust
// WRONG: Primitive obsession
pub trait Event {
    fn get_id(&self) -> String;  // Loses type information
}

fn append_event(stream_id: String, event: MyEvent) -> Result<(), Error>

// CORRECT: Domain types
pub trait Event {
    type EntityId;
    fn get_id(&self) -> Self::EntityId;
}

fn append_event(stream_id: &StreamId, event: &MyEvent) -> Result<(), Error>
```

**Benefits**:
- Compiler prevents passing version where stream_id expected
- Self-documenting code
- Validation happens at type boundary
- Can add domain logic to types later without breaking callers

### 1.3 Associated Types for Domain Relationships

**Principle**: Use associated types when there's a single "natural" type for a context.

```rust
// CORRECT: Associated type encodes relationship
pub trait Event {
    type EntityId: Clone + PartialEq;
    fn get_id(&self) -> Self::EntityId;
}

// AVOID: Generic parameter when relationship is fixed
pub trait Event<EntityId> {
    fn get_id(&self) -> EntityId;
}
```

**When to Use Associated Types vs. Generic Parameters**:

- **Associated Types**: When there's one "correct" type for the context
  - EntityId for an Event (each event type has one entity ID type)
  - Version for a Repository (each repository has one version type)

- **Generic Parameters**: When multiple valid types make sense
  - Repository error types (backends define their own errors)
  - State types (multiple state representations possible)

### 1.4 State Machines with Enums

**Principle**: Use Rust's enum types to model state transitions explicitly and make invalid transitions impossible.

```rust
// State machine for Stream lifecycle
#[derive(Debug)]
pub enum StreamState<E> {
    NotCreated,
    Exists(Vec<E>),
}

impl<E> StreamState<E> {
    // Enforce state preconditions - return Result for Railway-Oriented Programming
    fn assert_not_created(&self) -> Result<(), StreamError> {
        match self {
            StreamState::NotCreated => Ok(()),
            StreamState::Exists(_) => Err(StreamError::AlreadyExists),
        }
    }

    fn assert_created(&self) -> Result<(), StreamError> {
        match self {
            StreamState::Exists(_) => Ok(()),
            StreamState::NotCreated => Err(StreamError::NotFound),
        }
    }

    // Type-safe access to inner state
    fn get_events(&self) -> Result<&Vec<E>, StreamError> {
        match self {
            StreamState::Exists(events) => Ok(events),
            StreamState::NotCreated => Err(StreamError::NotFound),
        }
    }
}
```

**Usage in Deciders**:
```rust
fn decide(
    state: &StreamState<MyEvent>,
    cmd: &MyCommand,
) -> Result<Vec<MyEvent>, MyError> {
    match cmd {
        MyCommand::Create(_) => {
            state.assert_not_created()?;  // Compile-time enforced check
            // Can only reach here if state is NotCreated
            Ok(vec![MyEvent::Created { /* ... */ }])
        }
        MyCommand::Update(_) => {
            state.assert_created()?;  // Must exist to update
            let events = state.get_events()?;  // Type-safe access
            Ok(vec![MyEvent::Updated { /* ... */ }])
        }
    }
}
```

**Why This Matters**:
- Can't update stream that doesn't exist (type-safe)
- Can't create stream twice
- Explicit state transitions
- Pattern matching ensures all cases handled

---

## 2. Railway-Oriented Programming (Error Handling)

### 2.1 The Railway Metaphor

**Concept**: Functions are like railway tracks with two paths:
- **Success track**: `Result::Ok` path
- **Failure track**: `Result::Err` path
- **Switches**: Functions that can fail (using `?` operator)
- **Composition**: Chain operations with `?`

**Visual**:
```
Input
  │
  ├─[parse]──────┐
  │              ↓ Error track → Err(ParseError)
  ├─[validate]───┐
  │              ↓ Error track → Err(ValidationError)
  ├─[process]────┐
  │              ↓ Error track → Err(ProcessError)
  ↓
Output (Success) → Ok(Result)
```

### 2.2 Result Types Everywhere

**Principle**: All operations that can fail return `Result<T, E>`. Never panic in business logic.

```rust
// CORRECT: Explicit error handling
pub fn from(str: &str) -> Result<Self, ValidationError> {
    let uuid = Uuid::from_str(str)
        .map_err(|_| ValidationError::InvalidUUID(str.to_owned()))?;
    Ok(Self { uuid })
}

// WRONG: Panics on invalid input
pub fn from(str: &str) -> Self {
    Self {
        uuid: Uuid::from_str(str).expect("valid uuid")  // DON'T PANIC!
    }
}
```

**Exception**: `unwrap()` is acceptable in Evolvers where events are guaranteed valid by construction.

### 2.3 Railway Composition Pattern

**Pattern**: Chain validations using `?` operator for clean error propagation.

```rust
fn decide(
    context: &MyContext,
    state: &MyState,
    cmd: &MyCommand,
) -> Result<Vec<MyEvent>, MyError> {
    // Each ? is a "switch" to error track if it fails
    let entity_id = EntityId::from(&cmd.id)
        .map_err(MyError::InvalidId)?;  // Switch point

    Self::validate_exists(state, &entity_id)?;  // Switch point

    Self::validate_unique(context, &cmd.name)?;  // Switch point

    // Only reaches here if all switches stayed on success track
    Ok(vec![MyEvent::Processed { /* ... */ }])
}
```

### 2.4 Custom Error Types with `thiserror`

**Pattern**:
```rust
use thiserror::Error;

#[derive(Debug, Error, Clone)]
pub enum MyDomainError {
    #[error("Entity not found: {0}")]
    NotFound(EntityId),

    #[error("Name already taken: {0}")]
    NameTaken(String),

    #[error("Invalid field")]
    InvalidField(#[from] FieldError),

    #[error("Repository error: {0}")]
    RepositoryError(String),
}
```

**Requirements**:
- Descriptive error variants with context
- Use `#[from]` for automatic error conversion
- Implement `Display` via `#[error()]` attribute
- Domain-specific error types (per bounded context)
- Include relevant IDs and values that caused the error

### 2.5 Validation Helpers

**Principle**: Create small, composable validation functions that return `Result` for railway composition.

```rust
impl MyDecider {
    // Helper: Validate entity exists
    fn validate_exists(
        state: &MyState,
        id: &EntityId,
    ) -> Result<(), MyError> {
        state.entities.contains_key(id)
            .then_some(())
            .ok_or_else(|| MyError::NotFound(id.clone()))
    }

    // Helper: Validate name is unique
    fn validate_unique(
        ctx: &MyContext,
        name: &str,
    ) -> Result<(), MyError> {
        (!ctx.names.contains(name))
            .then_some(())
            .ok_or_else(|| MyError::NameTaken(name.to_string()))
    }

    // Helper: Unwrap Option or error
    fn unwrap_entity(
        entity: &Option<Entity>,
        id: &EntityId,
    ) -> Result<Entity, MyError> {
        entity.clone()
            .ok_or_else(|| MyError::NotFound(id.clone()))
    }
}
```

**Benefits**:
- Composable with `?` operator
- Reusable across commands
- Clear, documented validation logic
- Type-safe error handling
- Self-documenting business rules

### 2.6 Error Propagation Patterns

```rust
// Map errors to domain error type
let entity_id = EntityId::from(str)
    .map_err(MyError::InvalidField)?;

// Automatic conversion with #[from]
let value = MyValue::new(str)?;  // FieldError -> MyError via From

// Chain validations
Self::validate_exists(state, &id)?;
Self::validate_unique(context, &name)?;

// Transform Result type
context.validate_related(related_id)
    .map_err(|e| MyError::InvalidRelated(related_id, e))?;
```

---

## 3. Functional Purity at Domain Boundaries

### 3.1 Keep Business Logic Pure

**Principle**: Domain logic (`decide`, `evolve`) must be pure functions with no side effects.

```rust
// CORRECT: Pure decision function
impl Decider for MyDecider {
    fn decide(state: &Self::State, cmd: &Self::Cmd) -> Result<Vec<Self::Evt>, Self::Err> {
        // Only domain logic: validation, business rules, event creation
        state.validate_preconditions()?;

        Ok(vec![MyEvent::Created { /* ... */ }])
    }
}

// WRONG: Side effects in domain logic
impl Decider for MyDecider {
    fn decide(state: &Self::State, cmd: &Self::Cmd) -> Result<Vec<Self::Evt>, Self::Err> {
        log::info!("Processing command: {:?}", cmd);  // NO I/O
        self.metrics.increment("commands");           // NO SIDE EFFECTS
        db.query("SELECT ...").await?;                // NO DATABASE CALLS

        Ok(vec![MyEvent::Created { /* ... */ }])
    }
}
```

### 3.2 Isolate Side Effects to Repository Layer

**Separation of Concerns**:
- **Domain Layer** (`decide`, `evolve`): Pure transformations
- **Repository Layer**: I/O, network calls, persistence
- **Strategy Layer**: Composition of domain + repository

```rust
// Pure domain logic
impl Decider for MyDecider {
    fn decide(state: &Self::State, cmd: &Self::Cmd) -> Result<Vec<Self::Evt>, Self::Err> {
        // No I/O - pure validation and event creation
        Ok(vec![MyEvent::Created])
    }
}

// Side effects in repository
#[async_trait]
impl Repository for MyRepository {
    async fn append(&mut self, events: &Vec<Event>) -> Result<(), Error> {
        // I/O happens here
        self.db.write(events).await
    }
}
```

---

## 4. Event Sourcing Patterns

### 4.1 Decider Pattern

**Principle**: Deciders validate commands against current state and produce events. They must be pure functions.

```rust
pub struct MyDecider;

impl DeciderWithContext for MyDecider {
    type Ctx = MyContext;
    type Cmd = MyCommand;
    type Evt = MyEvent;
    type Err = MyError;

    fn decide(
        ctx: &Self::Ctx,
        state: &Self::State,
        cmd: &Self::Cmd,
    ) -> Result<Vec<Self::Evt>, Self::Err> {
        // 1. Validate state preconditions
        state.assert_valid()?;

        // 2. Validate command with context (cross-aggregate checks)
        Self::validate_unique(ctx, &cmd.name)?;

        // 3. Return events (never modify state directly)
        Ok(vec![MyEvent::Created { /* ... */ }])
    }
}
```

**Requirements**:
- Pure function (no side effects)
- Returns `Result<Vec<Event>, Error>`
- Never modifies state
- Validates all business rules
- Uses context for cross-aggregate validation
- Railway-oriented error handling

### 4.2 Evolver Pattern

**Principle**: Evolvers apply events to rebuild state. Must be pure and deterministic.

```rust
impl Evolver for MyDecider {
    type State = MyState;
    type Evt = MyEvent;

    fn evolve(state: Self::State, event: &Self::Evt) -> Self::State {
        match (state, event) {
            (MyState::NotCreated, MyEvent::Created { id, name }) => {
                MyState::Exists(Entity {
                    id: *id,
                    name: name.clone(),
                })
            }
            (MyState::Exists(entity), MyEvent::Updated { name, .. }) => {
                MyState::Exists(Entity {
                    name: name.clone().unwrap_or(entity.name),
                    ..entity
                })
            }
            // Invalid transitions return state unchanged
            (state, _) => state,
        }
    }
}
```

**Requirements**:
- Pure function (no I/O, no randomness)
- Deterministic (same inputs → same output)
- Never fails (events are valid by construction)
- Pattern match on (state, event) tuple
- Handle all state transitions explicitly
- `unwrap()` acceptable here since events are guaranteed valid

---

## 5. Naming Conventions

### 5.1 Value Objects

**Pattern**: `{Domain}{Property}`

Examples:
- `UserId`, `UserEmail`, `UserName`
- `StreamId`, `StreamVersion`
- `EventId`, `EventType`

**Structure**:
```rust
pub struct EntityId {
    uuid: Uuid,  // Private
}

impl EntityId {
    pub fn new() -> Self { /* ... */ }
    pub fn from(str: &str) -> Result<Self, Error> { /* ... */ }
    pub fn value(&self) -> Uuid { /* ... */ }
}
```

### 5.2 Commands

**Pattern**: `{Action}{Domain}Command`

Examples:
- `AddUserCommand`, `UpdateUserCommand`
- `CreateStreamCommand`, `AppendEventCommand`

**Structure**:
```rust
// Each command is a struct
pub struct AddUserCommand {
    pub name: String,
    pub email: String,
}

// Wrapped in enum for type safety
pub enum UserCommand {
    AddUser(AddUserCommand),
    UpdateUser(UpdateUserCommand),
}
```

### 5.3 Events

**Pattern**: `{Domain}{PastTenseAction}` or `{Domain}Event` enum

Examples:
- `UserAdded`, `UserUpdated`
- `StreamCreated`, `EventsAppended`

**Structure**:
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UserEvent {
    UserAdded {
        user_id: UserId,
        name: UserName,
    },
    UserUpdated {
        user_id: UserId,
        name: Option<UserName>,  // None = no change
    },
}
```

### 5.4 Errors

**Pattern**: `{Domain}Error`

Examples:
- `UserError`, `StreamError`, `RepositoryError`

**Structure**:
```rust
#[derive(Error, Debug)]
pub enum UserError {
    #[error("User not found: {0}")]
    NotFound(UserId),

    #[error("Email already taken: {0}")]
    EmailTaken(String),
}
```

### 5.5 State Types

**Pattern**: `{Domain}State`

Examples:
- `UserState`, `StreamState`

**Structure**:
```rust
// Enum for state machines
pub enum StreamState<E> {
    NotCreated,
    Exists(Vec<E>),
}

// Struct for collections
pub struct UsersState(HashMap<UserId, User>);
```

### 5.6 Traits

**Use Descriptive Nouns or Verb-Nouns**

Examples:
- `Evolver`, `Decider`, `DeciderWithContext`
- `EventRepository`, `StateRepository`
- `LoadDecideAppend`, `ReifyDecideSave`

**AVOID**: Prefixes like "I" or "T"
```rust
// WRONG
pub trait IDecider { }
pub trait TRepository { }

// CORRECT
pub trait Decider { }
pub trait Repository { }
```

### 5.7 Functions & Methods

**Use snake_case**

Common prefixes:
- `get_`: Accessor methods
- `from_`: Conversion methods
- `into_`: Consuming conversion methods
- `assert_`: State validation methods
- `validate_`: Business rule validation

---

## 6. Code Organization

### 6.1 Module Structure

```
src/
├── lib.rs                  // Public API exports
├── decider.rs              // Core trait definitions
├── strategies/             // High-level composition patterns
│   └── mod.rs
├── repository/             // Repository abstractions & implementations
│   ├── mod.rs              // Version types & core traits
│   ├── event.rs            // Event repository traits
│   ├── state.rs            // State repository traits
│   ├── in_memory/          // In-memory implementation
│   ├── esdb/               // EventStoreDB implementation
│   └── redis/              // Redis implementation
└── test_helpers/           // Shared test utilities
```

### 6.2 Import Organization

```rust
// 1. Standard library
use std::{collections::HashMap, fmt::Display};

// 2. External crates (alphabetical)
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

// 3. Framework dependencies (Epoch)
use epoch::{
    decider::{DeciderWithContext, Evolver},
    strategies::StateFromEventRepository,
};

// 4. Internal modules (relative imports)
use crate::repository::{EventRepository, Version};
use super::Audit;
```

---

## 7. Testing Patterns

### 7.1 Value Object Tests

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_uuid_accepted() {
        let id = EntityId::from("550e8400-e29b-41d4-a716-446655440000");
        assert!(id.is_ok());
    }

    #[test]
    fn invalid_uuid_rejected() {
        let id = EntityId::from("not-a-uuid");
        assert!(id.is_err());
    }

    #[test]
    fn empty_string_rejected() {
        let id = EntityId::from("");
        assert!(id.is_err());
    }
}
```

### 7.2 Decider Tests (Command Validation)

```rust
#[test]
fn cannot_create_duplicate() {
    let context = MyContext::default();
    let state = MyState::Exists(entity);
    let cmd = MyCommand::Create(data);

    let result = MyDecider::decide(&context, &state, &cmd);

    assert!(matches!(result, Err(MyError::AlreadyExists)));
}
```

### 7.3 Evolver Tests (State Evolution)

```rust
#[test]
fn event_creates_entity() {
    let state = MyState::NotCreated;
    let event = MyEvent::Created { id, name };

    let new_state = MyDecider::evolve(state, &event);

    assert!(matches!(new_state, MyState::Exists(_)));
}
```

---

## 8. Documentation Standards

### 8.1 NO EMOJIS

**Rule**: Never use emojis in code, comments, commit messages, or documentation.

**Rationale**:
- Professional codebase
- Consistent tone
- Avoids encoding issues
- Clear, focused communication

```rust
// CORRECT
/// Validates that the email address is not already taken.

// WRONG
/// Validates that the email address is not already taken ✨
```

### 8.2 Doc Comments

```rust
/// Creates a new EntityId by generating a random UUID v4.
///
/// # Examples
///
/// ```
/// let id = EntityId::new();
/// assert!(!id.value().is_nil());
/// ```
pub fn new() -> Self {
    Self {
        uuid: Uuid::new_v4(),
    }
}

/// Parses an EntityId from a string representation.
///
/// # Errors
///
/// Returns `ValidationError::InvalidUUID` if the string is not a valid UUID.
pub fn from(str: &str) -> Result<Self, ValidationError> {
    // ...
}
```

---

## 9. Anti-Patterns to Avoid

### 9.1 Don't Use `unwrap()` or `expect()` in Business Logic

```rust
// WRONG: Panics on invalid input
let id = EntityId::from(str).unwrap();

// CORRECT: Propagates error
let id = EntityId::from(str)
    .map_err(MyError::InvalidId)?;
```

**Exception**: `unwrap()` acceptable in Evolvers where events are guaranteed valid.

### 9.2 Don't Expose Public Mutable Fields

```rust
// WRONG: Can be modified to invalid state
pub struct EntityId {
    pub uuid: Uuid,
}

// CORRECT: Private with controlled access
pub struct EntityId {
    uuid: Uuid,  // Private
}

impl EntityId {
    pub fn value(&self) -> Uuid {
        self.uuid
    }
}
```

### 9.3 Don't Mix Domain and Infrastructure

```rust
// WRONG: Domain type with repository logic
impl Event {
    pub async fn save(&self, db: &Database) -> Result<(), Error> {
        db.insert_event(self).await
    }
}

// CORRECT: Separate domain from infrastructure
// Domain
pub struct Event {
    pub id: EventId,
    // ...
}

// Infrastructure (Repository)
#[async_trait]
impl EventRepository for MyRepository {
    async fn append(&mut self, events: &Vec<Event>) -> Result<(), Error> {
        self.db.write(events).await
    }
}
```

### 9.4 Don't Use Stringly-Typed Data

```rust
// WRONG: Error-prone, no type safety
fn append_event(stream_id: String, event_id: String) -> Result<(), Error>

// CORRECT: Type-safe, self-documenting
fn append_event(stream_id: &StreamId, event_id: &EventId) -> Result<(), Error>
```

### 9.5 Don't Ignore Errors

```rust
// WRONG: Silently ignores errors
let _ = validate_input();

// CORRECT: Handle or propagate
validate_input()?;

// Or explicitly document why ignoring
let _ = log_event();  // Logging failures are non-critical
```

### 9.6 Don't Use String-Based Configuration

```rust
// WRONG: Loses type information
pub trait Event {
    fn entity_type(&self) -> String;
    fn get_id(&self) -> String;
}

// CORRECT: Type-safe relationships
pub trait Event {
    type EntityId: Clone + PartialEq;
    fn get_id(&self) -> Self::EntityId;
}
```

---

## 10. Code Review Checklist

### Type Safety
- [ ] No public mutable fields on domain types
- [ ] All domain concepts have newtype wrappers (no primitive obsession)
- [ ] Value objects have private fields with smart constructors
- [ ] IDs use newtype pattern with validation

### Error Handling
- [ ] All fallible operations return `Result<T, E>`
- [ ] No `unwrap()` or `expect()` in business logic (except Evolvers)
- [ ] Custom error types use `thiserror`
- [ ] Errors include context (IDs, values that caused error)
- [ ] Railway-oriented composition with `?` operator

### State Machines
- [ ] State represented with enums where applicable
- [ ] State transitions validated with `assert_*` methods
- [ ] Pattern matching exhaustive on state transitions

### Event Sourcing
- [ ] Deciders are pure (no side effects)
- [ ] Evolvers are deterministic
- [ ] Events are immutable
- [ ] Context types for cross-aggregate validation

### Documentation
- [ ] NO EMOJIS in code, comments, or commits
- [ ] Public APIs have doc comments
- [ ] Examples included for non-obvious functions
- [ ] Error conditions documented

### Testing
- [ ] Value objects tested for validation
- [ ] Deciders tested for command validation
- [ ] Evolvers tested for state transitions
- [ ] Integration tests for full flows

### Code Quality
- [ ] Passes `cargo clippy` with no warnings
- [ ] Formatted with `cargo fmt`
- [ ] No primitive obsession
- [ ] Clear, descriptive names

---

## 11. Performance Considerations

### 11.1 Clone vs. Reference

```rust
// Small Copy types - pass by value
pub fn validate_id(id: EntityId) -> Result<(), Error> {
    // EntityId is Copy (contains only Uuid)
}

// Large types - pass by reference
pub fn validate_entity(entity: &Entity) -> Result<(), Error> {
    // Entity contains HashSet and multiple fields
}

// Return owned for API boundaries
pub fn value(&self) -> Uuid {
    self.uuid  // Copy
}

pub fn name(&self) -> String {
    self.name.clone()  // Explicit clone for caller
}
```

### 11.2 HashSet for Uniqueness Checks

```rust
pub struct MyContext {
    pub names: HashSet<String>,  // O(1) lookup
    pub ids: HashSet<EntityId>,
}

fn validate_unique(ctx: &MyContext, name: &str) -> Result<(), MyError> {
    (!ctx.names.contains(name))  // Efficient O(1) check
        .then_some(())
        .ok_or_else(|| MyError::NameTaken(name.to_string()))
}
```

---

## 12. Summary

**Core Principles**:
1. **Make illegal states unrepresentable** through types
2. **Railway-Oriented Programming** for error handling
3. **Protected concrete types** with smart constructors
4. **No primitive obsession** - wrap all domain concepts
5. **Pure functions** for Deciders and Evolvers
6. **NO EMOJIS** - professional codebase
7. **Type safety over convenience**

**Key Practices**:
- Value objects wrap all primitives with validation
- Result types for all fallible operations
- State machines with enums for explicit transitions
- Validation at type boundaries (smart constructors)
- Context types for cross-aggregate validation
- Railway composition with `?` operator

**Remember**: The compiler is your friend. Use Rust's type system to enforce business rules at compile time, not runtime.

---

**Review Cycle**: Update when new patterns emerge
**Maintained By**: Development Team

This document should be consulted before implementing any new feature. When in doubt, look at existing code for patterns.
