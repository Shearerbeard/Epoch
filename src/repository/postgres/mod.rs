//! PostgreSQL-backed event repository.
//!
//! Semantics mirror the ESDB backend:
//! - `load(None)` returns every event of this repository's stream type,
//!   ordered by global sequence (the ESDB `$ce-` category equivalent).
//! - `load_from_version(Exact(v))` is INCLUSIVE, matching ESDB's
//!   `StreamPosition::Position(v)`.
//! - `append` checks the expected version inside a transaction guarded
//!   by a per-stream advisory lock, so concurrent appends to one
//!   stream conflict deterministically instead of racing.

use std::{fmt::Debug, marker::PhantomData};

use async_trait::async_trait;
use bb8::Pool;
use bb8_postgres::PostgresConnectionManager;
use serde::{de::DeserializeOwned, Serialize};
use tokio_postgres::NoTls;

use crate::decider::Event;

use self::{error::PgRepositoryError, version::PgVersion};

use super::{
    event::VersionedEventRepositoryWithStreams, RepositoryVersion, VersionDiff,
    VersionedRepositoryError,
};

pub mod error;
pub mod version;

const SCHEMA: &str = include_str!("schema.sql");

type RepoResult<T> = Result<T, VersionedRepositoryError<PgRepositoryError, PgVersion>>;

#[derive(Clone)]
pub struct PgEventRepository<E> {
    pool: Pool<PostgresConnectionManager<NoTls>>,
    stream_type: String,
    _hidden: PhantomData<E>,
}

impl<E> PgEventRepository<E> {
    pub fn new(pool: Pool<PostgresConnectionManager<NoTls>>, stream_type: &str) -> Self {
        Self {
            pool,
            stream_type: stream_type.to_string(),
            _hidden: PhantomData,
        }
    }

    /// Build a pool from a postgres connection string, e.g.
    /// `postgres://user:pass@host:5432/dbname`.
    pub async fn pool_from_conn_str(
        conn_str: &str,
    ) -> Result<Pool<PostgresConnectionManager<NoTls>>, PgRepositoryError> {
        let config: tokio_postgres::Config = conn_str
            .parse()
            .map_err(|e: tokio_postgres::Error| PgRepositoryError::Config(e.to_string()))?;
        let manager = PostgresConnectionManager::new(config, NoTls);
        Ok(Pool::builder().build(manager).await?)
    }

    /// Apply schema.sql. Idempotent and safe under concurrent callers:
    /// a transaction-scoped advisory lock serializes the CREATE
    /// statements and releases automatically at commit, so an async
    /// cancellation cannot leak a session-level lock.
    pub async fn migrate(&self) -> Result<(), PgRepositoryError> {
        let mut conn = self.pool.get().await?;
        const MIGRATION_LOCK: i64 = 0x6570_6f63_685f_6d69; // "epoch_mi"
        let tx = conn.transaction().await?;
        tx.query("SELECT pg_advisory_xact_lock($1)", &[&MIGRATION_LOCK])
            .await?;
        tx.batch_execute(SCHEMA).await?;
        tx.commit().await?;
        Ok(())
    }

    fn stream_name(&self, stream_id: &str) -> String {
        format!("{}-{}", self.stream_type, stream_id)
    }
}

