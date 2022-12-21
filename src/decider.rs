use std::{marker::PhantomData, collections::HashMap};

use async_trait::async_trait;

trait Command {
    type State: Default + Clone;
}
trait Event {}

#[async_trait]
trait Decider<S, Cmd: Command, E: Event, Err> {
    fn decide(cmd: Cmd, state: S) -> Result<Vec<E>, Err>;
    fn evolve(state: S, event: E) -> S;
    fn init() -> S;
}

trait EventRepository<C: Command, E: Event, Err> {
    fn load(&self) -> Result<Vec<E>, Err>;
    fn append(&mut self, events: Vec<E>) -> Result<Vec<E>, Err>;
}

trait LockingEventRepository {}

trait StateRepository<C: Command, Err> {
    fn reify(&self) -> <C as Command>::State;
    fn save(&mut self, state: <C as Command>::State) -> Result<<C as Command>::State, Err>;
}

trait LockingStateRepository {}

#[derive(Default)]
struct InMemoryEventRepository<C: Command, E: Event + Clone> {
    events: Vec<E>,
    position: usize,
    pd: PhantomData<C>,
}

impl<C: Command, E: Event + Clone> EventRepository<C, E, InMemoryEventRepositoryError>
    for InMemoryEventRepository<C, E>
{
    fn load(&self) -> Result<Vec<E>, InMemoryEventRepositoryError> {
        Ok(self.events.clone())
    }

    fn append(&mut self, events: Vec<E>) -> Result<Vec<E>, InMemoryEventRepositoryError> {
        self.events.extend(events.clone());
        self.position += 1;

        Ok(events)
    }
}

enum InMemoryEventRepositoryError {}

#[derive(Default)]
struct InMemoryStateRepository<C: Command> {
    state: <C as Command>::State,
}

impl <C: Command> StateRepository<C, InMemoryEventRepositoryError> for InMemoryStateRepository<C> {
    fn reify(&self) -> <C as Command>::State {
        self.state.clone()
    }

    fn save(&mut self, state: <C as Command>::State) -> Result<<C as Command>::State, InMemoryEventRepositoryError> {
        self.state = state.clone();
        Ok(state)
    }
}


enum InMemoryStateRepositoryError {}
