#![allow(dead_code)] // skeleton entry; removed slice-by-slice as bodies fill (card 3)

//! SQLite-backed event repository.
//!
//! Semantics mirror the PostgreSQL and ESDB backends:
//! - `load(None)` returns every event of this repository's stream type,
//!   ordered by global sequence (the ESDB `$ce-` category equivalent).
//! - `load_from_version(Exact(v))` is INCLUSIVE, matching ESDB's
//!   `StreamPosition::Position(v)`.
//! - `append` checks the expected version inside an IMMEDIATE
//!   transaction, so concurrent appends to one stream conflict
//!   deterministically instead of racing: SQLite write transactions
//!   serialize the check-then-insert that a per-stream advisory lock
//!   guards in the PostgreSQL backend. Driver errors (including
//!   SQLITE_BUSY after the busy timeout) surface as `RepoErr`; only
//!   the version comparison produces `VersionConflict`.

use std::{fmt::Debug, marker::PhantomData};

use async_trait::async_trait;
use serde::{de::DeserializeOwned, Serialize};
use tokio_rusqlite::Connection;

use crate::decider::Event;

use self::{error::SqliteRepositoryError, version::SqliteVersion};

use super::{
    event::VersionedEventRepositoryWithStreams, RepositoryVersion, VersionedRepositoryError,
};

pub mod error;
pub mod version;

const SCHEMA: &str = include_str!("schema.sql");

type RepoResult<T> = Result<T, VersionedRepositoryError<SqliteRepositoryError, SqliteVersion>>;

#[derive(Clone)]
pub struct SqliteEventRepository<E> {
    connection: Connection,
    stream_type: String,
    _hidden: PhantomData<E>,
}

impl<E> SqliteEventRepository<E> {
    pub fn new(connection: Connection, stream_type: &str) -> Self {
        Self {
            connection,
            stream_type: stream_type.to_string(),
            _hidden: PhantomData,
        }
    }

    /// Open a file-backed database, e.g. `epoch.db`. Cross-process
    /// writers serialize on the busy timeout `migrate` installs.
    pub async fn open(path: &str, stream_type: &str) -> Result<Self, SqliteRepositoryError> {
        Ok(Self::new(Connection::open(path).await?, stream_type))
    }

    /// Open a private in-memory database. Every repository built on
    /// the returned handle (clones share it) sees the same database;
    /// nothing else can.
    pub async fn open_in_memory(stream_type: &str) -> Result<Self, SqliteRepositoryError> {
        Ok(Self::new(Connection::open_in_memory().await?, stream_type))
    }

    /// Apply schema.sql and install connection pragmas. Idempotent:
    /// every statement is IF NOT EXISTS or a pragma, so concurrent
    /// callers converge.
    pub async fn migrate(&self) -> Result<(), SqliteRepositoryError> {
        todo!()
    }
}

#[async_trait]
impl<'a, E> VersionedEventRepositoryWithStreams<'a, E, SqliteRepositoryError>
    for SqliteEventRepository<E>