#[async_trait]
impl<'a, E> VersionedEventRepositoryWithStreams<'a, E, PgRepositoryError> for PgEventRepository<E>
where
    E: Event + Sync + Send + Serialize + DeserializeOwned + Clone + Debug,
{
    type StreamId = String;
    type Version = PgVersion;

    async fn load(
        &self,
        id: Option<&Self::StreamId>,
    ) -> RepoResult<(Vec<E>, RepositoryVersion<PgVersion>)> {
        self.load_from_version(&RepositoryVersion::Any, id).await
    }

    async fn load_from_version(
        &self,
        version: &RepositoryVersion<PgVersion>,
        id: Option<&Self::StreamId>,
    ) -> RepoResult<(Vec<E>, RepositoryVersion<PgVersion>)> {
        let conn = self
            .pool
            .get()
            .await
            .map_err(PgRepositoryError::Pool)
            .map_err(VersionedRepositoryError::RepoErr)?;

        // Inclusive lower bound, matching ESDB StreamPosition::Position.
        let from_sequence: i64 = match version {
            RepositoryVersion::Exact(v) => v.sequence(),
            RepositoryVersion::Any
            | RepositoryVersion::NoStream
            | RepositoryVersion::StreamExists => 0,
        };

        let rows = match id {
            Some(stream_id) => {
                conn.query(
                    "SELECT event_data, sequence FROM events \
                     WHERE stream_name = $1 AND sequence >= $2 \
                     ORDER BY sequence ASC",
                    &[&self.stream_name(stream_id), &from_sequence],
                )
                .await
            }
            None => {
                conn.query(
                    "SELECT event_data, sequence FROM events \
                     WHERE stream_type = $1 AND global_sequence >= $2 \
                     ORDER BY global_sequence ASC",
                    &[&self.stream_type, &from_sequence],
                )
                .await
            }
        }
        .map_err(PgRepositoryError::Connection)
        .map_err(VersionedRepositoryError::RepoErr)?;

        if rows.is_empty() {
            // An empty filtered result on an existing stream is
            // StreamExists (position past the end), not NoStream -
            // matching how the ESDB backend distinguishes
            // ResourceNotFound from an exhausted read.
            if let Some(stream_id) = id {
                let exists: bool = conn
                    .query_one(
                        "SELECT EXISTS(SELECT 1 FROM events WHERE stream_name = $1)",
                        &[&self.stream_name(stream_id)],
                    )
                    .await
                    .map_err(PgRepositoryError::Connection)
                    .map_err(VersionedRepositoryError::RepoErr)?
                    .get(0);
                if exists {
                    return Ok((vec![], RepositoryVersion::StreamExists));
                }
            }
            return Ok((vec![], RepositoryVersion::NoStream));
        }

        let mut events = Vec::with_capacity(rows.len());
        let mut pos = RepositoryVersion::StreamExists;

        for row in rows {
            let event_data: serde_json::Value = row.get("event_data");
            let sequence: i64 = row.get("sequence");

            let event: E = serde_json::from_value(event_data)
                .map_err(PgRepositoryError::Serialization)
                .map_err(VersionedRepositoryError::RepoErr)?;

            events.push(event);
            pos = RepositoryVersion::Exact(PgVersion::from(sequence));
        }

        Ok((events, pos))
    }

    async fn append(
        &mut self,
        version: &RepositoryVersion<PgVersion>,
        stream: &Self::StreamId,
        events: &Vec<E>,
    ) -> RepoResult<(Vec<E>, RepositoryVersion<PgVersion>)>
    where
        'a: 'async_trait,
        E: 'async_trait,
    {
        let stream_name = self.stream_name(stream);

        let mut conn = self
            .pool
            .get()
            .await
            .map_err(PgRepositoryError::Pool)
            .map_err(VersionedRepositoryError::RepoErr)?;

        let transaction = conn
            .transaction()
            .await
            .map_err(PgRepositoryError::Connection)
            .map_err(VersionedRepositoryError::RepoErr)?;

        // Serialize appends per stream for the life of this transaction.
        transaction
            .execute(
                "SELECT pg_advisory_xact_lock(hashtext($1))",
                &[&stream_name],
            )
            .await
            .map_err(PgRepositoryError::Connection)
            .map_err(VersionedRepositoryError::RepoErr)?;

        let row = transaction
            .query_one(
                "SELECT COALESCE(MAX(sequence), 0) AS max_seq FROM events WHERE stream_name = $1",
                &[&stream_name],
            )
            .await
            .map_err(PgRepositoryError::Connection)
            .map_err(VersionedRepositoryError::RepoErr)?;

        let current_sequence: i64 = row.get("max_seq");
        let current_version = if current_sequence == 0 {
            RepositoryVersion::NoStream
        } else {
            RepositoryVersion::Exact(PgVersion::from(current_sequence))
        };

        let conflict = match version {
            RepositoryVersion::Exact(expected) => expected.sequence() != current_sequence,
            RepositoryVersion::NoStream => current_sequence != 0,
            // ESDB maps StreamExists and Any to ExpectedRevision::Any.
            RepositoryVersion::StreamExists | RepositoryVersion::Any => false,
        };

        if conflict {
            return Err(VersionedRepositoryError::VersionConflict(VersionDiff::new(
                *version,
                current_version,
            )));
        }

        if events.is_empty() {
            // Nothing to write; the stream's version is unchanged.
            transaction
                .commit()
                .await
                .map_err(PgRepositoryError::Connection)
                .map_err(VersionedRepositoryError::RepoErr)?;
            return Ok((vec![], current_version));
        }

        let mut next_sequence = current_sequence;
        for event in events {
            next_sequence += 1;
            let event_data = serde_json::to_value(event)
                .map_err(PgRepositoryError::Serialization)
                .map_err(VersionedRepositoryError::RepoErr)?;

            transaction
                .execute(
                    "INSERT INTO events (stream_name, stream_type, event_type, sequence, event_data) \
                     VALUES ($1, $2, $3, $4, $5)",
                    &[
                        &stream_name,
                        &self.stream_type,
                        &event.event_type(),
                        &next_sequence,
                        &event_data,
                    ],
                )
                .await
                .map_err(PgRepositoryError::Connection)
                .map_err(VersionedRepositoryError::RepoErr)?;
        }

        transaction
            .commit()
            .await
            .map_err(PgRepositoryError::Connection)
            .map_err(VersionedRepositoryError::RepoErr)?;

        Ok((
            events.to_owned(),
            RepositoryVersion::Exact(PgVersion::from(next_sequence)),
        ))
    }
}

