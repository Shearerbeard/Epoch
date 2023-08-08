pub(crate) mod deciders;
#[cfg(feature = "redis")]
pub(crate) mod redis;
pub(crate) mod repository;

pub(crate) trait ValueType<T> {
    fn value(&self) -> T;
}
