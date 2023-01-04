pub(crate) mod deciders;
pub(crate) mod repository;

pub(crate) trait ValueType<T> {
    fn value(&self) -> T;
}
