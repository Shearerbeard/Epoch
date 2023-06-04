use async_trait::async_trait;

use super::event::VersionedRepositoryError;

#[async_trait]
pub trait StateRepository<State, Err> {
    async fn reify(&self) -> Result<State, Err>;
    async fn save(&mut self, state: &State) -> Result<State, Err>;
}

#[async_trait]
pub trait VersionedStateRepository<'a, State, Err> {
    type Version;

    async fn reify(&self) -> Result<(State, Self::Version), Err>;
    async fn save(
        &mut self,
        version: &Self::Version,
        state: &State,
    ) -> Result<State, VersionedRepositoryError<Err>>
    where
        'a: 'async_trait,
        State: 'async_trait,
        Err: 'async_trait;
}
