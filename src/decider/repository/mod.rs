use async_trait::async_trait;

use super::{Command, Event};

#[cfg(feature = "in_memory")]
pub mod in_memory;
#[cfg(feature = "esdb")]
pub mod esdb;

// Event Repositories
#[async_trait]
pub trait EventRepository<C, E, Err>
where
    C: Command,
    E: Event + Sync + Send,
{
    async fn load(&self) -> Result<Vec<E>, Err>;
    async fn append(&mut self, events: Vec<E>) -> Result<Vec<E>, Err>;
}

#[async_trait]
pub trait LockingEventRepository<C, E, Err>
where
    C: Command,
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

#[async_trait]
pub trait LockingEventStoreWithStreams<'a, C, E, Err>
where
    C: Command,
    E: Event + Sync + Send,
{
    type StreamId;
    type Version: Eq;

    async fn load(&self, id: Option<Self::StreamId>) -> Result<(Vec<E>, Self::Version), Err>;
    async fn load_from_version(&self, version: Self::Version, id: Option<Self::StreamId>) -> Result<(Vec<E>, Self::Version), Err>;
    async fn append(
        &mut self,
        version: Self::Version,
        stream: Self::StreamId,
        events: Vec<E>,
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
pub trait LockingStateRepository<C: Command, Err> {
    type Version: Eq;

    async fn reify(&self) -> Result<(<C as Command>::State, Self::Version), Err>;
    async fn save(
        &mut self,
        version: Self::Version,
        state: &<C as Command>::State,
    ) -> Result<<C as Command>::State, Err>;
}
