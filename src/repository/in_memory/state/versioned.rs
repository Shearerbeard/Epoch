use std::{
    fmt::Debug,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;

use crate::repository::{state::VersionedStateRepository, RepositoryVersion};

#[derive(Debug, Clone)]
pub struct InMemoryStateRepository<State>
where
    State: Debug,
{
    state: Arc<Mutex<VersionedState<State>>>,
}

impl<State> InMemoryStateRepository<State>
where
    State: Debug,
{
    pub fn new(state: State) -> Self {
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
impl<'a, State> VersionedStateRepository<'a, State, Error> for InMemoryStateRepository<State>
where
    State: Debug + Clone + Send + Sync,
{
    type Version = RepositoryVersion;

    async fn reify(&self) -> Result<(State, Self::Version), Error> {
        let handle = self.state.lock().unwrap();

        Ok((handle.data.to_owned(), handle.version))
    }

    async fn save(&mut self, version: &Self::Version, state: &State) -> Result<State, Error> {
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
struct VersionedState<State>
where
    State: Debug,
{
    data: State,
    version: RepositoryVersion,
}

impl<State> VersionedState<State>
where
    State: Debug,
{
    fn new(data: State) -> Self {
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
    use crate::test_helpers::{
        deciders::user::UserDeciderState, repository::test_versioned_state_repository,
    };

    use super::*;

    #[actix_rt::test]
    async fn repository_spec_test() {
        let state_repository: InMemoryStateRepository<UserDeciderState> =
            InMemoryStateRepository::new(UserDeciderState::default());

        test_versioned_state_repository(state_repository).await;
    }
}
