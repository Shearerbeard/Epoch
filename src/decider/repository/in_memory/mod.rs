use std::{
    collections::HashMap,
    fmt::Debug,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;

use crate::decider::{Command, Event};

use super::{EventRepository, StateRepository};

/// Events ///
#[derive(Default)]
pub struct SimpleInMemoryEventRepository<E>
where
    E: Event + Clone + Send + Sync,
{
    state: Arc<Mutex<InMemoryEventRepositoryState<E>>>,
}

#[async_trait]
impl<E> EventRepository<E, SimpleInMemoryError> for SimpleInMemoryEventRepository<E>
where
    E: Event + Clone + Send + Sync,
{
    async fn load(&self) -> Result<Vec<E>, SimpleInMemoryError> {
        let lock = self.state.lock().unwrap();
        Ok(lock.events.clone())
    }

    async fn append(&mut self, events: &Vec<E>) -> Result<Vec<E>, SimpleInMemoryError> {
        let mut lock = self.state.lock().unwrap();
        lock.events.extend(events.to_owned());
        lock.position = lock.events.len();

        Ok(events.clone())
    }
}

impl<E> SimpleInMemoryEventRepository<E>
where
    E: Event + Clone + Send + Sync,
{
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(InMemoryEventRepositoryState {
                events: vec![],
                position: 0,
            })),
        }
    }
}

#[derive(Debug, Default)]
struct InMemoryEventRepositoryState<E> {
    events: Vec<E>,
    position: usize,
}

#[derive(Debug)]
pub enum SimpleInMemoryError {}

pub struct StreamedVersionedInMemoryEventRepository<E>
where
    E: Event + Sync + Send,
{
    state: HashMap<String, Arc<Mutex<InMemoryEventRepositoryState<E>>>>,
}

/// State ///
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
impl<C> StateRepository<C, SimpleInMemoryError> for InMemoryStateRepository<C>
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
    ) -> Result<<C as Command>::State, SimpleInMemoryError> {
        self.state = state.clone();
        Ok(self.state.to_owned())
    }
}

pub enum InMemoryStateRepositoryError {}
