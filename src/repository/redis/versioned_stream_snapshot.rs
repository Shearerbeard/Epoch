use std::{fmt::Debug, marker::PhantomData};

use async_trait::async_trait;
use redis_om::{redis::aio::MultiplexedConnection, Client, JsonModel, RedisError};
use serde::{de::DeserializeOwned, Serialize};
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
}

#[derive(Debug, Error)]
pub enum RedisJsonDTOError {
    #[error("Could not deserialize")]
    Deserialize,
}

pub trait VersionedRedisJsonDTO<Data: Clone> {
    fn id(&self) -> String;
    fn version(&self) -> RedisVersion;
    fn data(&self) -> Data;
    fn new(id: String, version: RedisVersion, data: Data) -> Self;
}

pub trait WithVersionedRedisDTO<JM>: Sized + Clone
where
    JM: JsonModel + VersionedRedisJsonDTO<Self>,
{
    fn to_dto(&self, version: RedisVersion) -> JM;
}

pub struct RedisJSONSnapshotRepository<State, JM>
where
    State: Send + Sync + Clone,
    JM: JsonModel + VersionedRedisJsonDTO<State> + Send + Sync,
{
    client: Client,
    snapshot_expiry: Option<usize>,
    _st: PhantomData<State>,
    _jm: PhantomData<JM>,
}

impl<State, JM> RedisJSONSnapshotRepository<State, JM>
where
    State: Send + Sync + Clone,
    JM: JsonModel + VersionedRedisJsonDTO<State> + Send + Sync,
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

    pub fn new(client: &Client) -> Self {
        Self {
            client: client.clone(),
            snapshot_expiry: None,
            _st: PhantomData::default(),
            _jm: PhantomData::default(),
        }
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
        + WithVersionedRedisDTO<JM>,
    JM: Send + Sync + JsonModel + VersionedRedisJsonDTO<State>,
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

        state
            .to_dto(version.clone())
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

    const TS: &str = "1686947654949-0";

    #[derive(JsonModel, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
    struct TestModel {
        id: String,
    }

    #[derive(JsonModel, Serialize, Deserialize)]
    struct TestModelDTO {
        id: String,
        version: RedisVersion,
        data: TestModel,
    }

    impl StateStream<String> for TestModel {
        fn to_stream_id(&self) -> String {
            self.id.clone()
        }
    }

    impl WithVersionedRedisDTO<TestModelDTO> for TestModel {
        fn to_dto(&self, version: RedisVersion) -> TestModelDTO {
            TestModelDTO::new(self.id.clone(), version, self.to_owned())
        }
    }

    impl VersionedRedisJsonDTO<TestModel> for TestModelDTO {
        fn id(&self) -> String {
            self.id.clone()
        }

        fn version(&self) -> RedisVersion {
            self.version.clone()
        }

        fn data(&self) -> TestModel {
            self.data.clone()
        }

        fn new(id: String, version: RedisVersion, data: TestModel) -> Self {
            Self { id, version, data }
        }
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

        let test = TestModel {
            id: "1".to_string(),
        };

        let mut repository: RedisJSONSnapshotRepository<TestModel, TestModelDTO> =
            RedisJSONSnapshotRepository::new(&client);

        let res1 = repository
            .save(&RedisVersion::try_from(TS).unwrap(), &test)
            .await
            .unwrap();

        let (res2, version) = repository.reify(Some(test.to_stream_id())).await.unwrap();

        if let RepositoryVersion::Exact(v) = version {
            assert_eq!(v.to_string(), TS.to_string());
            assert_eq!(res1, res2);
        } else {
            panic!("Should return exact repository version")
        }
    }
}
