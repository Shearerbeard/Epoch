use std::{fmt::Debug, sync::{Mutex, Arc}};

use async_trait::async_trait;

use crate::decider::{Command, Event};

use super::{EventRepository, StateRepository};

#[derive(Default)]
pub struct InMemoryEventRepository<E>
where
    E: Event + Clone + Send + Sync,
{
    state: Arc<Mutex<InMemoryEventRepositoryState<E>>> ,
}

#[async_trait]
impl<E> EventRepository<E, InMemoryEventRepositoryError> for InMemoryEventRepository<E>
where
    E: Event + Clone + Send + Sync,
{
    async fn load(&self) -> Result<Vec<E>, InMemoryEventRepositoryError> {
        let lock = self.state.lock().unwrap();
        Ok(lock.events.clone())
    }

    async fn append(&mut self, events: &Vec<E>) -> Result<Vec<E>, InMemoryEventRepositoryError> {
        let mut lock = self.state.lock().unwrap();
        lock.events.extend(events.to_owned());
        lock.position = lock.events.len();

        Ok(events.clone())
    }
}

impl<E> InMemoryEventRepository<E>
where
    E: Event + Clone + Send + Sync,
{
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(InMemoryEventRepositoryState {
                events: vec![],
                position: 0,
            })) 
        }
    }
}

#[derive(Debug, Default)]
struct InMemoryEventRepositoryState<E> {
    events: Vec<E>,
    position: usize
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
impl<C> StateRepository<C, InMemoryEventRepositoryError> for InMemoryStateRepository<C>
where
    C: Command + Send + Sync,
    <C as Command>::State: Default + Send + Sync + Debug,
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
