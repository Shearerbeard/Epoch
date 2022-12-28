pub(crate) mod deciders;

pub(crate) trait ValueType<T> {
    fn value(&self) -> T;
}