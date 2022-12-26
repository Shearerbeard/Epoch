use std::{marker::PhantomData, fmt::Debug};

use async_trait::async_trait;

use crate::decider::{Command, Event};

use super::{EventRepository, StateRepository};

#[derive(Default)]
pub struct InMemoryEventRepository<C: Command, E: Event + Clone + Send + Sync> {
    events: Vec<E>,
    position: usize,
    pd: PhantomData<C>,
}

#[async_trait]
impl<C: Command + Send + Sync, E: Event + Clone + Send + Sync>
    EventRepository<C, E, InMemoryEventRepositoryError> for InMemoryEventRepository<C, E>
{
    async fn load(&self) -> Result<Vec<E>, InMemoryEventRepositoryError> {
        Ok(self.events.clone())
    }

    async fn append(&mut self, events: Vec<E>) -> Result<Vec<E>, InMemoryEventRepositoryError> {
        self.events.extend(events.clone());
        self.position += 1;

        Ok(events)
    }
}

impl<C: Command, E: Event + Clone + Send + Sync> InMemoryEventRepository<C, E> {
    pub fn new() -> Self {
        let events: Vec<E> = vec![];

        Self {
            events,
            position: Default::default(),
            pd: PhantomData::<C>::default(),
        }
    }
}

#[derive(Debug)]
pub enum InMemoryEventRepositoryError {}

pub struct InMemoryStateRepository<C: Command> {
    state: <C as Command>::State,
}

impl<C> InMemoryStateRepository<C>
where
    C: Command + Send + Sync,
    <C as Command>::State: Default + Send + Sync + Debug,
{
    pub fn new() -> Self {
        Self {
            state: <C as Command>::State::default(),
        }
    }
}

#[async_trait]
impl<C> StateRepository<C, InMemoryEventRepositoryError>
    for InMemoryStateRepository<C>
    where
        C: Command + Send + Sync,
        <C as Command>::State: Default + Send + Sync + Debug
{
    async fn reify(&self) -> <C as Command>::State {
        self.state.clone()
    }

    async fn save(
        &mut self,
        state: &<C as Command>::State,
    ) -> Result<<C as Command>::State, InMemoryEventRepositoryError> {
        self.state = state.clone();
        Ok(self.state.to_owned())
    }
}

pub enum InMemoryStateRepositoryError {}
