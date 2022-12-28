pub mod event;
pub mod state;
#[cfg(feature = "esdb")]
pub mod esdb;
#[cfg(feature = "in_memory")]
pub mod in_memory;

#[derive(Debug, PartialEq, Eq)]
pub enum RepositoryVersion {
    Any,
    Exact(usize),
    NoStream
}