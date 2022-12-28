use std::fmt::Debug;

pub mod simple;
pub mod versioned_with_streams;

#[derive(Debug, Default)]
pub(crate) struct InMemoryEventRepositoryState<E> {
    events: Vec<E>,
    position: usize,
}
