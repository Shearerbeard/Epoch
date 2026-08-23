//! Versioned schema migrations for the streams tables.
//!
//! Steps are ordered, named, and recorded in an applied ledger
//! (`epoch_schema_migrations`) inside the database, replacing the
//! single CREATE-IF-NOT-EXISTS batch that could never amend an
//! existing table. A database created at any earlier shape reaches
//! the current schema through [`apply`] alone.
//!
//! The whole run - lock, ledger read, pending steps, ledger writes -
//! is one transaction behind the advisory-lock discipline the append
//! path already uses, so concurrent callers serialize and the later
//! ones find nothing pending. DDL in postgres is transactional, so a
//! failed step leaves no partial application behind.

use std::collections::HashSet;

use tokio_postgres::Client;

use super::PgStreamsError;

/// Serializes migrations across callers, pools, and processes.
const MIGRATION_LOCK: i64 = 0x6570_6f63_685f_7374; // "epoch_st"

/// Creates the applied ledger. Its own IF NOT EXISTS is safe because
/// it runs under the migration lock, before any ledger read.
const ENSURE_LEDGER: &str = "CREATE TABLE IF NOT EXISTS epoch_schema_migrations (
    version BIGINT PRIMARY KEY,
    name TEXT NOT NULL,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
)";

struct Migration {
    version: i64,
    name: &'static str,
    sql: &'static str,
}

/// Step 1 embeds `schema.sql` verbatim; the file stays the canonical
/// current-shape reference. Step 2 carries the check constraint to
/// databases created before it existed in the CREATE: postgres names
/// the inline constraint `stream_events_sequence_check`, and the
/// guard adds it only when a database has no constraint of that name,
/// which is also what makes re-running step 1's CREATE on an existing
/// table harmless. New schema work lands as further steps here, in
/// ascending version order.
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "create-stream-events",
        sql: include_str!("schema.sql"),
    },
    Migration {
        version: 2,
        name: "pin-sequences-positive",
        sql: "DO $m$
            BEGIN
                IF NOT EXISTS (
                    SELECT 1 FROM pg_constraint
                    WHERE conrelid = 'stream_events'::regclass
                      AND conname = 'stream_events_sequence_check'
                ) THEN
                    ALTER TABLE stream_events
                        ADD CONSTRAINT stream_events_sequence_check
                        CHECK (sequence > 0);
                END IF;
            END
            $m$",
    },
];

