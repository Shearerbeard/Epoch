use std::{
    fmt::Debug,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;

use crate::decider::{
    repository::{event::EventRepository, state::StateRepository},
    Command, Event,
};

use super::InMemoryEventRepositoryState;

#[derive(Default)]
pub struct InMemoryEventRepository<E>
where
    E: Event + Clone + Send + Sync,
{
    state: Arc<Mutex<InMemoryEventRepositoryState<E>>>,
}

impl<E> InMemoryEventRepository<E>
where
    E: Event + Clone + Send + Sync,
{
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(InMemoryEventRepositoryState::new())),
        }
    }
}

#[async_trait]
impl<E> EventRepository<E, ()> for InMemoryEventRepository<E>
where
    E: Event + Clone + Send + Sync,
{
    async fn load(&self) -> Result<Vec<E>, ()> {
        let lock = self.state.lock().unwrap();
        Ok(lock.events.clone())
    }

    async fn append(&mut self, events: &Vec<E>) -> Result<Vec<E>, ()> {
        let mut lock = self.state.lock().unwrap();
        lock.events.extend(events.to_owned());
        lock.position = lock.events.len();

        Ok(events.clone())
    }
}

impl<E> InMemoryEventRepositoryState<E> {
    pub fn new() -> Self {
        InMemoryEventRepositoryState {
            events: vec![],
            position: 0,
        }
    }
}

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
impl<C> StateRepository<C, ()> for InMemoryStateRepository<C>
where
    C: Command + Send + Sync,
    <C as Command>::State: Default + Send + Sync + Debug,
{
    async fn reify(&self) -> <C as Command>::State {
        self.state.clone()
    }

    async fn save(&mut self, state: &<C as Command>::State) -> Result<<C as Command>::State, ()> {
        self.state = state.clone();
        Ok(self.state.to_owned())
    }
}
