# PostgreSQL Repository Implementation Plan

> **Internal Planning Document**
> Plan for implementing a PostgreSQL backend for Epoch's event sourcing framework

**Created**: 2025-11-21
**Status**: Planning
**Target**: Epoch 1.0.0-alpha.19 or later

---

## Overview

Add PostgreSQL as a fourth backend option for Epoch, alongside in-memory, EventStoreDB, and Redis. This enables production-grade event sourcing with a traditional relational database that many teams already use.

## Motivation

### Why PostgreSQL

1. **Ubiquity**: Most teams already have PostgreSQL expertise and infrastructure
2. **ACID Compliance**: Strong consistency guarantees out of the box
3. **Mature Ecosystem**: Well-tested drivers, connection pooling, migrations
4. **Cost-Effective**: No need for specialized event store infrastructure
5. **Queryability**: SQL access to events for analytics and debugging
6. **Transactions**: Native support for atomic multi-event writes

### Use Cases

- **Teams without EventStoreDB**: Lower barrier to entry for event sourcing
- **Analytics Requirements**: SQL access to event history
- **Cost Sensitivity**: Avoid additional infrastructure
- **Hybrid Architectures**: Use PostgreSQL for some aggregates, EventStoreDB for others
- **Development/Testing**: Simpler local development setup

---

## Architecture

### Implementation Location

```
src/repository/postgres/
├── mod.rs                          // Public API, PgEventRepository struct
├── error.rs                        // PostgreSQL-specific errors
├── version.rs                      // PostgreSQL version type (sequence number)
├── queries/                        // SQL query modules
│   ├── load_events.sql            // Load events query
│   ├── save_events.sql            // Append events query
│   └── mod.rs                     // Query builders
└── schema.sql                     // Database schema (for documentation)
```

### Core Types

#### Repository Struct

```rust
#[derive(Clone)]
pub struct PgEventRepository<E> {
    pool: Pool<PostgresConnectionManager<NoTls>>,  // bb8 connection pool
    table_prefix: String,                           // e.g., "events_"
    _phantom: PhantomData<E>,
}
```

#### Version Type

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PgVersion {
    sequence: i64,  // PostgreSQL BIGSERIAL
}

impl From<i64> for PgVersion {
    fn from(seq: i64) -> Self {
        Self { sequence: seq }
    }
}
```

#### Error Type

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PgRepositoryError {
    #[error("Connection error: {0}")]
    Connection(#[from] tokio_postgres::Error),

    #[error("Connection pool error: {0}")]
    Pool(#[from] bb8::RunError<tokio_postgres::Error>),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Event not found for stream: {0}")]
    NotFound(String),

    #[error("Sequence mismatch: expected {expected}, got {actual}")]
    SequenceMismatch { expected: i64, actual: i64 },
}
```

---

## Database Schema

### Events Table

```sql
CREATE TABLE IF NOT EXISTS events (
    -- Primary key
    id BIGSERIAL PRIMARY KEY,

    -- Stream identification
    stream_name VARCHAR(255) NOT NULL,      -- e.g., "user-123"
    stream_type VARCHAR(100) NOT NULL,      -- e.g., "User"

    -- Event metadata
    event_type VARCHAR(255) NOT NULL,       -- e.g., "UserAdded"
    event_version INTEGER NOT NULL DEFAULT 1,

    -- Sequence and ordering
    sequence BIGINT NOT NULL,               -- Stream-specific sequence
    global_sequence BIGSERIAL NOT NULL,     -- Global ordering

    -- Event data
    event_data JSONB NOT NULL,              -- Serialized event
    metadata JSONB,                          -- Optional metadata

    -- Timestamps
    created_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT NOW(),

    -- Constraints
    CONSTRAINT unique_stream_sequence UNIQUE (stream_name, sequence)
);

-- Indexes for common queries
CREATE INDEX idx_events_stream_name ON events(stream_name);
CREATE INDEX idx_events_stream_type ON events(stream_type);
CREATE INDEX idx_events_created_at ON events(created_at);
CREATE INDEX idx_events_global_sequence ON events(global_sequence);

-- Efficient JSONB queries (if needed)
CREATE INDEX idx_events_event_data ON events USING GIN (event_data);
```

### Design Decisions

**stream_name vs aggregate_id**:
- Use `stream_name` to match Epoch terminology
- Format: `{stream_type}-{entity_id}` (e.g., "User-550e8400...")

**sequence vs global_sequence**:
- `sequence`: Per-stream ordering (for optimistic concurrency)
- `global_sequence`: Global event ordering (for projections)

