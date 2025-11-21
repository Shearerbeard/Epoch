# Epoch Coding Style Guide

> **Internal Reference for Development**
> This guide captures the coding conventions, patterns, and style preferences for the Epoch repository. Use this as a reference for maintaining consistency across the codebase.

## Core Principles

### 1. Type-Driven Development

**Prefer Compile-Time Guarantees Over Runtime Checks**

```rust
// GOOD: Use associated types to encode relationships
pub trait Event {
    type EntityId;
    fn get_id(&self) -> Self::EntityId;
}

// AVOID: String-based configuration
pub trait Event {
    fn get_id(&self) -> String;  // Loses type information
}
```

**When to Use Associated Types vs. Generic Parameters**

- **Associated Types**: When there's a single "natural" type for the context (EntityId for an Event)
- **Generic Parameters**: When multiple valid types make sense (Repository error types)

### 2. Functional Purity at Domain Boundaries

**Keep Business Logic Pure**

```rust
// GOOD: Pure decision function
impl Decider for UserDecider {
    fn decide(state: &Self::State, cmd: &Self::Cmd) -> Result<Vec<Self::Evt>, Self::Err> {
        match cmd {
            UserCommand::AddUser(name) => {
                if state.users.contains_key(name) {
                    Err(UserDeciderError::UserAlreadyExists)
                } else {
                    Ok(vec![UserEvent::UserAdded { name: name.clone() }])
                }
            }
        }
    }
}

// AVOID: Side effects in domain logic
impl Decider for UserDecider {
    fn decide(state: &Self::State, cmd: &Self::Cmd) -> Result<Vec<Self::Evt>, Self::Err> {
        // DON'T: Call external APIs, log, or perform I/O here
        log::info!("Processing command: {:?}", cmd);  // ❌
        self.metrics.increment("commands");           // ❌
    }
}
```

**Isolate Side Effects to Repository Layer**

- All I/O, network calls, and external interactions happen in repository implementations
- Domain logic (`decide`, `evolve`) contains only pure transformations

### 3. Error Handling Strategy

**Three-Layer Error Model**

1. **Domain Errors**: Business rule violations
   ```rust
   #[derive(Error, Debug, PartialEq, Eq, Clone)]
   pub enum UserDeciderError {
       #[error("User already exists")]
       UserAlreadyExists,

       #[error("Invalid name: {0}")]
       InvalidName(String),
   }
   ```

2. **Repository Errors**: Generic parameter allows backend flexibility
   ```rust
   pub trait VersionedEventRepository<E, Err> {
       async fn append(&mut self, events: &Vec<E>) -> Result<Vec<E>, Err>;
   }
   ```

3. **Version Conflicts**: Special handling in strategies
   ```rust
   pub enum VersionedRepositoryError<RepoErr, Version> {
       VersionConflict(VersionDiff<Version>),
       RepositoryError(RepoErr),
   }
   ```

**Use `thiserror` for Error Derivation**

```rust
use thiserror::Error;

#[derive(Error, Debug)]
pub enum MyError {
    #[error("Description of error")]
    VariantName,

    #[error("Error with context: {0}")]
    WithContext(String),

    #[error(transparent)]
    Wrapped(#[from] OtherError),
}
```

## Naming Conventions

### Traits

**Use Descriptive Nouns or Verb-Nouns**

```rust
// GOOD
pub trait Evolver { }
pub trait Decider { }
pub trait LoadDecideAppend { }
pub trait VersionedEventRepository { }

// AVOID: Prefixes like "I" or "T"
pub trait IDecider { }        // ❌
pub trait TRepository { }     // ❌
```

### Associated Types

**Use CapitalCase, Descriptive Names**

```rust
pub trait Decider {
    type Cmd;       // Clear abbreviation
    type Evt;       // Clear abbreviation
    type State;     // Full word
    type Err;       // Standard abbreviation
    type EntityId;  // Compound word
}
```

**Common Patterns:**
- `Cmd` / `Command`: Commands
- `Evt` / `Event`: Events
- `State`: Aggregate state
- `Err` / `Error`: Errors
- `Ctx` / `Context`: Dependency injection context
- `Version`: Optimistic locking version

### Functions & Methods

**Use snake_case**

