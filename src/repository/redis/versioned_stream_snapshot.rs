use std::{fmt::Debug, marker::PhantomData};

use async_trait::async_trait;
use redis_om::{
    redis::{aio::MultiplexedConnection, streams::StreamId},
    Client, JsonModel, RedisError,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use thiserror::Error;

use crate::repository::{
    state::{StateStream, VersionedStreamSnapshotRepository},
    RepositoryVersion, VersionedRepositoryError,
};

use super::RedisVersion;

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
    #[error("Could not use data transfer type")]
    RedisDTO(RedisJsonDTOError),
}

#[derive(Debug, Error)]
pub enum RedisJsonDTOError {
    #[error("Could not deserialize")]
    Deserialize,
}

pub trait VersionedStateModel<Data> {
    fn id(&self) -> String;
    fn version(&self) -> RedisVersion;
    fn data(&self) -> Data;
    fn new(id: String, version: RedisVersion, data: Data) -> Self;
}

pub trait RedisJsonDTO<JM, Data, DTOErr>
where
    JM: JsonModel + VersionedStateModel<Data>,
    Data: Serialize + DeserializeOwned + Debug + Sized,
{
    fn to_dto(data: Data, version: RedisVersion) -> JM;
    fn try_from_dto(model: &JM) -> Result<Data, DTOErr>;
}

pub struct RedisJSONSnapshotRepository<State, JM>
where
    State: Send + Sync,
    JM: JsonModel + VersionedStateModel<State> + Send + Sync,
{
    client: Client,
    snapshot_expiry: Option<usize>,
    _st: PhantomData<State>,
    _jm: PhantomData<JM>,
}

impl<State, JM> RedisJSONSnapshotRepository<State, JM>
where
    State: Send + Sync,
    JM: JsonModel + VersionedStateModel<State> + Send + Sync,
{
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
impl<'a, State, JM> VersionedStreamSnapshotRepository<State>
    for RedisJSONSnapshotRepository<State, JM>
where
    State: Send
        + Sync
        + Serialize
        + DeserializeOwned
        + JsonModel
        + Clone
        + StateStream<String>
        + Debug
        + RedisJsonDTO<JM, State, RedisJsonDTOError>,
    JM: Send + Sync + JsonModel + VersionedStateModel<State>,
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
        let mut conn = self.get_connection().await?;
        let json_model = JM::get(stream.unwrap(), &mut conn)
            .await
            .map_err(RedisRepositoryError::ReadError)
            .map_err(VersionedRepositoryError::RepoErr)?;

        Ok((
            json_model.data(),
            RepositoryVersion::Exact(json_model.version()),
        ))
    }

    async fn save(
        &mut self,
        version: &Self::Version,
        state: &State,
    ) -> Result<State, VersionedRepositoryError<Self::Err, Self::Version>> {
        let mut conn = self.get_connection().await?;

        State::to_dto(state.clone(), version.clone())
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
    use serde::{Deserialize, Serialize};

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
