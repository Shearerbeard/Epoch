use std::{
    fmt::Debug,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;

use crate::repository::{
    event::{VersionDiff, VersionedRepositoryError},
    state::VersionedStateRepository,
    RepositoryVersion,
};

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

    fn version_to_usize(
        version: &RepositoryVersion<usize>,
    ) -> Result<usize, VersionedRepositoryError<Error, usize>> {
        match version {
            RepositoryVersion::Exact(exact) => Ok(exact.to_owned()),
            RepositoryVersion::NoStream => Ok(0),
            RepositoryVersion::StreamExists => Ok(0),
            RepositoryVersion::Any => Err(VersionedRepositoryError::RepoErr(
                Error::ExactStreamVersionMustBeKnown,
            )),
        }
    }

    fn version_check(
        current: &RepositoryVersion<usize>,
        incoming: &RepositoryVersion<usize>,
    ) -> Result<(), VersionedRepositoryError<Error, usize>> {
        if Self::version_to_usize(current)? == Self::version_to_usize(incoming)? {
            Ok(())
        } else {
            Err(VersionedRepositoryError::VersionConflict(VersionDiff::new(
                current.to_owned(),
                incoming.to_owned(),
            )))
        }
    }

    fn bump_version(
        version: &RepositoryVersion<usize>,
    ) -> Result<RepositoryVersion<usize>, VersionedRepositoryError<Error, usize>> {
        Ok(RepositoryVersion::Exact(
            Self::version_to_usize(version)? + 1,
        ))
    }
}

#[async_trait]
impl<'a, State> VersionedStateRepository<'a, State, Error> for InMemoryStateRepository<State>
where
    State: Debug + Clone + Send + Sync,
{
    type Version = usize;

    async fn reify(&self) -> Result<(State, RepositoryVersion<Self::Version>), Error> {
        let handle = self.state.lock().unwrap();

        Ok((handle.data.to_owned(), handle.version))
    }

    async fn save(
        &mut self,
        version: &RepositoryVersion<Self::Version>,
        state: &State,
    ) -> Result<State, VersionedRepositoryError<Error, usize>> {
        let handle_lock = self.state.lock();
        let mut handle = handle_lock.unwrap();

        let _ = Self::version_check(&handle.version, version)?;

        handle.data = state.clone();
        handle.version = Self::bump_version(&handle.version)?;

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
    version: RepositoryVersion<usize>,
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