**JSONB for event_data**:
- Flexible schema evolution
- Query capability if needed
- Efficient storage

**No soft deletes**:
- Events are immutable
- No deletion support (by design)

---

## Trait Implementation

### VersionedEventRepositoryWithStreams

```rust
#[async_trait]
impl<'a, E> VersionedEventRepositoryWithStreams<'a, E, PgRepositoryError>
    for PgEventRepository<E>
where
    E: Event + Sync + Send + Serialize + DeserializeOwned + Clone + Debug,
{
    type StreamId = String;
    type Version = PgVersion;

    async fn load(
        &self,
        id: Option<&Self::StreamId>,
    ) -> Result<(Vec<E>, RepositoryVersion<PgVersion>), VersionedRepositoryError<PgRepositoryError, PgVersion>> {
        self.load_from_version(&RepositoryVersion::Any, id).await
    }

    async fn load_from_version(
        &self,
        version: &RepositoryVersion<PgVersion>,
        id: Option<&Self::StreamId>,
    ) -> Result<(Vec<E>, RepositoryVersion<PgVersion>), VersionedRepositoryError<PgRepositoryError, PgVersion>> {
        let conn = self.pool.get().await
            .map_err(PgRepositoryError::Pool)
            .map_err(VersionedRepositoryError::RepoErr)?;

        let stream_name = id.ok_or_else(|| {
            VersionedRepositoryError::RepoErr(PgRepositoryError::NotFound("no stream id".to_string()))
        })?;

        // Build query based on version
        let (query, params) = match version {
            RepositoryVersion::Any => {
                // Load all events for stream
                (
                    "SELECT event_data, sequence FROM events WHERE stream_name = $1 ORDER BY sequence ASC",
                    vec![stream_name as &(dyn ToSql + Sync)],
                )
            }
            RepositoryVersion::Exact(v) => {
                // Load events after specific version
                (
                    "SELECT event_data, sequence FROM events WHERE stream_name = $1 AND sequence > $2 ORDER BY sequence ASC",
                    vec![stream_name as &(dyn ToSql + Sync), &v.sequence as &(dyn ToSql + Sync)],
                )
            }
            _ => {
                return Ok((vec![], RepositoryVersion::NoStream));
            }
        };

        let rows = conn.query(query, &params).await
            .map_err(PgRepositoryError::Connection)
            .map_err(VersionedRepositoryError::RepoErr)?;

        if rows.is_empty() {
            return Ok((vec![], RepositoryVersion::NoStream));
        }

        let mut events = Vec::new();
        let mut last_version = RepositoryVersion::StreamExists;

        for row in rows {
            let event_data: serde_json::Value = row.get("event_data");
            let sequence: i64 = row.get("sequence");

            let event: E = serde_json::from_value(event_data)
                .map_err(PgRepositoryError::Serialization)
                .map_err(VersionedRepositoryError::RepoErr)?;

            events.push(event);
            last_version = RepositoryVersion::Exact(PgVersion::from(sequence));
        }

        Ok((events, last_version))
    }

    async fn append(
        &mut self,
        version: &RepositoryVersion<PgVersion>,
        stream: &Self::StreamId,
        events: &Vec<E>,
    ) -> Result<(Vec<E>, RepositoryVersion<PgVersion>), VersionedRepositoryError<PgRepositoryError, PgVersion>>
    where
        'a: 'async_trait,
        E: 'async_trait,
    {
        let mut conn = self.pool.get().await
            .map_err(PgRepositoryError::Pool)
            .map_err(VersionedRepositoryError::RepoErr)?;

        // Start transaction
        let transaction = conn.transaction().await
            .map_err(PgRepositoryError::Connection)
            .map_err(VersionedRepositoryError::RepoErr)?;

        // Get current sequence (for optimistic concurrency check)
        let current_seq_query = "SELECT COALESCE(MAX(sequence), 0) as max_seq FROM events WHERE stream_name = $1";
        let row = transaction.query_one(current_seq_query, &[stream]).await
            .map_err(PgRepositoryError::Connection)
            .map_err(VersionedRepositoryError::RepoErr)?;

        let current_sequence: i64 = row.get("max_seq");
        let current_version = if current_sequence == 0 {
            RepositoryVersion::NoStream
        } else {
            RepositoryVersion::Exact(PgVersion::from(current_sequence))
        };

        // Check version (optimistic concurrency control)
        match version {
            RepositoryVersion::Exact(expected) if expected.sequence != current_sequence => {
                return Err(VersionedRepositoryError::VersionConflict(VersionDiff::new(
                    *version,
                    current_version,
                )));
            }
            RepositoryVersion::NoStream if current_sequence != 0 => {
                return Err(VersionedRepositoryError::VersionConflict(VersionDiff::new(
                    *version,
                    current_version,
                )));
            }
            _ => {}
        }

        // Insert events
        let insert_query = "
            INSERT INTO events (stream_name, stream_type, event_type, sequence, event_data)
            VALUES ($1, $2, $3, $4, $5)
        ";

        let stream_type = events.first()
            .map(|e| e.event_type())
            .unwrap_or_else(|| "Unknown".to_string());

        let mut next_sequence = current_sequence;
        for event in events {
            next_sequence += 1;

            let event_data = serde_json::to_value(event)
                .map_err(PgRepositoryError::Serialization)
                .map_err(VersionedRepositoryError::RepoErr)?;

            transaction.execute(
                insert_query,
                &[stream, &stream_type, &event.event_type(), &next_sequence, &event_data],
            ).await
                .map_err(PgRepositoryError::Connection)
                .map_err(VersionedRepositoryError::RepoErr)?;
        }

        // Commit transaction
        transaction.commit().await
            .map_err(PgRepositoryError::Connection)
            .map_err(VersionedRepositoryError::RepoErr)?;

        Ok((
            events.clone(),
            RepositoryVersion::Exact(PgVersion::from(next_sequence)),
        ))
    }
}
```

