use std::{
    fmt::Debug,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;

use crate::{
    decider::Command,
    repository::{state::VersionedStateRepository, RepositoryVersion},
};

#[derive(Debug, Clone)]
pub struct InMemoryStateRepository<C>
where
    C: Command + Debug,
    <C as Command>::State: Debug,
{
    state: Arc<Mutex<VersionedState<C>>>,
}

impl<C> InMemoryStateRepository<C>
where
    C: Command + Debug,
    <C as Command>::State: Debug,
{
    fn new(state: <C as Command>::State) -> Self {
        Self {
            state: Arc::new(Mutex::new(VersionedState::new(state))),
        }
    }

    fn version_to_usize(version: &RepositoryVersion) -> Result<usize, Error> {
        if let RepositoryVersion::Exact(exact) = version {
            Ok(exact.to_owned())
        } else {
            Err(Error::ExactStreamVersionMustBeKnown)
        }
    }

    fn version_check(
        current: &RepositoryVersion,
        incoming: &RepositoryVersion,
    ) -> Result<(), Error> {
        if let &RepositoryVersion::StreamExists = current {
            return if let &RepositoryVersion::Exact(_) = incoming {
                Ok(())
            } else {
                Err(Error::ExactStreamVersionMustBeKnown)
            };
        }

        if Self::version_to_usize(current)? < Self::version_to_usize(incoming)? {
            Ok(())
        } else {
            Err(Error::VersionOutOfDate)
        }
    }
}

#[async_trait]
impl<C> VersionedStateRepository<C, Error> for InMemoryStateRepository<C>
where
    C: Command + Debug,
    <C as Command>::State: Debug,
{
    type Version = RepositoryVersion;

    async fn reify(&self) -> Result<(<C as Command>::State, Self::Version), Error> {
        let handle = self.state.lock().unwrap();

        Ok((handle.data.to_owned(), handle.version))
    }

    async fn save(
        &mut self,
        version: &Self::Version,
        state: &<C as Command>::State,
    ) -> Result<<C as Command>::State, Error> {
        let handle_lock = self.state.lock();
        let mut handle = handle_lock.unwrap();

        let _ = Self::version_check(&handle.version, version)?;

        handle.data = state.clone();
        handle.version = version.to_owned();

        drop(handle);

        Ok(state.to_owned())
    }
}

#[derive(Debug, Clone)]
struct VersionedState<C>
where
    C: Command + Debug,
    <C as Command>::State: Debug,
{
    data: <C as Command>::State,
    version: RepositoryVersion,
}

impl<C> VersionedState<C>
where
    C: Command + Debug,
    <C as Command>::State: Debug,
{
    fn new(data: <C as Command>::State) -> Self {
        Self {
            data,
            version: RepositoryVersion::StreamExists,
        }
    }
}


#[derive(Debug, Clone)]
pub enum Error {
    ExactStreamVersionMustBeKnown,
    VersionOutOfDate,
}

#[cfg(test)]
mod tests {
    use crate::test_helpers::{deciders::user::{UserCommand, UserDeciderState}, repository::test_versioned_state_repository};

    use super::*;

    #[actix_rt::test]
    async fn repository_spec_test() {
        let state_repository: InMemoryStateRepository<UserCommand> =
            InMemoryStateRepository::new(UserDeciderState::default());

        test_versioned_state_repository(state_repository).await;
    }
}
