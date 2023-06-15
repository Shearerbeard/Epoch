use async_trait::async_trait;

use crate::repository::{
    state::VersionedStreamSnapshotRepository, RepositoryVersion, VersionedRepositoryError,
};

use super::RedisVersion;

pub struct RedisJSONSnapshotRepository {}

#[async_trait]
impl<State, Err> VersionedStreamSnapshotRepository<State, Err> for RedisJSONSnapshotRepository
where
    State: Send + Sync,
    Err: Send + Sync,
{
    type Version = RedisVersion;

    type StreamId = String;

    async fn reify(
        &self,
        stream: Option<Self::StreamId>,
    ) -> Result<
        (State, RepositoryVersion<Self::Version>),
        VersionedRepositoryError<Err, Self::Version>,
    > {
        todo!()
    }

    async fn save(
        &mut self,
        version: &RepositoryVersion<Self::Version>,
        state: &State,
        stream: Option<Self::StreamId>,
    ) -> Result<State, VersionedRepositoryError<Err, Self::Version>> {
        todo!()
    }
}
