//! Golden frames for the saga runner and the outbox executor:
//! whole-frame spec tests against the in-memory backend. The coverage
//! manifest, including the exclusion rows, lives in
//! `docs/design/e20-saga-outbox-DESIGN.md`.
//!
//! Whole frame means the complete stored state, not substrings: each
//! assertion compares a full stream's records - payloads and envelopes
//! together - against an expected `RecordedEvent` list built by hand
//! from the spec.
//!
//! ```sh
//! cargo test --lib saga_outbox_golden
//! ```

mod executor;
mod runner;
mod support;
