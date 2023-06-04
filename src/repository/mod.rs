use serde::{Deserialize, Serialize};

#[cfg(feature = "esdb")]
pub mod esdb;
#[cfg(feature = "redis")]
pub mod redis;
pub mod event;
#[cfg(feature = "in_memory")]
pub mod in_memory;
pub mod state;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepositoryVersion<V> {
    Any,
    Exact(V),
    NoStream,
    StreamExists,
}
