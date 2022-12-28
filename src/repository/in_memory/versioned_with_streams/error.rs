use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Cannot append, event version is out of date")]
    VersionConflict,
}