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

use std::{fmt::Debug, marker::PhantomData, time::Duration};

use async_trait::async_trait;
use serde::{de::DeserializeOwned, Serialize};
use tokio_rusqlite::{
    params,
    rusqlite::{self, TransactionBehavior},
    Connection,
};

use crate::decider::Event;

use self::{error::SqliteRepositoryError, version::SqliteVersion};

use super::{
    event::VersionedEventRepositoryWithStreams, RepositoryVersion, VersionDiff,
    VersionedRepositoryError,
};

pub mod error;
pub mod version;

const SCHEMA: &str = include_str!("schema.sql");

/// How long a file-backed connection waits on a competing writer
/// before surfacing SQLITE_BUSY as a repository error.
const BUSY_TIMEOUT_MS: u64 = 5_000;

/// Flatten the actor's error envelope for driver-level failures:
/// inner errors and close errors are connection failures; a closed
/// actor is a handle failure. Underlying SQLite failures always reach
/// `Connection`, never `Handle`.
fn actor_error(envelope: tokio_rusqlite::Error<rusqlite::Error>) -> SqliteRepositoryError {
    match envelope {
        tokio_rusqlite::Error::Error(e) | tokio_rusqlite::Error::Close((_, e)) => {
            SqliteRepositoryError::Connection(e)
        }
        _ => SqliteRepositoryError::Handle(tokio_rusqlite::Error::ConnectionClosed),
    }
}