#[cfg(test)]
mod tests {
    use const_random::const_random;

    use super::*;

    use crate::test_helpers::{
        deciders::user::UserEvent,
        repository::{
            versioned_event_repository_with_streams_occ_spec,
            versioned_event_repository_with_streams_spec,
        },
    };

    const BASE_STREAM: u32 = const_random!(u32);

    async fn repo_from_environment(stream_type: &str) -> PgEventRepository<UserEvent> {
        let conn_str = std::env::var("EPOCH_PG_TEST_URL")
            .unwrap_or_else(|_| "postgres://vikunja:devpass@localhost:54320/vikunja".to_string());
        let pool = PgEventRepository::<UserEvent>::pool_from_conn_str(&conn_str)
            .await
            .expect("pg pool from EPOCH_PG_TEST_URL");
        let repo = PgEventRepository::new(pool, stream_type);
        repo.migrate().await.expect("schema migrates");
        // Stream names are compile-time constants, so clear prior runs.
        let pool = repo.pool.clone();
        let conn = pool.get().await.expect("pool connection");
        conn.execute(
            "DELETE FROM events WHERE stream_type = $1",
            &[&repo.stream_type],
        )
        .await
        .expect("test stream reset");
        repo
    }

    #[actix_rt::test]
    async fn versioned_event_repository_with_streams_spec_postgres() {
        let stream_type = format!("spec-{}", BASE_STREAM);
        let repo = repo_from_environment(&stream_type).await;
        versioned_event_repository_with_streams_spec(repo).await;
    }

    #[actix_rt::test]
    async fn versioned_event_repository_with_streams_occ_spec_postgres() {
        let stream_type = format!("occ-spec-{}", BASE_STREAM);
        let repo = repo_from_environment(&stream_type).await;
        versioned_event_repository_with_streams_occ_spec(repo).await;
    }

    #[actix_rt::test]
    async fn load_missing_stream_reports_no_stream() {
        let stream_type = format!("missing-{}", BASE_STREAM);
        let repo = repo_from_environment(&stream_type).await;
        let res = repo.load(Some(&"never-written".to_string())).await;
        assert!(matches!(res, Ok((v, RepositoryVersion::NoStream)) if v.is_empty()));
    }

    #[actix_rt::test]
    async fn concurrent_appends_conflict_deterministically() {
        use crate::test_helpers::deciders::user::{User, UserName};

        let stream_type = format!("race-{}", BASE_STREAM);
        let repo_a = repo_from_environment(&stream_type).await;
        let repo_b = repo_a.clone();
        let stream_id = "shared".to_string();

        let event = |n: &str| {
            vec![UserEvent::UserAdded(User::new(
                1,
                UserName::try_from(n.to_string()).expect("valid name"),
            ))]
        };

        let (first, second) = (event("Mike"), event("Stella"));

        let (res_a, res_b) = futures::future::join(
            repo_a
                .clone()
                .append(&RepositoryVersion::NoStream, &stream_id, &first),
            repo_b
                .clone()
                .append(&RepositoryVersion::NoStream, &stream_id, &second),
        )
        .await;

        let outcomes = [res_a, res_b];
        let conflicts = outcomes
            .iter()
            .filter(|r| matches!(r, Err(VersionedRepositoryError::VersionConflict(_))))
            .count();
        assert_eq!(
            conflicts, 1,
            "exactly one racer must conflict: {outcomes:?}"
        );

        let (events, _) = repo_a.load(Some(&stream_id)).await.expect("stream loads");
        assert_eq!(events.len(), 1, "only the winning append landed");
    }
}
