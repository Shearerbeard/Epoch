use std::error::Error;
use std::fmt::Debug;
use std::marker::PhantomData;

use async_trait::async_trait;
use redis_om::RedisError;
use redis_om::{redis::aio::MultiplexedConnection, Client, StreamModel};
use serde::{de::DeserializeOwned, Serialize};

use crate::decider::Event;

use crate::repository::{event::VersionedEventRepositoryWithStreams, RepositoryVersion};
use crate::repository::{VersionDiff, VersionedRepositoryError, WithFineGrainedStreamId};

use super::{RedisRepositoryError, RedisVersion};

pub trait StreamModelDTO<SM, DTOErr>
where
    SM: StreamModel,
    DTOErr: Error + Debug,
{
    fn into_dto(self) -> SM::Data;
    fn try_from_dto(model: SM::Data) -> Result<Self, DTOErr>
    where
        Self: Sized;
}

#[derive(Debug, Clone)]
pub struct RedisStreamsEventRepository<SM, DTO>
where
    SM: StreamModel<Data = DTO>,
{
    client: Client,
    _sm: PhantomData<SM>,
}

impl<SM, DTO> RedisStreamsEventRepository<SM, DTO>
where
    SM: StreamModel<Data = DTO>,
{
    pub fn new(client: &Client) -> Self {
        Self {
            client: client.to_owned(),
            _sm: PhantomData::default(),
        }
    }

    pub async fn get_connection(&self) -> Result<MultiplexedConnection, RedisError> {
        self.client.get_multiplexed_async_connection().await
    }
}

#[async_trait]
impl<'a, E, SM, DTO, DTOErr>
    VersionedEventRepositoryWithStreams<'a, E, RedisRepositoryError<DTOErr>>
    for RedisStreamsEventRepository<SM, DTO>
