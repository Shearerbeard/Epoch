use std::fmt::Debug;

use async_trait::async_trait;
use redis_om::{redis::aio::MultiplexedConnection, Client, JsonModel, RedisError};
use thiserror::Error;

use crate::repository::{
    state::{StateStream, VersionedStreamSnapshotRepository},
    RepositoryVersion, VersionedRepositoryError,
};

use super::{RedisVersion};

#[derive(Debug, Error)]
pub enum RedisRepositoryError {
    #[error("Redis connection error {0:?}")]
    ConnectionError(RedisError),
    #[error("Could not parse redis stream version {0:?}")]
    ParseVersion(String),
    #[error("Could not read stream: {0:?}")]
    ReadError(RedisError),
    #[error("Could not save state: {0:?}")]
    SaveError(RedisError),
    #[error("Could not parse event {0:?}")]
    ParseEvent(RedisError),
}

pub struct RedisJSONSnapshotRepository {
    client: Client,
    snapshot_expiry: Option<usize>,
}

impl RedisJSONSnapshotRepository {
    pub async fn get_connection(
        &self,
    ) -> Result<MultiplexedConnection, VersionedRepositoryError<RedisRepositoryError, RedisVersion>>
    {
        self.client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| {
                VersionedRepositoryError::RepoErr(RedisRepositoryError::ConnectionError(e))
            })
    }
}

#[async_trait]
impl<State> VersionedStreamSnapshotRepository<State> for RedisJSONSnapshotRepository
where
    State: Send + Sync + JsonModel + Clone + StateStream<String>,
{
    type Version = RedisVersion;
    type Err = RedisRepositoryError;

    type StreamId = String;

    async fn reify(
        &self,
        stream: Option<Self::StreamId>,
    ) -> Result<
        (State, RepositoryVersion<Self::Version>),
        VersionedRepositoryError<Self::Err, Self::Version>,
    > {
        todo!()
    }

    async fn save(
        &mut self,
        version: &Self::Version,
        state: &State,
    ) -> Result<State, VersionedRepositoryError<Self::Err, Self::Version>> {
        let mut conn = self.get_connection().await?;

        // TODO: Require ToRedisStateDTO<State>
        // Convert State to DTO
        // struct RedisStateDTO<State> {
        //     id: String,
        //     data: State,

        // }

        // impl<State> RedisStateDTO<State> {
        //     fn _get_composite_id(id: Self::StreamId, version: Self::Version) -> &str{
        //         format!("{}:{}", id, version)
        //     }
        // }

        // TODO: Write Version to Version Set in redis under key <JSONModelKey>:versions
        // TODO: Write Latest to key in redus under <JSONModelKey>:latest_version

        state
            .clone()
            .save(&mut conn)
            .await
            .map_err(RedisRepositoryError::SaveError)
            .map_err(VersionedRepositoryError::RepoErr)?;

        if let Some(secs) = self.snapshot_expiry {
            state
                .expire(secs, &mut conn)
                .await
                .map_err(RedisRepositoryError::SaveError)
                .map_err(VersionedRepositoryError::RepoErr)?;
        }

        Ok(state.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use serde::{Serialize, Deserialize};

    use super::*;

    #[derive(JsonModel, Serialize, Deserialize)]
    struct TestModel {
        id: String,
    }

    async fn client_from_environment() -> Client {
        let _ = dotenv::dotenv().expect("File .env or Env Vars not found");

        let settings: String = dotenv::var("REDIS_CONNECTION_STRING")
            .expect("Redis to be set in env")
            .parse()
            .expect("Redis connection string to parse");

        Client::open(settings).expect("Redis Client")
    }

    #[actix_rt::test]
    async fn test_json() {
        let client = client_from_environment().await;
        let mut conn = client.get_multiplexed_async_connection().await.unwrap();

        let mut test = TestModel {
            id: "1".to_string(),
        };

        let _ = test.save(&mut conn).await.unwrap();
    }
}
