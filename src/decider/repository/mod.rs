use async_trait::async_trait;

use super::{Command, Event};

#[cfg(feature = "esdb")]
pub mod esdb;
#[cfg(feature = "in_memory")]
pub mod in_memory;

// Event Repositories
#[async_trait]
pub trait EventRepository<E, Err>
where
    E: Event + Sync + Send,
{
    async fn load(&self) -> Result<Vec<E>, Err>;
    async fn append(&mut self, events: Vec<E>) -> Result<Vec<E>, Err>;
}

#[async_trait]
pub trait VersionedEventRepository<E, Err>
where
    E: Event + Sync + Send,
{
    type Version: Eq;

    async fn load(&self) -> Result<(Vec<E>, Self::Version), Err>;
    async fn append(
        &mut self,
        version: Self::Version,
        events: Vec<E>,
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
{
    type StreamId;
    type Version: Eq;

    async fn load(&self, id: Option<&Self::StreamId>) -> Result<(Vec<E>, Self::Version), Err>;
    async fn load_from_version(
        &self,
        version: &Self::Version,
        id: Option<&Self::StreamId>,
    ) -> Result<(Vec<E>, Self::Version), Err>;
    async fn append(
        &mut self,
        version: &Self::Version,
        stream: &Self::StreamId,
        events: &Vec<E>,
    ) -> Result<(Vec<E>, Self::Version), Err>
    where
        'a: 'async_trait,
        E: 'async_trait;
}

// State Repositories
#[async_trait]
pub trait StateRepository<C: Command, Err> {
    async fn reify(&self) -> <C as Command>::State;
    async fn save(&mut self, state: &<C as Command>::State) -> Result<<C as Command>::State, Err>;
}

#[async_trait]
pub trait VersionedStateRepository<C: Command, Err> {
    type Version: Eq;

    async fn reify(&self) -> Result<(<C as Command>::State, Self::Version), Err>;
    async fn save(
        &mut self,
        version: Self::Version,
        state: &<C as Command>::State,
    ) -> Result<<C as Command>::State, Err>;
}
