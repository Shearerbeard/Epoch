//! Live PostgreSQL replay tests for the saga runner and outbox executor.
//! They cover the storage-enforced intent uniqueness and feed positions
//! that the in-memory harness cannot prove. Each scenario uses unique
//! stream keys, and the executor steps past other sagas' outbox entries.
//!
//! ```sh
//! cargo test --features postgres --test saga_outbox_postgres -- --nocapture
//! ```

#![cfg(feature = "postgres")]

mod support;
mod windows;
mod wire_format;