```rust
// GOOD
fn event_type(&self) -> String { }
fn get_id(&self) -> Self::EntityId { }
fn evolve(state: Self::State, event: &Self::Evt) -> Self::State { }

// Standard prefixes:
// - get_: Accessor methods
// - from_: Conversion methods
// - into_: Consuming conversion methods
```

### Domain Types

**Enums as Algebraic Data Types (ADTs)**

```rust
// Commands: Verb-based variants
pub enum UserCommand {
    AddUser(AddUserCommand),
    UpdateUser(UpdateUserCommand),
    RemoveUser(RemoveUserCommand),
}

// Events: Past-tense variants
pub enum UserEvent {
    UserAdded { user_id: UserId, name: UserName },
    UserUpdated { user_id: UserId, name: Option<UserName> },
    UserRemoved { user_id: UserId },
}

// Errors: Descriptive names
pub enum UserDeciderError {
    UserNotFound,
    UserAlreadyExists,
    InvalidUserName(String),
}
```

**Value Objects: NewType Pattern**

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UserId(String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserName(String);

impl UserName {
    pub fn new(name: String) -> Result<Self, ValidationError> {
        // Validation logic
        Ok(UserName(name))
    }
}
```

## Trait Patterns

### Trait Hierarchies

**Build Incrementally**

```rust
// Base trait
pub trait Evolver {
    type State;
    type Evt: Event;
    fn evolve(state: Self::State, event: &Self::Evt) -> Self::State;
}

// Extension trait
pub trait Decider: Evolver {
    type Cmd: Send + Sync;
    type Err;
    fn decide(state: &Self::State, cmd: &Self::Cmd) -> Result<Vec<Self::Evt>, Self::Err>;
}

// Alternative extension (separate concern)
pub trait DeciderWithContext: Evolver {
    type Ctx: std::fmt::Debug;
    type Cmd: Send + Sync + std::fmt::Debug;
    type Err: std::fmt::Debug;
    fn decide(ctx: &Self::Ctx, state: &Self::State, cmd: &Self::Cmd)
        -> Result<Vec<Self::Evt>, Self::Err>;
}
```

### Generic Constraints

**Place Constraints at Trait Definition When Possible**

```rust
// GOOD: Constraints on associated type
pub trait Event {
    type EntityId: Clone + PartialEq;  // ✓
    fn get_id(&self) -> Self::EntityId;
}

// GOOD: Constraints on trait bound
pub trait Repository<E: Event + Serialize> {  // ✓
    // ...
}
```

**Use `where` Clauses for Complex Bounds**

```rust
// GOOD: Readable where clause
impl<D, R> LoadDecideAppend for MyStrategy<D, R>
where
    D: DeciderWithContext + Send + Sync,
    R: VersionedEventRepositoryWithStreams<D::Evt, MyError>,
    D::Evt: Clone + Send,
{
    // Implementation
}
```

### Async Traits

**Use `async-trait` Crate**

```rust
use async_trait::async_trait;

#[async_trait]
pub trait EventRepository<E, Err> {
    async fn load(&self) -> Result<Vec<E>, Err>;
    async fn append(&mut self, events: &Vec<E>) -> Result<Vec<E>, Err>;
}
```

**Lifetime Considerations**

```rust
// When generic lifetimes cause issues with async-trait:
#[async_trait]
pub trait VersionedEventRepositoryWithStreams<'a, E, Err>
where
    E: Event + Send + Sync,
    Self: Send + Sync,
{
    // Methods use 'a for borrowed data
}
```

## Code Organization

### Module Structure

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
    ├── deciders.rs         // Example domain
    └── repository.rs       // Repository spec tests
```

### File Organization Within Modules

1. **Imports**: Group by source (std, external crates, internal modules)
2. **Type Definitions**: Traits, then structs, then enums
3. **Implementations**: Group by type
4. **Tests**: At bottom with `#[cfg(test)]`

```rust
// 1. Imports
use std::collections::HashMap;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use crate::Event;

// 2. Trait definitions
pub trait MyTrait {
    // ...
}

// 3. Struct definitions
pub struct MyStruct {
    // ...
}

// 4. Enum definitions
pub enum MyEnum {
    // ...
}

// 5. Implementations
impl MyTrait for MyStruct {
    // ...
}

// 6. Tests
#[cfg(test)]
mod tests {
    // ...
}
```

## Feature Flags

**Use for Optional Dependencies**

```toml
[features]
default = ["in_memory", "esdb", "redis"]
in_memory = []
esdb = ["dep:eventstore", "dep:uuid", "dep:serde_json"]
redis = ["dep:redis-om"]
```

**Conditional Compilation**

```rust
#[cfg(feature = "esdb")]
pub mod esdb;

#[cfg(feature = "redis")]
pub mod redis;
```

## Testing Patterns

### Generic Spec Tests

**Write Tests That Work Across Implementations**

```rust
pub async fn versioned_event_repository_with_streams_spec<'a, Err, V>()
where
    V: Eq + std::fmt::Debug,
    Err: std::fmt::Debug,
{
    // Generic test implementation
}

// Use in concrete tests:
#[actix_rt::test]
async fn test_in_memory_repository() {
    versioned_event_repository_with_streams_spec::<MyError, usize>().await;
}
```

### Example Domains in test_helpers

**Provide Complete Working Examples**

```rust
// In test_helpers/deciders.rs
pub struct UserDecider;

impl Evolver for UserDecider {
    type State = UserState;
    type Evt = UserEvent;

    fn evolve(mut state: Self::State, event: &Self::Evt) -> Self::State {
        // Full implementation
    }
}

impl Decider for UserDecider {
    type Cmd = UserCommand;
    type Err = UserDeciderError;

    fn decide(state: &Self::State, cmd: &Self::Cmd) -> Result<Vec<Self::Evt>, Self::Err> {
        // Full implementation
    }
}
```

### Assertion Style

```rust
use assert_matches::assert_matches;

// Pattern matching assertions
assert_matches!(result, Ok(UserEvent::UserAdded { .. }));

// Equality assertions
assert_eq!(state.users.len(), 1);
assert!(state.users.contains_key(&user_id));
```

## Documentation

### Public API Documentation

**Doc Comments for All Public Items**

```rust
/// A repository for storing and retrieving events.
///
/// This trait provides the core operations for event persistence:
/// - Loading events from a stream
/// - Appending new events to a stream
///
/// # Type Parameters
///
/// * `E` - The event type, must implement [`Event`]
/// * `Err` - The error type for repository operations
///
/// # Example
///
/// ```rust
/// use epoch::{Event, EventRepository};
///
/// async fn example<R: EventRepository<MyEvent, MyError>>(repo: &mut R) {
///     let events = repo.load().await?;
/// }
/// ```
#[async_trait]
pub trait EventRepository<E, Err> {
    /// Loads all events from the repository.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the underlying storage operation fails.
    async fn load(&self) -> Result<Vec<E>, Err>;
}
```

### Internal Documentation

**Use Regular Comments for Implementation Notes**

```rust
impl MyStruct {
    fn complex_method(&self) {
        // This lifetime exists to work around async-trait compiler limitations
        // See: https://github.com/dtolnay/async-trait/issues/123

        // Calculate the base value
        let base = self.compute_base();

        // Apply transformation with retry logic
        self.retry_transform(base)
    }
}
```

## Serialization

### Serde Patterns

**Derive for Domain Types**

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserName(String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UserEvent {
    UserAdded {
        user_id: UserId,
        name: UserName,
    },
    UserUpdated {
        user_id: UserId,
        name: Option<UserName>,
    },
}
```

**Event Trait for Type Names**

```rust
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

## Async & Concurrency

### Async-First Design

**All Repository Operations Are Async**

```rust
#[async_trait]
pub trait Repository {
    async fn load(&self) -> Result<Data, Error>;
    async fn save(&mut self, data: Data) -> Result<(), Error>;
}
```

### Retry Logic

**Exponential Backoff for Version Conflicts**

```rust
pub async fn load_decide_append_with_retry<D, R>(
    decide_ctx: &D::Ctx,
    cmd: &D::Cmd,
    repo: &mut R,
    stream_id: &R::StreamId,
    max_retries: usize,
) -> Result<(Vec<D::Evt>, R::Version), VersionedRepositoryError<R::RepoErr, R::Version>>
where
    D: DeciderWithContext,
    R: VersionedEventRepositoryWithStreams<D::Evt, R::RepoErr>,
{
    let mut retry_count = 0;

    loop {
        match try_once(decide_ctx, cmd, repo, stream_id).await {
            Ok(result) => return Ok(result),
            Err(VersionedRepositoryError::VersionConflict(_)) if retry_count < max_retries => {
                retry_count += 1;
                let delay = Duration::from_millis(10 * 2_u64.pow(retry_count as u32));
                thread::sleep(delay);
            }
            Err(e) => return Err(e),
        }
    }
}
```

**Note**: Current alpha uses `thread::sleep` instead of `tokio::sleep` for pragmatic reasons. This may change in future versions.

## Anti-Patterns to Avoid

### 1. Mixing Pure and Impure Code

```rust
// ❌ BAD: Side effects in domain logic
impl Decider for UserDecider {
    fn decide(state: &Self::State, cmd: &Self::Cmd) -> Result<Vec<Self::Evt>, Self::Err> {
        println!("Processing command");  // ❌ Side effect
        // decision logic
    }
}

// ✓ GOOD: Pure domain logic
impl Decider for UserDecider {
    fn decide(state: &Self::State, cmd: &Self::Cmd) -> Result<Vec<Self::Evt>, Self::Err> {
        // Pure decision logic only
    }
}
```

### 2. String-Based Type Relationships

```rust
// ❌ BAD: Loses type safety
pub trait Event {
    fn entity_type(&self) -> String;
    fn get_id(&self) -> String;
}

// ✓ GOOD: Type-safe relationships
pub trait Event {
    type EntityId: Clone + PartialEq;
    fn get_id(&self) -> Self::EntityId;
}
```

### 3. Concrete Error Types in Generic Contexts

```rust
// ❌ BAD: Limits reusability
pub trait Repository<E> {
    async fn load(&self) -> Result<Vec<E>, std::io::Error>;  // ❌ Too specific
}

// ✓ GOOD: Generic error parameter
pub trait Repository<E, Err> {
    async fn load(&self) -> Result<Vec<E>, Err>;  // ✓ Flexible
}
```

### 4. Mutable State in Evolver

```rust
// ❌ BAD: Mutates and returns
fn evolve(mut state: Self::State, event: &Self::Evt) -> Self::State {
    state.users.insert(event.user_id.clone(), event.user.clone());
    state  // ❌ Unclear ownership
}

// ✓ GOOD: Clear ownership transfer
fn evolve(mut state: Self::State, event: &Self::Evt) -> Self::State {
    state.users.insert(event.user_id.clone(), event.user.clone());
    state  // ✓ Explicit return of owned value
}
```

## Dependencies

### Required Dependencies

```toml
[dependencies]
async-trait = "0.1"
serde = { version = "1.0", features = ["derive"] }
thiserror = "1.0"
```

### Optional Backend Dependencies

```toml
eventstore = { version = "3.0", optional = true }
redis-om = { version = "0.4", optional = true }
uuid = { version = "1.0", optional = true }
serde_json = { version = "1.0", optional = true }
```

### Dev Dependencies

```toml
[dev-dependencies]
actix-rt = "2.9"
assert_matches = "1.5"
```

## Version Management

### Semantic Versioning

- **Alpha**: 1.0.0-alpha.N (current)
- **Breaking changes**: Increment alpha number
- **New features**: Increment alpha number
- **Bug fixes**: Increment alpha number

### Changelog Expectations

- Document API changes in CHANGELOG.md
- Include migration guides for breaking changes
- Reference issue/PR numbers

## Summary Checklist

When contributing to Epoch, ensure:

- [ ] Pure functions for `decide` and `evolve` (no side effects)
- [ ] Associated types for domain relationships
- [ ] Generic error parameters in traits
- [ ] `async-trait` for async trait methods
- [ ] `thiserror` for error derivation
- [ ] `serde` derives for serializable types
- [ ] Feature flags for optional backends
- [ ] Doc comments for public API
- [ ] Generic spec tests for repository implementations
- [ ] Event trait implementation with `event_type` and `get_id`
- [ ] Proper trait bounds (Send, Sync where needed)
- [ ] Follow naming conventions (snake_case functions, CapitalCase types)
- [ ] Keep README examples up to date with API changes

---

This style guide reflects the patterns established in Epoch v1.0.0-alpha.18 and should evolve as the library matures.