/// Flatten the actor's error envelope for append outcomes: a
/// unit-of-work failure (including VersionConflict) passes through;
/// actor-lifecycle failures surface as repository errors.
fn actor_outcome<T>(
    outcome: Result<
        T,
        tokio_rusqlite::Error<VersionedRepositoryError<SqliteRepositoryError, SqliteVersion>>,
    >,
) -> Result<T, VersionedRepositoryError<SqliteRepositoryError, SqliteVersion>> {
    match outcome {
        Ok(value) => Ok(value),
        Err(tokio_rusqlite::Error::Error(inner)) => Err(inner),
        Err(tokio_rusqlite::Error::Close((_, e))) => Err(VersionedRepositoryError::RepoErr(
            SqliteRepositoryError::Connection(e),
        )),
        Err(_) => Err(VersionedRepositoryError::RepoErr(
            SqliteRepositoryError::Handle(tokio_rusqlite::Error::ConnectionClosed),
        )),
    }
}

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
        self.connection
            .call(move |conn| {
                conn.busy_timeout(Duration::from_millis(BUSY_TIMEOUT_MS))?;
                // WAL lets file-backed readers proceed beside the
                // writer; on a :memory: database SQLite reports
                // "memory" back, which this query consumes harmlessly.
                conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get::<_, String>(0))?;
                conn.execute_batch(SCHEMA)?;
                Ok(())
            })
            .await
            .map_err(actor_error)
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

    async fn load_from_version(
        &self,
        version: &RepositoryVersion<SqliteVersion>,
        id: Option<&Self::StreamId>,
    ) -> RepoResult<(Vec<E>, RepositoryVersion<SqliteVersion>)> {
        // Inclusive lower bound, matching ESDB StreamPosition::Position.
        let from_sequence: i64 = match version {
            RepositoryVersion::Exact(v) => v.sequence(),
            RepositoryVersion::Any
            | RepositoryVersion::NoStream
            | RepositoryVersion::StreamExists => 0,
        };

        let stream_type = self.stream_type.clone();
        let stream_key = id.cloned();
        let outcome: (Vec<(String, i64)>, bool) = self
            .connection
            .call(move |conn| {
                let rows = match stream_key.as_deref() {
                    Some(stream_key) => conn
                        .prepare(
                            "SELECT event_data, sequence FROM events \
                             WHERE stream_type = ?1 AND stream_key = ?2 AND sequence >= ?3 \
                             ORDER BY sequence ASC",
                        )?
                        .query_map(params![stream_type, stream_key, from_sequence], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                        })?
                        .collect::<std::result::Result<Vec<_>, _>>()?,
                    None => conn
                        .prepare(
                            "SELECT event_data, sequence FROM events \
                             WHERE stream_type = ?1 AND global_sequence >= ?2 \
                             ORDER BY global_sequence ASC",
                        )?
                        .query_map(params![stream_type, from_sequence], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                        })?
                        .collect::<std::result::Result<Vec<_>, _>>()?,
                };

                // An empty filtered result on an existing stream is
                // StreamExists (position past the end), not NoStream -
                // matching how the ESDB backend distinguishes
                // ResourceNotFound from an exhausted read. The check
                // shares the read's actor round-trip.
                let mut stream_exists = false;
                if rows.is_empty() {
                    if let Some(stream_key) = stream_key.as_deref() {
                        stream_exists = conn.query_row(
                            "SELECT EXISTS(SELECT 1 FROM events \
                             WHERE stream_type = ?1 AND stream_key = ?2)",
                            params![stream_type, stream_key],
                            |row| row.get::<_, i64>(0),
                        )? != 0;
                    }
                }
                Ok((rows, stream_exists))
            })
            .await
            .map_err(actor_error)
            .map_err(VersionedRepositoryError::RepoErr)?;

        let (rows, stream_exists) = outcome;
        if rows.is_empty() {
            return if stream_exists {
                Ok((vec![], RepositoryVersion::StreamExists))
            } else {
                Ok((vec![], RepositoryVersion::NoStream))
            };
        }

        let mut events = Vec::with_capacity(rows.len());
        let mut pos = RepositoryVersion::StreamExists;

        for (event_data, sequence) in rows {
            let event: E = serde_json::from_str(&event_data)
                .map_err(SqliteRepositoryError::Serialization)
                .map_err(VersionedRepositoryError::RepoErr)?;
            events.push(event);
            pos = RepositoryVersion::Exact(SqliteVersion::from(sequence));
        }

        Ok((events, pos))
    }

    async fn append(
        &mut self,
        version: &RepositoryVersion<SqliteVersion>,
        stream: &Self::StreamId,
        events: &Vec<E>,
    ) -> RepoResult<(Vec<E>, RepositoryVersion<SqliteVersion>)>
    where
        'a: 'async_trait,
        E: 'async_trait,
    {
        // BEGIN IMMEDIATE takes the write lock before the version
        // check, so check-then-insert is atomic and the losing racer
        // conflicts instead of racing: the substitute for the postgres
        // backend's per-stream advisory lock, coarser (database-wide)
        // but strictly stronger. Driver failures (including SQLITE_BUSY
        // once the timeout expires) surface as RepoErr; only this
        // comparison produces VersionConflict.
        let stream_type = self.stream_type.clone();
        let stream_key = stream.clone();
        let expected = *version;

        // The closure owns everything it touches; payloads are
        // serialized on the caller's side of the actor.
        let payloads = events
            .iter()
            .map(|event| {
                serde_json::to_string(event)
                    .map_err(SqliteRepositoryError::Serialization)
                    .map_err(VersionedRepositoryError::RepoErr)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let event_types: Vec<String> = events.iter().map(|e| e.event_type()).collect();

        let outcome = self
            .connection
            .call(move |conn| {
                let repo_err = |e: rusqlite::Error| {
                    VersionedRepositoryError::RepoErr(SqliteRepositoryError::Connection(e))
                };

                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(repo_err)?;

                let current_sequence: i64 = tx
                    .query_row(
                        "SELECT COALESCE(MAX(sequence), 0) FROM events \
                         WHERE stream_type = ?1 AND stream_key = ?2",
                        params![stream_type, stream_key],
                        |row| row.get(0),
                    )
                    .map_err(repo_err)?;

                let current_version = if current_sequence == 0 {
                    RepositoryVersion::NoStream
                } else {
                    RepositoryVersion::Exact(SqliteVersion::from(current_sequence))
                };

                let conflict = match expected {
                    RepositoryVersion::Exact(v) => v.sequence() != current_sequence,
                    RepositoryVersion::NoStream => current_sequence != 0,
                    // ESDB maps StreamExists and Any to ExpectedRevision::Any.
                    RepositoryVersion::StreamExists | RepositoryVersion::Any => false,
                };

                if conflict {
                    return Err(VersionedRepositoryError::VersionConflict(VersionDiff::new(
                        expected,
                        current_version,
                    )));
                }

                if payloads.is_empty() {
                    // Nothing to write; the stream's version is unchanged.
                    tx.commit().map_err(repo_err)?;
                    return Ok(current_version);
                }

                let mut next_sequence = current_sequence;
                for (event_type, payload) in event_types.into_iter().zip(payloads) {
                    next_sequence += 1;
                    tx.execute(
                        "INSERT INTO events \
                         (stream_type, stream_key, event_type, sequence, event_data) \
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![stream_type, stream_key, event_type, next_sequence, payload],
                    )
                    .map_err(repo_err)?;
                }

                tx.commit().map_err(repo_err)?;
                Ok(RepositoryVersion::Exact(SqliteVersion::from(next_sequence)))
            })
            .await;

        let returned = actor_outcome(outcome)?;
        Ok((events.to_vec(), returned))
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

        // Reads are inclusive of the given position, so the empty
        // result needs a position past the stream's head.
        let past_the_end = match version {
            RepositoryVersion::Exact(v) => {
                RepositoryVersion::Exact(SqliteVersion::from(v.sequence() + 1))
            }
            other => other,
        };

        let res = repo
            .load_from_version(&past_the_end, Some(&stream_id))
            .await;
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

    #[actix_rt::test]
    async fn independent_connections_conflict_deterministically() {
        // Two separately opened actors on one file: the race BEGIN
        // IMMEDIATE exists for. No shared actor serializes the calls,
        // so the loser must wait out the busy timeout and then lose
        // the version check.
        let file = tempfile::NamedTempFile::new().expect("temp file");
        let path = file.path().to_str().expect("utf8 path").to_string();

        let repo_a = SqliteEventRepository::open(&path, "race")
            .await
            .expect("open a");
        let repo_b = SqliteEventRepository::open(&path, "race")
            .await
            .expect("open b");
        repo_a.migrate().await.expect("migrate a");
        repo_b.migrate().await.expect("migrate b");

        let stream_id = "shared".to_string();
        let (first, second) = (one_event("Mike"), one_event("Stella"));

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
