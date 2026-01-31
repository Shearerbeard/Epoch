# Epoch - Claude Code Guidelines

## Project Overview

Epoch is an Event Sourcing + CQRS framework for Rust, inspired by Thalo, Eventful (Haskell), Equinox (F#), and f(model) (Kotlin).

## Event Sourcing Core Principles

This framework implements event sourcing patterns. All contributors should understand:

- **Events are immutable facts**: Once stored, events never change. They represent things that happened, not things that might happen.
- **Events are the source of truth**: State is always derived by replaying events, never stored directly.
- **One stream per aggregate**: Events in a stream belong to a single aggregate root. Use `StreamIdFromEvent` to ensure proper partitioning.
- **Evolve functions must be total**: The `evolve` function should never panic. Make invalid state transitions unrepresentable through types.

### Event Design Guidelines

- Events should be versioned at the type level when schemas change (e.g., `UserCreatedV1` → `UserCreatedV2`)
- Avoid making fields `Option` just to handle versioning - create new event variants instead
- All events must include their entity ID to support stream partitioning
- Commands should be idempotent or include deduplication IDs for safe retries

### The Decider Pattern

This framework uses the Decider pattern with three core functions:
- `decide(cmd, state) -> Result<Vec<Event>, Error>` - Validates commands against current state
- `evolve(state, event) -> State` - Applies events to produce new state (must be pure and total)
- `initial_state() -> State` - Provides the starting state for an aggregate

## Rust Style Guidelines

### Strong Typing Philosophy

This project embraces strong, expressive types. Follow these principles:

- **Prefer newtypes over primitives**: Wrap primitive types in newtype structs to add semantic meaning and prevent mixing unrelated values of the same underlying type.
  ```rust
  // Prefer this
  pub struct UserId(String);
  pub struct UserName(String);

  // Over this
  fn create_user(id: String, name: String) // Easy to mix up!
  ```

- **Protected constructors**: Use private fields with public constructor functions that validate invariants. Return `Result` types when construction can fail.
  ```rust
  pub struct Email(String);

  impl Email {
      pub fn new(value: impl Into<String>) -> Result<Self, EmailError> {
          let s = value.into();
          if s.contains('@') {
              Ok(Self(s))
          } else {
              Err(EmailError::InvalidFormat)
          }
      }
  }
  ```

- **Make impossible states unrepresentable**: Use enums and type system features to ensure invalid states cannot be constructed at compile time.
  ```rust
  // Prefer this - states are explicit and exhaustive
  pub enum OrderState {
      Pending { created_at: DateTime },
      Confirmed { confirmed_at: DateTime },
      Shipped { tracking: TrackingNumber },
      Delivered { delivered_at: DateTime },
  }

  // Over this - allows invalid combinations
  pub struct Order {
      is_confirmed: bool,
      is_shipped: bool,
      tracking: Option<String>, // Can be Some when not shipped!
  }
  ```

- **Use phantom types to prevent ID mixing**: Even if two IDs are both `usize` internally, they should be distinct types.
  ```rust
  pub struct UserId(usize);
  pub struct AccountId(usize);
  // These cannot be accidentally swapped at call sites
  ```

### General Rust Conventions

- Use `thiserror` for library error types
- Prefer `&[T]` over `&Vec<T>` in function signatures
- Implement `Display` instead of `ToString`
- When implementing `Ord` and `PartialOrd`, ensure `partial_cmp` delegates to `cmp` and `cmp` does not use comparison operators
- Use `async-trait` for async trait methods
- Avoid `.unwrap()` in `decide`/`evolve` functions - invalid states should be prevented by types
- The framework is async-runtime agnostic; tests use `actix-rt` but callers can use any runtime

## Pre-commit Workflow

The following checks are part of the pre-commit workflow and must pass:

```bash
cargo fmt                      # Format code
cargo clippy --all-features    # Lint checks
cargo test --all-features      # All tests including integration
```

## Commit Guidelines

- **DO** prompt the user before creating commits
- **DO NOT** commit on the user's behalf without explicit approval
- Provide clear commit message suggestions following conventional commits style

## Features

The crate has the following optional features:
- `in_memory` - In-memory repository implementations (default)
- `esdb` - EventStoreDB repository implementation
- `redis` - Redis repository implementation (uses `redis-om`)

## Known Issues

- `redis-om` (v0.1.0) is unmaintained and depends on `redis v0.22.3` which has future-incompatibility warnings for Rust 2024 edition. Migration to direct `redis` crate usage is on the roadmap.

## Testing

Integration tests for Redis and ESDB require running services. Use docker-compose:

```bash
docker compose up -d
```

Create a `.env` file with connection strings:
```
REDIS_CONNECTION_STRING=redis://127.0.0.1:6379
ESDB_CONNECTION_STRING=esdb://localhost:2113?tls=false
```

Run in-memory tests only:
```bash
cargo test --no-default-features --features in_memory
```

## Session Management

**Starting a session:**
1. Read `HANDOFF.md` for current project state
2. Check `TODO.md` for pending work
3. Verify environment: `cargo test --all-features`

**Ending a session:**
1. Update `HANDOFF.md` with session summary
2. Add new discoveries to `TODO.md`
3. Commit both files with your changes