where
    E: Event
        + Sync
        + Send
        + Serialize
        + DeserializeOwned
        + Clone
        + Debug
        + StreamModelDTO<SM, DTOErr>,
    SM: StreamModel<Data = DTO> + Send + Sync,
    DTO: WithFineGrainedStreamId + Clone + Send + Sync + redis_om::FromRedisValue,
    DTOErr: Debug + Error + Send + Sync + Clone,
{
    type StreamId = String;
    type Version = RedisVersion;

    async fn load(
        &self,
        id: Option<&Self::StreamId>,
    ) -> Result<
        (Vec<E>, RepositoryVersion<RedisVersion>),
        VersionedRepositoryError<RedisRepositoryError<DTOErr>, RedisVersion>,
    > {
        let mut conn = self
            .get_connection()
            .await
            .map_err(RedisRepositoryError::ConnectionError)
            .map_err(VersionedRepositoryError::RepoErr)?;

        let rv = <SM as StreamModel>::range("-", "+", &mut conn)
            .await
            .map_err(RedisRepositoryError::ReadError)
            .map_err(VersionedRepositoryError::RepoErr)?;

        let mut evts: Vec<E> = vec![];
        let mut redis_version = RepositoryVersion::NoStream;

        for raw_event in rv {
            let dto = raw_event
                .data::<DTO>()
                .map_err(RedisRepositoryError::ParseDTO)
                .map_err(VersionedRepositoryError::RepoErr)?;

            // Filter out events not belonging to sub stream
            // Redis does not currently support filtering streams
            // https://github.com/redis/redis/issues/5827
            if let Some(stream_id) = id {
                if !dto.fine_grained_eq(stream_id) {
                    continue;
                }
            }

            let ev = E::try_from_dto(dto)
                .map_err(RedisRepositoryError::FromDTO)
                .map_err(VersionedRepositoryError::RepoErr)?;

            redis_version =
                RepositoryVersion::Exact(RedisVersion::try_from(raw_event.id.as_ref()).unwrap());

            evts.push(ev);
        }

        Ok((evts, redis_version))
    }

    async fn load_from_version(
        &self,
        version: &RepositoryVersion<RedisVersion>,
        id: Option<&Self::StreamId>,
    ) -> Result<
        (Vec<E>, RepositoryVersion<RedisVersion>),
        VersionedRepositoryError<RedisRepositoryError<DTOErr>, RedisVersion>,
    > {
        let mut conn = self
            .get_connection()
            .await
            .map_err(RedisRepositoryError::ConnectionError)
            .map_err(VersionedRepositoryError::RepoErr)?;

        let start = if let RepositoryVersion::Exact(v) = version {
            v.to_string()
        } else {
            "-".to_string()
        };

        let rv = <SM as StreamModel>::range(start, "+".to_string(), &mut conn)
            .await
            .map_err(RedisRepositoryError::ReadError)
            .map_err(VersionedRepositoryError::RepoErr)?;

        let mut evts: Vec<E> = vec![];
        let mut redis_version = RepositoryVersion::NoStream;

        for raw_event in rv {
            let dto = raw_event
                .data::<DTO>()
                .map_err(RedisRepositoryError::ParseDTO)
                .map_err(VersionedRepositoryError::RepoErr)?;

            // Filter out events not belonging to sub stream
            // Redis does not currently support filtering streams
            // https://github.com/redis/redis/issues/5827
            if let Some(stream_id) = id {
                if !dto.fine_grained_eq(stream_id) {
                    continue;
                }
            }

            let ev = E::try_from_dto(dto)
                .map_err(RedisRepositoryError::FromDTO)
                .map_err(VersionedRepositoryError::RepoErr)?;

            redis_version =
                RepositoryVersion::Exact(RedisVersion::try_from(raw_event.id.as_ref()).unwrap());

            evts.push(ev);
        }

        Ok((evts, redis_version))
    }

    async fn append(
        &mut self,
        version: &RepositoryVersion<RedisVersion>,
        _stream: &Self::StreamId,
        events: &Vec<E>,
    ) -> Result<
        (Vec<E>, RepositoryVersion<RedisVersion>),
        VersionedRepositoryError<RedisRepositoryError<DTOErr>, RedisVersion>,
    >
    where
        'a: 'async_trait,
        E: 'async_trait,
    {
        let mut conn = self
            .get_connection()
            .await
            .map_err(RedisRepositoryError::ConnectionError)
            .map_err(VersionedRepositoryError::RepoErr)?;

        match version {
            RepositoryVersion::Any => {}
            RepositoryVersion::Exact(v) => {
                let res = <SM as StreamModel>::range(v.to_string(), "-".to_string(), &mut conn)
                    .await
                    .map_err(RedisRepositoryError::ReadError)
                    .map_err(VersionedRepositoryError::RepoErr)?;

                if res.len() > 1 {
                    let last_message_id = res.last().unwrap().id.to_owned();

                    return Err(VersionedRepositoryError::VersionConflict(VersionDiff::new(
                        *version,
                        RepositoryVersion::Exact(
                            RedisVersion::try_from(last_message_id.as_ref()).unwrap(),
                        ),
                    )));
                }
            }
            RepositoryVersion::NoStream => {
                let len = <SM as StreamModel>::len(&mut conn)
                    .await
                    .map_err(RedisRepositoryError::ReadError)
                    .map_err(VersionedRepositoryError::RepoErr)?;

                if len > 0 {
                    return Err(VersionedRepositoryError::VersionConflict(VersionDiff::new(
                        *version,
                        RepositoryVersion::StreamExists,
                    )));
                }
            }
            RepositoryVersion::StreamExists => {
                let len = <SM as StreamModel>::len(&mut conn)
                    .await
                    .map_err(RedisRepositoryError::ReadError)
                    .map_err(VersionedRepositoryError::RepoErr)?;

                if len == 0 {
                    return Err(VersionedRepositoryError::VersionConflict(VersionDiff::new(
                        *version,
                        RepositoryVersion::NoStream,
                    )));
                }
            }
        }

        let evts = events
            .iter()
            .map(|e| e.clone().into_dto())
            .collect::<Vec<DTO>>();

        let mut version = RepositoryVersion::NoStream;

        for e in evts {
            let version_str = SM::publish(&e, &mut conn).await.map_err(|e| {
                VersionedRepositoryError::RepoErr(RedisRepositoryError::ConnectionError(e))
            })?;

            version =
                RepositoryVersion::Exact(RedisVersion::try_from(version_str.as_str()).unwrap());
        }

        Ok((events.to_owned(), version))
    }
}

#[cfg(test)]
mod tests {
    use redis_om::redis::streams::StreamMaxlen;

    use super::*;
    use crate::test_helpers::{
        deciders::user::UserEvent,
        redis::{TestUserDTOErr, TestUserEventDTO, TestUserEventDTOManager},
        repository::{
            versioned_event_repository_with_streams_occ_spec,
            versioned_event_repository_with_streams_spec,
        },
    };

    async fn client_from_environment() -> Client {
        let _ = dotenv::dotenv().expect("File .env or Env Vars not found");

        let settings: String = dotenv::var("REDIS_CONNECTION_STRING")
            .expect("Redis to be set in env")
            .parse()
            .expect("Redis connection string to parse");

        let client = Client::open(settings).expect("Redis Client");

        let mut conn = client.get_multiplexed_async_connection().await.unwrap();
        TestUserEventDTOManager::trim(StreamMaxlen::Equals(0), &mut conn)
            .await
            .unwrap();

        client
    }

    #[actix_rt::test]
    async fn repository_spec_tests() {
        let client = client_from_environment().await;
        let event_repository =
            RedisStreamsEventRepository::<TestUserEventDTOManager, TestUserEventDTO>::new(&client);

        // Run both tests in sequence because we cannot specify a stream identifier per test in redis
        let _ = versioned_event_repository_with_streams_spec(event_repository.clone()).await;
        let _ = versioned_event_repository_with_streams_occ_spec(event_repository).await;
    }
}
