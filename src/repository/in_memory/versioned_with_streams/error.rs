use thiserror::Error;

use crate::repository::VersionDiff;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Cannot append, event version is out of date")]
    VersionConflict(VersionDiff<usize>),
}