where
    E: Event + Sync + Send + Serialize + DeserializeOwned + Clone + Debug,
{
    type StreamId = String;
    type Version = SqliteVersion;

    async fn load(
        &self,
        id: Option<&Self::StreamId>,
    ) -> RepoResult<(Vec<E>, RepositoryVersion<SqliteVersion>)> {
        self.load_from_version(&RepositoryVersion::Any, id).await
    }

    #[expect(
        unused_variables,
        reason = "todo!() body; filled by card 3 (sqlite fill)"
    )]
    async fn load_from_version(
        &self,
        version: &RepositoryVersion<SqliteVersion>,
        id: Option<&Self::StreamId>,
    ) -> RepoResult<(Vec<E>, RepositoryVersion<SqliteVersion>)> {
        todo!()
    }

    #[expect(
        unused_variables,
        reason = "todo!() body; filled by card 3 (sqlite fill)"
    )]
    async fn append(
        &mut self,
        version: &RepositoryVersion<SqliteVersion>,
        stream: &Self::StreamId,
        events: &[E],
    ) -> RepoResult<(Vec<E>, RepositoryVersion<SqliteVersion>)>
    where
        'a: 'async_trait,
        E: 'async_trait,
    {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_helpers::{
        deciders::user::{User, UserEvent, UserName},
        repository::{
            versioned_event_repository_with_streams_occ_spec,
            versioned_event_repository_with_streams_spec,
        },
    };

    // Every test builds its repository over a private in-memory
    // database, so no stream-type namespacing or teardown is needed.
    async fn fresh_repo() -> SqliteEventRepository<UserEvent> {
        let repo = SqliteEventRepository::open_in_memory("spec")
            .await
            .expect("memory connection");
        repo.migrate().await.expect("schema migrates");
        repo
    }

    fn one_event(name: &str) -> Vec<UserEvent> {
        vec![UserEvent::UserAdded(User::new(
            1,
            UserName::try_from(name.to_string()).expect("valid name"),
        ))]
    }

    #[actix_rt::test]
    async fn versioned_event_repository_with_streams_spec_sqlite() {
        let repo = fresh_repo().await;
        versioned_event_repository_with_streams_spec(repo, None).await;
    }

    #[actix_rt::test]
    async fn versioned_event_repository_with_streams_occ_spec_sqlite() {
        let repo = fresh_repo().await;
        versioned_event_repository_with_streams_occ_spec(repo).await;
    }

    #[actix_rt::test]
    async fn load_missing_stream_reports_no_stream() {
        let repo = fresh_repo().await;
        let res = repo.load(Some(&"never-written".to_string())).await;
        assert!(matches!(res, Ok((v, RepositoryVersion::NoStream)) if v.is_empty()));
    }

    #[actix_rt::test]
    async fn empty_load_on_existing_stream_reports_stream_exists() {
        let mut repo = fresh_repo().await;
        let stream_id = "one-event".to_string();
        let (_, version) = repo
            .append(&RepositoryVersion::NoStream, &stream_id, &one_event("Mike"))
            .await
            .expect("seed append");

        let res = repo.load_from_version(&version, Some(&stream_id)).await;
        assert!(matches!(res, Ok((v, RepositoryVersion::StreamExists)) if v.is_empty()));
    }

    #[actix_rt::test]
    async fn empty_append_returns_current_version() {
        let mut repo = fresh_repo().await;
        let stream_id = "seeded".to_string();
        let (_, version) = repo
            .append(&RepositoryVersion::NoStream, &stream_id, &one_event("Mike"))
            .await
            .expect("seed append");

        let res = repo.append(&version, &stream_id, &[]).await;
        assert!(matches!(
            res,
            Ok((v, returned)) if v.is_empty() && returned == version
        ));
    }

    #[actix_rt::test]
    async fn append_conflicts_report_expected_and_actual() {
        let mut repo = fresh_repo().await;
        let stream_id = "conflict".to_string();
        let (_, version) = repo
            .append(&RepositoryVersion::NoStream, &stream_id, &one_event("Mike"))
            .await
            .expect("seed append");

        // NoStream expected on an existing stream conflicts.
        let res = repo
            .append(
                &RepositoryVersion::NoStream,
                &stream_id,
                &one_event("Stella"),
            )
            .await;
        match res {
            Err(VersionedRepositoryError::VersionConflict(diff)) => {
                assert_eq!(diff.expected(), RepositoryVersion::NoStream);
                assert_eq!(diff.actual(), version);
            }
            other => panic!("expected a conflict, got {other:?}"),
        }

        // A stale Exact expectation conflicts with the actual head.
        let stale = RepositoryVersion::Exact(SqliteVersion::from(7));
        let res = repo.append(&stale, &stream_id, &one_event("Stella")).await;
        match res {
            Err(VersionedRepositoryError::VersionConflict(diff)) => {
                assert_eq!(diff.expected(), stale);
                assert_eq!(diff.actual(), version);
            }
            other => panic!("expected a conflict, got {other:?}"),
        }
    }

    #[actix_rt::test]
    async fn concurrent_appends_conflict_deterministically() {
        let repo = fresh_repo().await;
        let repo_b = repo.clone();
        let stream_id = "shared".to_string();

        let (first, second) = (one_event("Mike"), one_event("Stella"));

        let (res_a, res_b) = futures::future::join(
            repo.clone()
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

        let (events, _) = repo.load(Some(&stream_id)).await.expect("stream loads");
        assert_eq!(events.len(), 1, "only the winning append landed");
    }
}
