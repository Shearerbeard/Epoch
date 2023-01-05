use thiserror::Error;

use crate::repository::event::VersionDiff;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Cannot append, event version is out of date")]
    VersionConflict(VersionDiff),
}
