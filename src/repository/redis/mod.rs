pub use redis_om::redis::*;

use redis_om::RedisError;
use thiserror::Error;

pub mod versioned_event;
pub mod versioned_stream_snapshot;

#[derive(Debug, Error)]
pub enum RedisRepositoryError {
    #[error("Redis connection error {0:?}")]
    ConnectionError(RedisError),
    #[error("Could not parse redis stream version {0:?}")]
    ParseVersion(String),
    #[error("Could not read stream: {0:?}")]
    ReadError(RedisError),
    #[error("Could not save state: {0:?}")]
    SaveError(RedisError),
    #[error("Could not parse event {0:?}")]
    ParseEvent(RedisError),
    #[error("Could not convert DTO to Event: {0:?}")]
    FromDTO(String),
}

#[derive(Debug, Eq, PartialEq, PartialOrd, Ord, Copy, Clone)]
pub struct RedisVersion {
    timestamp: usize,
    version: usize,
}

impl TryFrom<&str> for RedisVersion {
    type Error = RedisRepositoryError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        let mut split = value.split("-");

        Ok(Self {
            timestamp: split
                .next()
                .ok_or_else(|| Self::Error::ParseVersion(value.to_string()))?
                .parse()
                .map_err(|_e| Self::Error::ParseVersion(value.to_string()))?,
            version: split
                .next()
                .ok_or_else(|| Self::Error::ParseVersion(value.to_string()))?
                .parse()
                .map_err(|_e| Self::Error::ParseVersion(value.to_string()))?,
        })
    }
}

impl ToString for RedisVersion {
    fn to_string(&self) -> String {
        format!("{}-{}", self.timestamp, self.version)
    }
}