---

## Implementation Steps

### Phase 1: Foundation (2-3 hours)

1. **Create module structure**
   - [ ] Create `src/repository/postgres/` directory
   - [ ] Add `mod.rs` with basic exports
   - [ ] Add `error.rs` with `PgRepositoryError` using thiserror
   - [ ] Add `version.rs` with `PgVersion` type

2. **Add dependencies to Cargo.toml**
   ```toml
   [dependencies]
   # PostgreSQL (optional feature)
   tokio-postgres = { version = "0.7", optional = true, features = ["with-serde_json-1"] }
   bb8 = { version = "0.8", optional = true }
   bb8-postgres = { version = "0.8", optional = true }

   [features]
   postgres = ["dep:tokio-postgres", "dep:bb8", "dep:bb8-postgres"]
   ```

3. **Define schema**
   - [ ] Create `schema.sql` with events table
   - [ ] Document in comments (not executed by code)

### Phase 2: Core Implementation (3-4 hours)

4. **Implement PgEventRepository struct**
   - [ ] Connection pooling setup
   - [ ] Constructor with connection string
   - [ ] Helper methods for stream naming

5. **Implement VersionedEventRepositoryWithStreams**
   - [ ] `load()` method
   - [ ] `load_from_version()` method
   - [ ] `append()` with optimistic concurrency

6. **Error handling**
   - [ ] Connection errors
   - [ ] Serialization errors
   - [ ] Version conflict errors
   - [ ] Map to `VersionedRepositoryError`

### Phase 3: Testing (2-3 hours)

7. **Generic spec tests**
   - [ ] Add PostgreSQL test setup in `src/repository/postgres/mod.rs`
   - [ ] Use `test_helpers::repository::versioned_event_repository_with_streams_spec`
   - [ ] Use `test_helpers::repository::versioned_event_repository_with_streams_occ_spec`

8. **Integration tests**
   - [ ] Test with UserEvent from test_helpers
   - [ ] Test version conflict scenarios
   - [ ] Test stream isolation

9. **Docker Compose for testing**
   ```yaml
   version: '3.8'
   services:
     postgres:
       image: postgres:15
       environment:
         POSTGRES_PASSWORD: postgres
         POSTGRES_DB: epoch_test
       ports:
         - "5432:5432"
   ```

### Phase 4: Documentation (1-2 hours)

10. **Update documentation**
    - [ ] Add PostgreSQL section to architecture doc
    - [ ] Update README.md with PostgreSQL example
    - [ ] Add to coding style guide if new patterns emerge

11. **Example code**
    - [ ] Create example in README showing PostgreSQL usage
    - [ ] Document connection string format
    - [ ] Document schema setup requirements

12. **Update CHANGELOG.md**
    - [ ] Add to Unreleased section under "Added"

---

## Usage Example

```rust
use epoch::repository::postgres::PgEventRepository;
use tokio_postgres::NoTls;

// Create repository
let connection_string = "postgresql://user:password@localhost/epoch_db";
let repository = PgEventRepository::<UserEvent>::connect(connection_string, NoTls)
    .await
    .expect("Failed to connect to PostgreSQL");

// Use with strategy
use epoch::strategies::LoadDecideAppend;

let strategy = MyStrategy {
    repository,
    decider: UserDecider,
};

let result = strategy.execute(context, command).await?;
```

