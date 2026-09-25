//! The live-postgres crash table: the five crash windows of the saga
//! runner and the outbox executor, proven against the live compose
//! postgres - the store whose intent-key uniqueness index and global
//! feed positions the in-memory golden harness
//! (`src/streams/saga_outbox_golden/`) cannot exhibit. The consumer
//! harness here mirrors the golden's shapes - `Src`/`Cmd`/`Fx`, a
//! scripted saga, a `ReactionFold`, an `EffectPort`, a
//! `CompensationHook` - but self-contained and postgres-shaped, with
//! serde derives on the consumer types because the pg wire form is
//! JSONB.
//!
//! The suite migrates the schema once per scenario and writes only
//! ULID-nonce ids, so runs never collide and storage is never
//! cleared. Two namespaces see cross-run traffic by design: the
//! `ledger` category (per-run unique stream keys) and the
//! framework-owned `saga-outbox` category, whose feed carries every
//! prior run's rows. The executor scenarios therefore step in bounded
//! loops - each step skips and acks foreign entries - until THIS
//! scenario's expected state holds.
//!
//! ```sh
//! cargo test --features postgres --test e20_saga_outbox_postgres -- --nocapture
//! ```

#![cfg(feature = "postgres")]

mod support;
mod windows;
mod wire_format;
