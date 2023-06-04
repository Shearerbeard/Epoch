use std::fmt::Debug;
use std::marker::PhantomData;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
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
    fn from_dto(model: S::Data) -> Self;
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("Redis connection error {0:?}")]
    RedisError(RedisError),
}

#[derive(Debug)]
pub struct RedisVersion {
    time: DateTime<Utc>,
    version: usize,
}

impl PartialEq for RedisVersion {
    fn eq(&self, other: &Self) -> bool {
        self.time == other.time && self.version == other.version
    }
}

impl Eq for RedisVersion {
    fn assert_receiver_is_total_eq(&self) {}
}

impl PartialOrd for RedisVersion {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match self.time.partial_cmp(&other.time) {
            Some(core::cmp::Ordering::Equal) => {}
            ord => return ord,
        }
        self.version.partial_cmp(&other.version)
    }
}

impl Ord for RedisVersion {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        todo!()
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
            .map_err(|e| VersionedRepositoryError::RepoErr(Error::RedisError(e)))
    }
}

#[async_trait]
impl<'a, E, SM, DTO> VersionedEventRepositoryWithStreams<'a, E, Error>
    for RedisStreamsEventRepository<E, SM, DTO>
where
    E: Event + Sync + Send + Serialize + DeserializeOwned + Clone + Debug + StreamModelDTO<SM>,
    SM: StreamModel<Data = DTO> + Send + Sync,
    DTO: Clone + Send + Sync,
{
    type StreamId = String;
    type Version = RedisVersion;

    async fn load(
        &self,
        id: Option<&Self::StreamId>,
    ) -> Result<(Vec<E>, RepositoryVersion<RedisVersion>), VersionedRepositoryError<Error, RedisVersion>> {
        todo!()
    }

    async fn load_from_version(
        &self,
        version: &RepositoryVersion<RedisVersion>,
        id: Option<&Self::StreamId>,
    ) -> Result<(Vec<E>, RepositoryVersion<RedisVersion>), VersionedRepositoryError<Error, RedisVersion>> {
        todo!()
    }

    async fn append(
        &mut self,
        version: &RepositoryVersion<RedisVersion>,
        stream: &Self::StreamId,
        events: &Vec<E>,
    ) -> Result<(Vec<E>, RepositoryVersion<RedisVersion>), VersionedRepositoryError<Error, RedisVersion>>
    where
        'a: 'async_trait,
        E: 'async_trait,
    {
        let mut conn = self.get_connection().await?;
        let evts = events
            .iter()
            .map(|e| e.clone().into_dto())
            .collect::<Vec<DTO>>();

        // let mut version: RepositoryVersion = RepositoryVersion::Any;

        for e in evts {
            let _ = SM::publish(&e, &mut conn)
                .await
                .map_err(|e| VersionedRepositoryError::RepoErr(Error::RedisError(e)))?;
        }

        todo!()
    }
}
