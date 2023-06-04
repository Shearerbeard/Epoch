use std::fmt::Debug;
use std::marker::PhantomData;

use async_trait::async_trait;
use redis_om::Client;
use serde::{de::DeserializeOwned, Serialize};

use crate::decider::Event;

use super::{
    event::{VersionedEventRepositoryWithStreams, VersionedRepositoryError},
    RepositoryVersion,
};

#[derive(Debug)]
pub enum Error {}

#[derive(Debug)]
pub struct RedisStreamsEventRepository<E> {
    client: Client,
    stream_name: String,
    _hidden: PhantomData<E>,
}

#[async_trait]
impl<'a, E> VersionedEventRepositoryWithStreams<'a, E, Error> for RedisStreamsEventRepository<E>
where
    E: Event + Sync + Send + Serialize + DeserializeOwned + Clone + Debug,
{
    type StreamId = String;

    async fn load(
        &self,
        id: Option<&Self::StreamId>,
    ) -> Result<(Vec<E>, RepositoryVersion), VersionedRepositoryError<Error>> {
        todo!()
    }

    async fn load_from_version(
        &self,
        version: &RepositoryVersion,
        id: Option<&Self::StreamId>,
    ) -> Result<(Vec<E>, RepositoryVersion), VersionedRepositoryError<Error>> {
        todo!()
    }

    async fn append(
        &mut self,
        version: &RepositoryVersion,
        stream: &Self::StreamId,
        events: &Vec<E>,
    ) -> Result<(Vec<E>, RepositoryVersion), VersionedRepositoryError<Error>>
    where
        'a: 'async_trait,
        E: 'async_trait,
    {
        todo!()
    }
}
