#[cfg(feature = "esdb")]
pub mod esdb;
pub mod event;
#[cfg(feature = "in_memory")]
pub mod in_memory;
pub mod state;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositoryVersion {
    Any,
    Exact(usize),
    NoStream,
    StreamExists,
}
