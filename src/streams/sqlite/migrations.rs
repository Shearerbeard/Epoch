//! Versioned schema migrations for the SQLite streams tables.
//!
//! Steps are ordered, named SQL files under `migrations/`, embedded
//! verbatim, and recorded in an applied ledger
//! (`epoch_schema_migrations`) inside the database. A database created
//! at any earlier shape reaches the current schema through
//! [`super::SqliteEventStreams::migrate`] alone.
//!
//! A step file is IMMUTABLE once any database has applied it. An edit
//! to an applied step retroactively changes what fresh databases get,
//! while applied ones keep the old shape - identical ledgers,
//! divergent schemas. Schema changes arrive as new numbered step
//! files, never as edits to existing ones; the current shape of the
//! schema is the composition of the steps, readable in order.
//!
//! The whole run - ledger read, pending steps, ledger writes - is one
//! transaction under SQLite's database-wide write lock, so concurrent
//! callers serialize and the later ones find nothing pending. DDL in
//! SQLite is transactional, so a failed step rolls everything back, the
//! ledger table included.

#![allow(dead_code)]

use std::time::Duration;

use tokio_rusqlite::rusqlite;

use super::SqliteStreamsError;

/// One numbered schema step: the version the ledger records, the name
/// the ledger carries, and the SQL the step runs.
#[derive(Clone, Copy)]
struct Migration {
    version: i64,
    name: &'static str,
    sql: &'static str,
}

/// The steps this backend applies, in version order. The step files
/// arrive with the schema; the runner's behavior is the hole this
/// module carries.
static MIGRATIONS: &[Migration] = &[];

/// Apply every step the database has not recorded, in version order,
/// recording each in the ledger inside the same transaction that ran
/// its DDL.
#[expect(unused_variables, reason = "todo!() body; filled by E28")]
pub(super) fn apply(
    conn: &mut rusqlite::Connection,
    lock_timeout: Duration,
) -> Result<(), SqliteStreamsError> {
    todo!("E28: migration runner")
}
