# Epoch - TODO

## High Priority

### Replace `redis-om` dependency
- **Status**: Planned
- **Reason**: `redis-om` (v0.1.0) is unmaintained and pins `redis` to v0.22.3, which has future-incompatibility warnings for Rust 2024 edition
- **Approach**: Migrate to direct `redis` crate usage with `streams` and `json` features
- **Files affected**:
  - `src/repository/redis/versioned_event.rs` - Uses `StreamModel`
  - `src/repository/redis/versioned_stream_snapshot.rs` - Uses `JsonModel`
  - `src/repository/redis/mod.rs` - Uses `RedisError`
  - `src/test_helpers/redis.rs` - Test DTOs
- **Estimated effort**: 4-7 hours
- **Reference**: See research notes in session that created this file

## Medium Priority

### Add `rust-version` to Cargo.toml
- **Status**: Planned
- **Reason**: Document MSRV (minimum supported Rust version) for dependency resolution
- **Approach**: Add `rust-version = "1.75"` (or appropriate version) to `[package]` section

### Consider `cargo-deny` integration
- **Status**: Planned
- **Reason**: Audit dependencies for security and license issues
- **Approach**: Add `deny.toml` and CI check

## Low Priority

### Update docker-compose.yml
- **Status**: Minor
- **Reason**: Remove obsolete `version` attribute (causes warning)
- **Approach**: Delete line 1 (`version: "3.9"`)

### EventStoreDB image version
- **Status**: Investigate
- **Reason**: Using `eventstore/eventstore:20.10.2-buster-slim` which is old
- **Approach**: Test with newer ES image, verify compatibility with `eventstore` crate v4.0.0

## Completed

- [x] Modernize dependencies (2026-01-31)
- [x] Fix clippy warnings (2026-01-31)
- [x] Migrate `dotenv` → `dotenvy` (2026-01-31)
- [x] Add CLAUDE.md project guidelines (2026-01-31)
- [x] Set up `.claude/` directory structure (2026-01-31)
