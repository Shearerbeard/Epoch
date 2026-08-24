//! Versioned schema migrations for the streams tables.
//!
//! Steps are ordered, named SQL files under `migrations/`, embedded
//! verbatim, and recorded in an applied ledger
//! (`epoch_schema_migrations`) inside the database - replacing the
//! single CREATE-IF-NOT-EXISTS batch that could never amend an
//! existing table. A database created at any earlier shape reaches
//! the current schema through [`PgEventStreams::migrate`] alone.
//!
//! A step file is IMMUTABLE once any database has applied it. An
//! edit to an applied step retroactively changes what fresh
//! databases get, while applied ones keep the old shape - identical
//! ledgers, divergent schemas. Schema changes arrive as new numbered
//! step files, never as edits to existing ones; the current shape of
//! the schema is the composition of the steps, readable in order.
//!
//! The whole run - lock, ledger read, pending steps, ledger writes -
//! is one transaction behind the advisory-lock discipline the append
//! path already uses, so concurrent callers serialize and the later
//! ones find nothing pending. DDL in postgres is transactional, so a
//! failed step rolls everything back, the ledger table included.

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

#[derive(Clone, Copy)]
struct Migration {
    version: i64,
    name: &'static str,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "create-stream-events",
        sql: include_str!("migrations/0001-create-stream-events.sql"),
    },
    Migration {
        version: 2,
        name: "pin-sequences-positive",
        sql: include_str!("migrations/0002-pin-sequences-positive.sql"),
    },
    Migration {
        version: 3,
        name: "event-metadata",
        sql: include_str!("migrations/0003-event-metadata.sql"),
    },
];

/// Apply every step the database has not recorded, in version order,
/// recording each in the ledger inside the same transaction that ran
/// its DDL.
pub(super) async fn apply(client: &mut Client) -> Result<(), PgStreamsError> {
    apply_steps(client, MIGRATIONS).await
}

async fn apply_steps(client: &mut Client, steps: &[Migration]) -> Result<(), PgStreamsError> {
    let tx = client.transaction().await?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&MIGRATION_LOCK])
        .await?;
    tx.batch_execute(ENSURE_LEDGER).await?;

    let rows = tx
        .query("SELECT version FROM epoch_schema_migrations", &[])
        .await?;
    let applied: HashSet<i64> = rows.iter().map(|row| row.get(0)).collect();

    for step in steps.iter().filter(|step| !applied.contains(&step.version)) {
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
    use std::sync::Arc;

    use bb8::Pool;
    use bb8_postgres::PostgresConnectionManager;
    use tokio::sync::Barrier;
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

    fn store(pool: &PgPool) -> PgEventStreams<String, ()> {
        PgEventStreams::new(pool.clone(), "migrations")
    }

    /// The applied ledger's (version, name) rows, in order.
    async fn ledger_rows(pool: &PgPool) -> Vec<(i64, String)> {
        let conn = pool.get().await.expect("ledger connection");
        conn.query(
            "SELECT version, name FROM epoch_schema_migrations ORDER BY version",
            &[],
        )
        .await
        .expect("ledger read")
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect()
    }

    /// The shape of `stream_events` that matters to the migration
    /// story: every constraint, by name and definition. Comparing
    /// this across databases is comparing their schema state.
    async fn constraint_defs(pool: &PgPool) -> Vec<(String, String)> {
        let conn = pool.get().await.expect("constraint connection");
        conn.query(
            "SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE conrelid = 'stream_events'::regclass \
             ORDER BY conname",
            &[],
        )
        .await
        .expect("constraint read")
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect()
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

        store(&pool).migrate().await.expect("first migrate repairs");
        store(&pool)
            .migrate()
            .await
            .expect("second migrate is a no-op");

        assert_eq!(
            constraint_defs(&pool).await,
            vec![
                (
                    "stream_events_pkey".to_owned(),
                    "PRIMARY KEY (global_sequence)".to_owned(),
                ),
                (
                    "stream_events_position".to_owned(),
                    "UNIQUE (category, stream_key, sequence)".to_owned(),
                ),
                (
                    "stream_events_sequence_check".to_owned(),
                    "CHECK ((sequence > 0))".to_owned(),
                ),
            ],
            "the repair step added the real check constraint"
        );

        assert_eq!(
            ledger_rows(&pool).await,
            vec![
                (1, "create-stream-events".to_owned()),
                (2, "pin-sequences-positive".to_owned()),
                (3, "event-metadata".to_owned()),
            ],
            "every step recorded in order"
        );

        let conn = pool.get().await.expect("verification connection");
        let events: i64 = conn
            .query_one("SELECT COUNT(*) FROM stream_events", &[])
            .await
            .expect("event count")
            .get(0);
        assert_eq!(events, 1, "the seeded event survived the repair");
    }

    /// Four callers hit the PUBLIC `migrate()` at once, through a
    /// barrier so the advisory lock is genuinely contended - once on
    /// a fresh database, once on an already-current one - and the two
    /// databases converge to the same schema and ledger.
    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_migrate_callers_converge_on_both_starting_states() {
        const CALLERS: usize = 4;
        scratch_database("epoch_migration_fresh").await;
        scratch_database("epoch_migration_current").await;
        let fresh = pool_for("epoch_migration_fresh").await;
        let current = pool_for("epoch_migration_current").await;

        // The already-current database exists before the storm hits it.
        store(&current)
            .migrate()
            .await
            .expect("pre-migrate the current database");

        for pool in [&fresh, &current] {
            let barrier = Arc::new(Barrier::new(CALLERS));
            let mut callers = Vec::new();
            for _ in 0..CALLERS {
                let barrier = Arc::clone(&barrier);
                let store = store(pool);
                callers.push(tokio::spawn(async move {
                    barrier.wait().await;
                    store.migrate().await.expect("concurrent migrate")
                }));
            }
            for caller in callers {
                caller.await.expect("caller task");
            }
        }

        assert_eq!(
            ledger_rows(&fresh).await,
            ledger_rows(&current).await,
            "both starting states record the same steps exactly once"
        );
        assert_eq!(
            constraint_defs(&fresh).await,
            constraint_defs(&current).await,
            "both starting states converge to the same schema"
        );
    }

    /// A failing step rolls the whole run back - its DDL, the ledger
    /// rows, and the ledger table itself - so nothing partial
    /// survives and the next run starts clean.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failing_step_leaves_no_partial_application() {
        scratch_database("epoch_migration_failure").await;
        let pool = pool_for("epoch_migration_failure").await;

        let poison = Migration {
            version: 2,
            name: "poison",
            sql: "CREATE TABLE poison_marker (x INT); SELECT 1/0;",
        };
        let steps = [MIGRATIONS[0], poison];
        let mut conn = pool.get().await.expect("caller connection");
        apply_steps(&mut conn, &steps)
            .await
            .expect_err("the poison step fails");

        let conn = pool.get().await.expect("verification connection");
        for table in ["poison_marker", "stream_events", "epoch_schema_migrations"] {
            let present: i64 = conn
                .query_one(
                    "SELECT COUNT(*) FROM information_schema.tables \
                     WHERE table_schema = 'public' AND table_name = $1",
                    &[&table],
                )
                .await
                .expect("table lookup")
                .get(0);
            assert_eq!(present, 0, "no {table} survives the rollback");
        }
    }

    /// The step list is the migration contract; its order is load
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
