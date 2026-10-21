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
