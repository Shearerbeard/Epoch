use thiserror::Error;

#[derive(Debug, Error)]
pub enum PgRepositoryError {
    #[error("Connection error: {0}")]
    Connection(#[from] tokio_postgres::Error),

    #[error("Connection pool error: {0}")]
    Pool(#[from] bb8::RunError<tokio_postgres::Error>),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Invalid connection config: {0}")]
    Config(String),
}
