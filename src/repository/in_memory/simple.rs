use std::{
    fmt::Debug,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;

use crate::{
    decider::Event,
    repository::{event::EventRepository, state::StateRepository},
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

pub struct InMemoryStateRepository<State> {
    state: State,
}

impl<State> InMemoryStateRepository<State>
where
    State: Default + Send + Sync + Debug + Clone,
{
    pub fn new() -> Self {
        Self::default()
    }
}

impl<State> Default for InMemoryStateRepository<State>
where
    State: Default,
{
    fn default() -> Self {
        Self {
            state: State::default(),
        }
    }
}

#[async_trait]
impl<State> StateRepository<State, ()> for InMemoryStateRepository<State>
where
    State: Default + Send + Sync + Debug + Clone,
{
    async fn reify(&self) -> State {
        self.state.clone()
    }

    async fn save(&mut self, state: &State) -> Result<State, ()> {
        self.state = state.clone();
        Ok(self.state.to_owned())
    }
}
