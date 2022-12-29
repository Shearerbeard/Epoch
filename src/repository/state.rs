use async_trait::async_trait;

use crate::decider::Command;

#[async_trait]
pub trait StateRepository<C: Command, Err> {
    async fn reify(&self) -> <C as Command>::State;
    async fn save(&mut self, state: &<C as Command>::State) -> Result<<C as Command>::State, Err>;
}

#[async_trait]
pub trait VersionedStateRepository<C: Command, Err> {
    type Version: Eq;

    async fn reify(&self) -> Result<(<C as Command>::State, Self::Version), Err>;
    async fn save(
        &mut self,
        version: &Self::Version,
        state: &<C as Command>::State,
    ) -> Result<<C as Command>::State, Err>;
}
