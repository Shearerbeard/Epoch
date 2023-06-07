use std::fmt::Debug;
use std::marker::PhantomData;

use async_trait::async_trait;
use redis_om::{redis::aio::MultiplexedConnection, Client, RedisError, StreamModel};
use serde::{de::DeserializeOwned, Serialize};
use thiserror::Error;

use crate::decider::Event;

use super::{
    event::{VersionedEventRepositoryWithStreams, VersionedRepositoryError},
    RepositoryVersion,
};

pub trait StreamModelDTO<S>
where
    S: StreamModel,
{
    fn into_dto(self) -> S::Data;
    fn try_from_dto(model: S::Data) -> Result<Self, Error>
    where
        Self: Sized;
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("Redis connection error {0:?}")]
    ConnectionError(RedisError),
    #[error("Could not parse redis stream version {0:?}")]
    ParseVersion(String),
    #[error("Could not read stream: {0:?}")]
    ReadError(RedisError),
    #[error("Could not parse event {0:?}")]
    ParseEvent(RedisError),
    #[error("Could not convert DTO to Event: {0:?}")]
    FromDTO(String),
}

#[derive(Debug, Eq, PartialEq, PartialOrd, Ord)]
pub struct RedisVersion {
    timestamp: usize,
    version: usize,
}

impl TryFrom<&str> for RedisVersion {
    type Error = Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        let mut split = value.split("-");

        Ok(Self {
            timestamp: split
                .next()
                .ok_or_else(|| Self::Error::ParseVersion(value.to_string()))?
                .parse()
                .map_err(|_e| Self::Error::ParseVersion(value.to_string()))?,
            version: split
                .next()
                .ok_or_else(|| Self::Error::ParseVersion(value.to_string()))?
                .parse()
                .map_err(|_e| Self::Error::ParseVersion(value.to_string()))?,
        })
    }
}

impl ToString for RedisVersion {
    fn to_string(&self) -> String {
        format!("{}-{}", self.timestamp, self.version)
    }
}

#[derive(Debug)]
pub struct RedisStreamsEventRepository<E, SM, DTO>
where
    SM: StreamModel<Data = DTO>,
    E: StreamModelDTO<SM>,
{
    client: Client,
    stream_model: SM,
    _event: PhantomData<E>,
}

impl<E, SM, DTO> RedisStreamsEventRepository<E, SM, DTO>
where
    SM: StreamModel<Data = DTO>,
    E: StreamModelDTO<SM>,
{
    pub fn new(client: &Client, stream_model: SM) -> Self {
        Self {
            client: client.to_owned(),
            stream_model,
            _event: PhantomData::default(),
        }
    }

    pub async fn get_connection(
        &self,
    ) -> Result<MultiplexedConnection, VersionedRepositoryError<Error, RedisVersion>> {
        self.client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| VersionedRepositoryError::RepoErr(Error::ConnectionError(e)))
    }
}

#[async_trait]
impl<'a, E, SM, DTO> VersionedEventRepositoryWithStreams<'a, E, Error>
    for RedisStreamsEventRepository<E, SM, DTO>
where
    E: Event + Sync + Send + Serialize + DeserializeOwned + Clone + Debug + StreamModelDTO<SM>,
    SM: StreamModel<Data = DTO> + Send + Sync,
    DTO: Clone + Send + Sync + redis_om::FromRedisValue,
{
    type StreamId = String;
    type Version = RedisVersion;

    async fn load(
        &self,
        id: Option<&Self::StreamId>,
    ) -> Result<
        (Vec<E>, RepositoryVersion<RedisVersion>),
        VersionedRepositoryError<Error, RedisVersion>,
    > {
        let mut conn = self.get_connection().await?;

        let rv = self
            .stream_model
            .read(None, None, &mut conn)
            .await
            .map_err(Error::ReadError)
            .map_err(VersionedRepositoryError::RepoErr)?;

        let mut evts: Vec<E> = vec![];
        let mut redis_version = RepositoryVersion::NoStream;

        for raw_event in rv {
            let ev = raw_event
                .data::<DTO>()
                .map_err(Error::ParseEvent)
                .and_then(E::try_from_dto)
                .map_err(|_| Error::FromDTO("Placeholder".to_string()))
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
        VersionedRepositoryError<Error, RedisVersion>,
    > {
        let mut conn = self.get_connection().await?;

        let start = if let RepositoryVersion::Exact(v) = version {
            Some(v.to_string())
        } else {
            None
        };

        let rv = <SM as StreamModel>::range(start, Option::<String>::None, &mut conn)
            .await
            .map_err(Error::ReadError)
            .map_err(VersionedRepositoryError::RepoErr)?;

        let mut evts: Vec<E> = vec![];
        let mut redis_version = RepositoryVersion::NoStream;

        for raw_event in rv {
            let ev = raw_event
                .data::<DTO>()
                .map_err(Error::ParseEvent)
                .and_then(E::try_from_dto)
                .map_err(|_| Error::FromDTO("Placeholder".to_string()))
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
        stream: &Self::StreamId,
        events: &Vec<E>,
    ) -> Result<
        (Vec<E>, RepositoryVersion<RedisVersion>),
        VersionedRepositoryError<Error, RedisVersion>,
    >
    where
        'a: 'async_trait,
        E: 'async_trait,
    {
        let mut conn = self.get_connection().await?;
        let evts = events
            .iter()
            .map(|e| e.clone().into_dto())
            .collect::<Vec<DTO>>();

        let mut version = RepositoryVersion::NoStream;

        for e in evts {
            let version_str = SM::publish(&e, &mut conn)
                .await
                .map_err(|e| VersionedRepositoryError::RepoErr(Error::ConnectionError(e)))?;

            version =
                RepositoryVersion::Exact(RedisVersion::try_from(version_str.as_str()).unwrap());
        }

        Ok((events.to_owned(), version))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{
        deciders::user::{
            Guitar, User, UserCommand, UserDecider, UserDeciderCtx, UserDeciderState, UserEvent,
            UserId, UserName,
        },
        repository::test_versioned_event_repository_with_streams,
        ValueType,
    };

    #[actix_rt::test]
    async fn repository_spec_test() {
        // let base_stream = BASE_STREAM;
        // let client = store_from_environment(&base_stream.to_string(), vec![1, 2]).await;
        // let event_repository =
        //     ESDBEventRepository::<UserEvent>::new(&client, &base_stream.to_string());

        // let _ = test_versioned_event_repository_with_streams(event_repository).await;
    }
}
