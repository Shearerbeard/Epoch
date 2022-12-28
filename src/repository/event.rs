use std::fmt::Debug;

use async_trait::async_trait;

use crate::decider::Event;
use super::RepositoryVersion;

#[async_trait]
pub trait EventRepository<E, Err>
where
    E: Event + Sync + Send,
{
    async fn load(&self) -> Result<Vec<E>, Err>;
    async fn append(&mut self, events: &Vec<E>) -> Result<Vec<E>, Err>;
}

#[async_trait]
pub trait VersionedEventRepository<E, Err>
where
    E: Event + Sync + Send,
{
    type Version: Eq;

    async fn load(&self) -> Result<(Vec<E>, &Self::Version), Err>;
    async fn load_from_version(&self) -> Result<(Vec<E>, Option<&Self::Version>), Err>;
    async fn append(
        &mut self,
        version: &Self::Version,
        events: &Vec<E>,
    ) -> Result<(Vec<E>, Self::Version), Err>;
}

// Lifetimes added here to fix codegen issue with macro generated lifetimes - adding 'a and 'async_trait prevents
// compile errors about E not living long enough for fn append
// https://stackoverflow.com/questions/69560112/how-to-use-rust-async-trait-generic-to-a-lifetime-parameter
// https://github.com/dtolnay/async-trait/issues/8
// https://play.rust-lang.org/?version=stable&mode=debug&edition=2018&gist=e977da3ddc0c21639b3116e123a94b6f
#[async_trait]
pub trait VersionedEventRepositoryWithStreams<'a, E, Err>
where
    E: Event + Sync + Send,
    Err: Debug
{
    type StreamId;

    async fn load(&self, id: Option<&Self::StreamId>) -> Result<(Vec<E>, RepositoryVersion), Err>;
    async fn load_from_version(
        &self,
        version: &RepositoryVersion,
        id: Option<&Self::StreamId>,
    ) -> Result<(Vec<E>, RepositoryVersion), Err>;
    async fn append(
        &mut self,
        version: &RepositoryVersion,
        stream: &Self::StreamId,
        events: &Vec<E>,
    ) -> Result<(Vec<E>, RepositoryVersion), Err>
    where
        'a: 'async_trait,
        E: 'async_trait;
}