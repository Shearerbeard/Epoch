use thiserror::Error;
use tokio_rusqlite::rusqlite;

#[derive(Debug, Error)]
pub enum SqliteRepositoryError {
    #[error("Connection handle error: {0}")]
    Handle(#[from] tokio_rusqlite::Error),

    #[error("Connection error: {0}")]
    Connection(#[from] rusqlite::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}
