use async_trait::async_trait;

#[async_trait]
pub trait StateRepository<State, Err> {
    async fn reify(&self) -> State;
    async fn save(&mut self, state: &State) -> Result<State, Err>;
}

#[async_trait]
pub trait VersionedStateRepository<'a, State, Err>
where
    State: Send + Sync,
    Err: Send + Sync,
{
    type Version: Eq + Send + Sync;

    async fn reify(&self) -> Result<(State, Self::Version), Err>;
    async fn save(&mut self, version: &Self::Version, state: &State) -> Result<State, Err>
    where
        'a: 'async_trait,
        State: 'async_trait,
        Err: 'async_trait;
}