---

## Railway-Oriented Programming Patterns

### Error Handling

Following our Railway-Oriented Programming principles:

```rust
async fn load_events(&self, stream: &str) -> Result<Vec<E>, PgRepositoryError> {
    // Each ? is a "switch" to error track
    let conn = self.pool.get().await?;  // Connection error track

    let rows = conn.query(LOAD_QUERY, &[stream]).await?;  // Query error track

    let events = rows.iter()
        .map(|row| self.deserialize_event(row))  // Deserialization error track
        .collect::<Result<Vec<_>, _>>()?;

    Ok(events)  // Success track
}
```

### Smart Constructors

```rust
impl PgEventRepository<E> {
    /// Create repository with validation
    pub async fn connect(
        connection_string: &str,
        tls: NoTls,
    ) -> Result<Self, PgRepositoryError> {
        // Validate connection string
        let config = connection_string.parse()
            .map_err(PgRepositoryError::Connection)?;

        // Create pool
        let manager = PostgresConnectionManager::new(config, tls);
        let pool = Pool::builder()
            .build(manager)
            .await
            .map_err(PgRepositoryError::Pool)?;

        Ok(Self {
            pool,
            table_prefix: "events".to_string(),
            _phantom: PhantomData,
        })
    }
}
```

---

## Design Decisions

### 1. Use bb8 for Connection Pooling

**Why**: Industry standard, async, well-maintained

**Alternatives considered**:
- deadpool: Similar functionality, chose bb8 for consistency with Thalo
- r2d2: Synchronous only

### 2. JSONB for Event Storage

**Why**:
- Flexible schema evolution
- Query capability
- Efficient storage
- Native PostgreSQL support

**Alternatives considered**:
- TEXT with JSON: Less efficient queries
- BYTEA with bincode: Not human-readable, harder to debug

### 3. Per-Stream Sequence Numbers

**Why**:
- Optimistic concurrency control
- Stream isolation
- Matches EventStoreDB semantics

**Alternatives considered**:
- Global sequence only: No per-stream concurrency control
- UUID-based versioning: Less efficient, no ordering

### 4. Transactions for Append

**Why**:
- ACID guarantees
- Atomic multi-event writes
- Version check + insert as unit

**Trade-offs**:
- Slightly lower throughput vs batching
- Acceptable for typical event sourcing loads

---

## Performance Considerations

### Indexing Strategy

**Essential indexes**:
- `(stream_name, sequence)` - UNIQUE constraint + query optimization
- `stream_name` - Load events for stream
- `global_sequence` - Global ordering for projections

**Optional indexes**:
- `created_at` - Time-based queries
- `stream_type` - Category queries
- `event_data` GIN - JSONB queries (add only if needed)

### Connection Pooling

**Default pool settings**:
```rust
Pool::builder()
    .max_size(20)                    // Max connections
    .min_idle(Some(5))               // Min idle connections
    .connection_timeout(Duration::from_secs(30))
    .build(manager)
    .await?
```

**Tuning guidance**:
- Adjust based on workload
- Monitor connection usage
- Consider pgbouncer for very high loads

### Query Optimization

**Load queries**:
- Use prepared statements (implicit in tokio-postgres)
- Index on `(stream_name, sequence)`
- Limit to necessary columns

**Append queries**:
- Batch inserts in transaction
- Use RETURNING clause for confirmation
- Minimal transaction scope

---

## Migration from Other Backends

### From In-Memory

```rust
// Before
let repository = InMemoryEventRepository::<UserEvent>::new();

// After
let repository = PgEventRepository::<UserEvent>::connect(
    "postgresql://localhost/epoch_db",
    NoTls,
).await?;
```

### From EventStoreDB

**Schema migration**:
- Export events from ESDB
- Transform to PostgreSQL format
- Import with correct sequence numbers

**Code changes**:
- Replace `ESDBEventRepository` with `PgEventRepository`
- Update connection setup
- Stream naming compatible

### From Redis

**Version type change**:
- Redis: `RedisVersion { timestamp, version }`
- PostgreSQL: `PgVersion { sequence }`

**Data migration**:
- Export from Redis Streams
- Map stream IDs
- Import with monotonic sequence

---

## Testing Strategy

