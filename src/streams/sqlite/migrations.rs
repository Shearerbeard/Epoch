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

use std::collections::HashSet;
use std::time::Duration;

use tokio_rusqlite::rusqlite;

use super::{classify_lock_failure, install_busy_timeout, SqliteStreamsError};

/// One numbered schema step: the version the ledger records, the name
/// the ledger carries, and the SQL the step runs.
#[derive(Clone, Copy)]
struct Migration {
    version: i64,
    name: &'static str,
    sql: &'static str,
}

/// The steps this backend applies, in version order.
static MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "create-stream-events",
        sql: include_str!("migrations/0001-create-stream-events.sql"),
    },
    Migration {
        version: 2,
        name: "feed-cursors",
        sql: include_str!("migrations/0002-feed-cursors.sql"),
    },
    Migration {
        version: 3,
        name: "outbox-intent-index",
        sql: include_str!("migrations/0003-outbox-intent-index.sql"),
    },
];

/// The applied ledger. Its own IF NOT EXISTS is safe because it runs
/// inside the migration transaction, before any ledger read.
const ENSURE_LEDGER: &str = "CREATE TABLE IF NOT EXISTS epoch_schema_migrations (
    version INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
)";

/// Apply every step the database has not recorded, in version order,
/// recording each in the ledger inside the same transaction that ran
/// its DDL.
pub(super) fn apply(
    conn: &mut rusqlite::Connection,
    lock_timeout: Duration,
) -> Result<(), SqliteStreamsError> {
    apply_steps(conn, MIGRATIONS, lock_timeout)
}

/// Apply the given steps, in version order, recording each in the
/// ledger inside the same transaction that ran its DDL. The step list
/// is a parameter so a test can inject a failing step.
fn apply_steps(
    conn: &mut rusqlite::Connection,
    steps: &[Migration],
    lock_timeout: Duration,
) -> Result<(), SqliteStreamsError> {
    // The effective bound goes in before the transaction opens: the
    // write lock is taken by the BEGIN itself, so a bound installed
    // after it would bound nothing.
    install_busy_timeout(conn, lock_timeout)
        .map_err(|error| classify_lock_failure(error, lock_timeout))?;

    let transaction = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| classify_lock_failure(error, lock_timeout))?;

    transaction
        .execute_batch(ENSURE_LEDGER)
        .map_err(|error| classify_lock_failure(error, lock_timeout))?;

    let applied = applied_versions(&transaction)?;
    for step in steps.iter().filter(|step| !applied.contains(&step.version)) {
        transaction
            .execute_batch(step.sql)
            .map_err(|error| classify_lock_failure(error, lock_timeout))?;
        transaction
            .execute(
                "INSERT INTO epoch_schema_migrations (version, name) VALUES (?1, ?2)",
                rusqlite::params![step.version, step.name],
            )
            .map_err(|error| classify_lock_failure(error, lock_timeout))?;
    }

    transaction
        .commit()
        .map_err(|error| classify_lock_failure(error, lock_timeout))?;
    Ok(())
}

/// The versions the ledger already records.
fn applied_versions(
    transaction: &rusqlite::Transaction<'_>,
) -> Result<HashSet<i64>, SqliteStreamsError> {
    let mut statement = transaction
        .prepare("SELECT version FROM epoch_schema_migrations")
        .map_err(|error| classify_lock_failure(error, Duration::from_secs(5)))?;
    let versions: Vec<i64> = statement
        .query_map([], |row| row.get::<_, i64>(0))
        .map_err(|error| classify_lock_failure(error, Duration::from_secs(5)))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| classify_lock_failure(error, Duration::from_secs(5)))?;
    Ok(versions.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use tokio_rusqlite::Connection;

    use super::*;

    /// The steps the ledger records, in order.
    fn ledger_rows(conn: &mut rusqlite::Connection) -> Vec<(i64, String)> {
        let mut statement = conn
            .prepare("SELECT version, name FROM epoch_schema_migrations ORDER BY version")
            .expect("the ledger reads");
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("the ledger rows query")
            .collect::<Result<Vec<_>, _>>()
            .expect("the ledger rows collect")
    }

    /// Whether a table is present in the database.
    fn table_present(conn: &mut rusqlite::Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type = 'table' AND name = ?1)",
            [name],
            |row| row.get::<_, i64>(0),
        )
        .expect("the table lookup runs")
            != 0
    }

    /// The steps apply in order, and the ledger records each one.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_steps_apply_in_order_and_the_ledger_records_them() {
        let connection = Connection::open_in_memory()
            .await
            .expect("memory connection");
        connection
            .call(|conn| {
                install_busy_timeout(conn, Duration::from_secs(5))?;
                apply(conn, Duration::from_secs(5))
            })
            .await
            .expect("the schema migrates");

        let rows = connection
            .call(|conn| Ok::<_, rusqlite::Error>(ledger_rows(conn)))
            .await
            .expect("the ledger reads back");
        assert_eq!(
            rows,
            vec![
                (1, "create-stream-events".to_owned()),
                (2, "feed-cursors".to_owned()),
                (3, "outbox-intent-index".to_owned()),
            ],
            "every step recorded in order"
        );
    }

    /// A second migration is a no-op: the ledger does not grow and the
    /// schema is unchanged.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_migrate_is_a_no_op() {
        let connection = Connection::open_in_memory()
            .await
            .expect("memory connection");
        for _ in 0..2 {
            connection
                .call(|conn| {
                    install_busy_timeout(conn, Duration::from_secs(5))?;
                    apply(conn, Duration::from_secs(5))
                })
                .await
                .expect("the schema migrates");
        }

        let rows = connection
            .call(|conn| Ok::<_, rusqlite::Error>(ledger_rows(conn)))
            .await
            .expect("the ledger reads back");
        assert_eq!(rows.len(), 3, "a second run records nothing new");
    }

    /// A failing step rolls the whole run back: its DDL, the ledger
    /// rows, and the ledger table itself. Nothing partial survives,
    /// and the next run starts clean.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failing_step_leaves_no_partial_application() {
        let poison = Migration {
            version: 2,
            name: "poison",
            sql: "CREATE TABLE poison_marker (x INT); INSERT INTO no_such_table VALUES (1);",
        };
        let steps = [MIGRATIONS[0], poison];

        let connection = Connection::open_in_memory()
            .await
            .expect("memory connection");
        let outcome = connection
            .call(move |conn| {
                install_busy_timeout(conn, Duration::from_secs(5))?;
                apply_steps(conn, &steps, Duration::from_secs(5))
            })
            .await;
        assert!(outcome.is_err(), "the poison step must fail: {outcome:?}");

        let (poisoned, events, ledger) = connection
            .call(|conn| {
                Ok::<_, rusqlite::Error>((
                    table_present(conn, "poison_marker"),
                    table_present(conn, "stream_events"),
                    table_present(conn, "epoch_schema_migrations"),
                ))
            })
            .await
            .expect("the tables are looked up");
        assert!(!poisoned, "the poisoned step's table does not survive");
        assert!(!events, "the first step's table does not survive");
        assert!(!ledger, "the ledger table does not survive");
    }

    /// The step list is the migration contract: its order is load
    /// bearing and pinned here, not assumed.
    #[test]
    fn steps_are_ascending_and_gapless() {
        let versions: Vec<i64> = MIGRATIONS.iter().map(|step| step.version).collect();
        assert_eq!(versions.first(), Some(&1), "versions start at 1");
        assert!(
            versions.windows(2).all(|pair| pair[0] + 1 == pair[1]),
            "versions ascend without gaps: {versions:?}"
        );
    }
}