/// Apply every step the database has not recorded, in version order,
/// recording each in the ledger inside the same transaction that ran
/// its DDL.
pub(super) async fn apply(client: &mut Client) -> Result<(), PgStreamsError> {
    let tx = client.transaction().await?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&MIGRATION_LOCK])
        .await?;
    tx.batch_execute(ENSURE_LEDGER).await?;

    let rows = tx
        .query("SELECT version FROM epoch_schema_migrations", &[])
        .await?;
    let applied: HashSet<i64> = rows.iter().map(|row| row.get(0)).collect();

    for step in MIGRATIONS
        .iter()
        .filter(|step| !applied.contains(&step.version))
    {
        tx.batch_execute(step.sql).await?;
        tx.execute(
            "INSERT INTO epoch_schema_migrations (version, name) VALUES ($1, $2)",
            &[&step.version, &step.name],
        )
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

#[cfg(all(test, feature = "postgres"))]
mod tests {
    use bb8::Pool;
    use bb8_postgres::PostgresConnectionManager;
    use tokio_postgres::NoTls;

    use super::*;
    use crate::streams::postgres::{PgEventStreams, PgPool};

    /// The pre-repair shape: the original CREATE before the check
    /// constraint existed. A database holding this table and no
    /// applied ledger is exactly the state the versioned mechanism
    /// exists to repair.
    const OLD_SHAPE: &str = "CREATE TABLE stream_events (
        global_sequence BIGSERIAL PRIMARY KEY,
        category VARCHAR(255) NOT NULL,
        stream_key VARCHAR(255) NOT NULL,
        event_type VARCHAR(255) NOT NULL,
        sequence BIGINT NOT NULL,
        event_data JSONB NOT NULL,
        created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        CONSTRAINT stream_events_position
            UNIQUE (category, stream_key, sequence)
    );
    CREATE INDEX idx_stream_events_category
        ON stream_events (category, global_sequence)";

    /// The compose server's config from `EPOCH_PG_TEST_URL`; there is
    /// no safe default database to fall back on, so an unset variable
    /// must stop the run rather than silently pick one.
    fn server_config() -> tokio_postgres::Config {
        let _ = dotenv::dotenv();
        std::env::var("EPOCH_PG_TEST_URL")
            .expect("EPOCH_PG_TEST_URL must be set (see .env.example)")
            .parse()
            .expect("parse EPOCH_PG_TEST_URL")
    }

    /// Scratch databases keep each scenario's starting shape exact.
    /// Database names cannot arrive as query parameters, so the name
    /// is interpolated; every caller passes a fixed literal.
    async fn scratch_database(name: &str) {
        let (admin, driver) = server_config()
            .connect(NoTls)
            .await
            .expect("admin connection to the compose server");
        // The socket driver ends when the client drops; a driver error
        // mid-statement surfaces through the statement call itself.
        tokio::spawn(async move {
            let _ = driver.await;
        });
        admin
            .execute(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"), &[])
            .await
            .expect("drop any earlier scratch database");
        admin
            .execute(&format!("CREATE DATABASE {name}"), &[])
            .await
            .expect("create the scratch database");
    }

    async fn pool_for(name: &str) -> PgPool {
        let mut config = server_config();
        config.dbname(name);
        Pool::builder()
            .build(PostgresConnectionManager::new(config, NoTls))
            .await
            .expect("pool over the scratch database")
    }

    /// `migrate()` must carry an old-shape database to the current
    /// schema with data intact, through no path but itself, and stay
    /// idempotent once there.
    #[tokio::test(flavor = "multi_thread")]
    async fn old_shape_database_reaches_current_through_migrate_alone() {
        scratch_database("epoch_migration_old_shape").await;
        let pool = pool_for("epoch_migration_old_shape").await;

        let seed = pool.get().await.expect("seed connection");
        seed.batch_execute(OLD_SHAPE).await.expect("seed old shape");
        seed.execute(
            "INSERT INTO stream_events \
                 (category, stream_key, event_type, sequence, event_data) \
                 VALUES ('c', 'k', 'T', 1, '\"{}\"')",
            &[],
        )
        .await
        .expect("seed one event");

        let store = PgEventStreams::<String, ()>::new(pool.clone(), "c");
        store.migrate().await.expect("first migrate repairs");
        store.migrate().await.expect("second migrate is a no-op");

        let conn = pool.get().await.expect("verification connection");
        let constraint: i64 = conn
            .query_one(
                "SELECT COUNT(*) FROM pg_constraint \
                 WHERE conrelid = 'stream_events'::regclass \
                   AND conname = 'stream_events_sequence_check'",
                &[],
            )
            .await
            .expect("constraint lookup")
            .get(0);
        assert_eq!(constraint, 1, "the repair step added the check");

        let ledger: Vec<(i64, String)> = conn
            .query(
                "SELECT version, name FROM epoch_schema_migrations \
                 ORDER BY version",
                &[],
            )
            .await
            .expect("ledger read")
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();
        assert_eq!(
            ledger,
            vec![
                (1, "create-stream-events".to_owned()),
                (2, "pin-sequences-positive".to_owned()),
            ],
            "both steps recorded in order"
        );

        let events: i64 = conn
            .query_one("SELECT COUNT(*) FROM stream_events", &[])
            .await
            .expect("event count")
            .get(0);
        assert_eq!(events, 1, "the seeded event survived the repair");
    }

    /// Fresh and already-current databases converge, and concurrent
    /// callers all succeed with the ledger applied exactly once.
    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_callers_converge_on_a_fresh_database() {
        scratch_database("epoch_migration_fresh").await;
        let pool = pool_for("epoch_migration_fresh").await;

        let mut callers = Vec::new();
        for _ in 0..4 {
            let pool = pool.clone();
            callers.push(tokio::spawn(async move {
                let mut conn = pool.get().await.expect("caller connection");
                apply(&mut conn).await.expect("concurrent migrate")
            }));
        }
        for caller in callers {
            caller.await.expect("caller task");
        }

        let mut conn = pool.get().await.expect("verification connection");
        // An already-current database migrates to the same state.
        apply(&mut conn)
            .await
            .expect("no-op migrate after convergence");

        let ledger: i64 = conn
            .query_one("SELECT COUNT(*) FROM epoch_schema_migrations", &[])
            .await
            .expect("ledger count")
            .get(0);
        assert_eq!(ledger, 2, "each step recorded exactly once");

        let steps: Vec<i64> = MIGRATIONS.iter().map(|step| step.version).collect();
        assert!(
            steps.windows(2).all(|pair| pair[0] < pair[1]),
            "the step list stays in ascending version order: {steps:?}"
        );
    }
}
