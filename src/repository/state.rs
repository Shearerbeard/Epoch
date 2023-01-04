use async_trait::async_trait;

#[async_trait]
pub trait StateRepository<State, Err> {
    async fn reify(&self) -> State;
    async fn save(&mut self, state: &State) -> Result<State, Err>;
}

#[async_trait]
pub trait VersionedStateRepository<State, Err> {
    type Version: Eq;

    async fn reify(&self) -> Result<(State, Self::Version), Err>;
    async fn save(&mut self, version: &Self::Version, state: &State) -> Result<State, Err>;
}
