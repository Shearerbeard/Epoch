use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::decider::Event;

#[cfg(feature = "esdb")]
pub mod esdb;
pub mod event;
#[cfg(feature = "in_memory")]
pub mod in_memory;
#[cfg(feature = "redis")]
pub mod redis;
pub mod state;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepositoryVersion<V> {
    Any,
    Exact(V),
    NoStream,
    StreamExists,
}

#[derive(Debug, Error)]
pub enum VersionedRepositoryError<RepoErr, V> {
    #[error("Version conflict {0:?}")]
    VersionConflict(VersionDiff<V>),
    #[error("Repository Error {0}")]
    RepoErr(RepoErr),
}

#[derive(Debug)]
pub struct VersionDiff<V> {
    expected: RepositoryVersion<V>,
    actual: RepositoryVersion<V>,
}

impl<V: Clone> VersionDiff<V> {
    pub fn new(expected: RepositoryVersion<V>, actual: RepositoryVersion<V>) -> Self {
        Self { expected, actual }
    }

    pub fn expected(&self) -> RepositoryVersion<V> {
        self.expected.to_owned()
    }

    pub fn actual(&self) -> RepositoryVersion<V> {
        self.actual.to_owned()
    }
}

pub trait WithFineGrainedStreamId {
    fn to_fine_grained_id(&self) -> String;
    fn fine_grained_eq(&self, comp: &str) -> bool {
        comp == self.to_fine_grained_id()
    }
}

pub trait StreamIdFromEvent<Evt: Event>: Sized {
    fn from(e: Evt) -> Self {
        Self::event_entity_id_into(e.get_id())
    }

    fn event_entity_id_into(id: <Evt as Event>::EntityId) -> Self;
}