### Unit Tests

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pg_version_ordering() {
        let v1 = PgVersion::from(1);
        let v2 = PgVersion::from(2);
        assert!(v1 < v2);
    }

    #[test]
    fn error_contains_context() {
        let err = PgRepositoryError::SequenceMismatch {
            expected: 5,
            actual: 3,
        };
        assert!(err.to_string().contains("5"));
        assert!(err.to_string().contains("3"));
    }
}
```

### Integration Tests

```rust
#[actix_rt::test]
async fn test_pg_repository_spec() {
    // Setup test database
    let conn_string = env::var("PG_TEST_CONNECTION")
        .unwrap_or_else(|_| "postgresql://postgres:postgres@localhost/epoch_test".to_string());

    let repository = PgEventRepository::<UserEvent>::connect(&conn_string, NoTls)
        .await
        .expect("Failed to connect");

    // Run generic spec tests
    versioned_event_repository_with_streams_spec(repository).await;
}
```

### Test Database Setup

```bash
# Create test database
createdb epoch_test

# Run schema
psql epoch_test < src/repository/postgres/schema.sql

# Set environment variable
export PG_TEST_CONNECTION="postgresql://localhost/epoch_test"

# Run tests
cargo test --features postgres
```

---

## Known Limitations

### Current Scope

**Not Included in Initial Implementation**:
- [ ] Projections (separate concern)
- [ ] Event upcasting (future feature)
- [ ] Snapshot support (separate PR)
- [ ] Outbox pattern (can add later)
- [ ] Event publishing (separate concern)

**Rationale**: Focus on core event storage first, iterate based on usage.

### PostgreSQL-Specific Constraints

**Maximum connections**:
- Limited by PostgreSQL configuration
- Use connection pooling
- Consider pgbouncer for scale

**Event size**:
- JSONB has practical size limits (~1MB)
- Design events to be small and focused
- Split large payloads if needed

**Global ordering**:
- BIGSERIAL provides ordering
- Not guaranteed across distributed writes
- Use timestamps for distributed systems

---

## Future Enhancements

### Phase 2 Features

**Projections**:
- Read model generation
- Subscribe to global event stream
- Materialize views

**Snapshots**:
- State repository implementation
- Periodic snapshot creation
- Snapshot + events loading

**Outbox Pattern**:
- Reliable event publishing
- Transaction-safe message dispatch
- Integration with message brokers

### Phase 3 Features

**Event Upcasting**:
- Version migration
- Schema evolution
- Backward compatibility

**Multi-tenancy**:
- Schema-per-tenant
- Table partitioning
- Row-level security

**Advanced Queries**:
- Event type filtering
- Time-range queries
- JSONB path queries
- Full-text search

---

## References

### Internal

- [Epoch Architecture Philosophy](./epoch-architecture-philosophy.md)
- [Coding Style Guide](./coding-style-guide.md)
- [ESDB Repository Implementation](../../../src/repository/esdb/mod.rs)
- [Redis Repository Implementation](../../../src/repository/redis/mod.rs)

### External

- [Thalo PostgreSQL Implementation](https://github.com/Shearerbeard/thalo/tree/thalo-eventstoredb/thalo-postgres)
- [tokio-postgres Documentation](https://docs.rs/tokio-postgres)
- [bb8 Connection Pool](https://docs.rs/bb8)
- [PostgreSQL JSONB](https://www.postgresql.org/docs/current/datatype-json.html)
- [Event Sourcing Patterns](https://martinfowler.com/eaaDev/EventSourcing.html)

---

## Checklist for Completion

### Implementation
- [ ] Module structure created
- [ ] Dependencies added to Cargo.toml
- [ ] PgEventRepository struct implemented
- [ ] VersionedEventRepositoryWithStreams trait implemented
- [ ] Error types defined with thiserror
- [ ] Connection pooling working
- [ ] Optimistic concurrency working

### Testing
- [ ] Generic spec tests passing
- [ ] Integration tests passing
- [ ] Docker Compose setup documented
- [ ] Version conflict tests passing
- [ ] Stream isolation verified

### Documentation
- [ ] Architecture doc updated
- [ ] README example added
- [ ] Coding style guide updated (if needed)
- [ ] Schema documented
- [ ] Migration guide written
- [ ] CHANGELOG.md updated

### Code Quality
- [ ] Follows Railway-Oriented Programming patterns
- [ ] Uses smart constructors
- [ ] No primitive obsession
- [ ] NO EMOJIS in code or docs
- [ ] Passes cargo clippy
- [ ] Passes cargo fmt

---

**Status**: Ready for implementation
**Estimated Effort**: 8-12 hours total
**Target Release**: 1.0.0-alpha.19

This planning document provides a complete roadmap for adding PostgreSQL support to Epoch while maintaining consistency with existing patterns and architectural principles.
