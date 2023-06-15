use async_trait::async_trait;

use super::{RepositoryVersion, VersionedRepositoryError};

#[async_trait]
pub trait StateRepository<State, Err> {
    async fn reify(&self) -> Result<State, Err>;
    async fn save(&mut self, state: &State) -> Result<State, Err>;
}

#[async_trait]
pub trait VersionedStateRepository<'a, State, Err>
where
    State: Send + Sync,
    Err: Send + Sync,
{
    type Version: Eq + Send + Sync;

    async fn reify(&self) -> Result<(State, RepositoryVersion<Self::Version>), Err>;
    async fn save(
        &mut self,
        version: &RepositoryVersion<Self::Version>,
        state: &State,
    ) -> Result<State, VersionedRepositoryError<Err, Self::Version>>
    where
        'a: 'async_trait,
        State: 'async_trait,
        Err: 'async_trait;
}

#[async_trait]
pub trait VersionedStreamSnapshotRepository<State, Err>
where
    State: Send + Sync,
    Err: Send + Sync,
{
    type Version: Eq + Send + Sync;
    type StreamId: Eq + Send + Sync;

    async fn reify(
        &self,
        stream: Option<Self::StreamId>,
    ) -> Result<
        (State, RepositoryVersion<Self::Version>),
        VersionedRepositoryError<Err, Self::Version>,
    >;

    async fn save(
        &mut self,
        version: &RepositoryVersion<Self::Version>,
        state: &State,
        stream: Option<Self::StreamId>,
    ) -> Result<State, VersionedRepositoryError<Err, Self::Version>>;
}
