use std::{error::Error, fmt::Debug};

use redis_om::RedisError;

use thiserror::Error;

pub mod versioned_event;
pub mod versioned_stream_snapshot;

#[derive(Debug, Error)]
pub enum RedisRepositoryError<DTOErr: Error + Debug> {
    #[error("Redis connection error {0:?}")]
    ConnectionError(RedisError),
    #[error("Redis Version Error {0:?}")] 
    Version(RedisVersionError),
    #[error("Could not read stream: {0:?}")]
    ReadError(RedisError),
    #[error("Could not save state: {0:?}")]
    SaveError(RedisError),
    #[error("Could not parse event {0:?}")]
    ParseDTO(RedisError),
    #[error("Could not convert DTO to Event: {0:?}")]
    FromDTO(DTOErr),
}

#[derive(Error, Debug)]
pub enum RedisVersionError {
    #[error("Could not parse redis stream version {0:?}")]
    ParseVersion(String),
}

#[derive(Debug, Eq, PartialEq, Copy, Clone)]
pub struct RedisVersion {
    timestamp: usize,
    version: usize,
}

impl Ord for RedisVersion {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        if self > other {
            std::cmp::Ordering::Greater
        } else if self < other {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Equal
        }
    }
}

impl PartialOrd for RedisVersion {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match self.timestamp.partial_cmp(&other.timestamp) {
            Some(core::cmp::Ordering::Equal) => {}
            ord => return ord,
        }
        self.version.partial_cmp(&other.version)
    }
}

impl TryFrom<&str> for RedisVersion
{
    type Error = RedisVersionError;

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

#[cfg(test)]
mod tests {
    use super::*;

    const TS1: &str = "1686947654949-0";
    const TS2: &str = "1686947654949-1";
    const TS3: &str = "1686947676187-0";
    const TS4: &str = "1686947676187-1";
    const TS5: &str = "1686947697295-0";
    const TS6: &str = "1686947697295-1";

    #[test]
    fn test_redis_version_to_from_str() {
        assert_eq!(TS1, RedisVersion::try_from(TS1).unwrap().to_string());
        assert_eq!(TS2, RedisVersion::try_from(TS2).unwrap().to_string());
        assert_eq!(TS3, RedisVersion::try_from(TS3).unwrap().to_string());
        assert_eq!(TS4, RedisVersion::try_from(TS4).unwrap().to_string());
        assert_eq!(TS5, RedisVersion::try_from(TS5).unwrap().to_string());
        assert_eq!(TS6, RedisVersion::try_from(TS6).unwrap().to_string());
    }

    #[test]
    fn test_redis_version_ordered() {
        let mut versions: Vec<RedisVersion> = vec![
            TS6.try_into().unwrap(),
            TS1.try_into().unwrap(),
            TS5.try_into().unwrap(),
            TS2.try_into().unwrap(),
            TS4.try_into().unwrap(),
            TS3.try_into().unwrap(),
        ];

        versions.sort();

        assert_eq!(
            versions
                .into_iter()
                .map(|v| v.to_string())
                .collect::<Vec<String>>(),
            vec![TS1, TS2, TS3, TS4, TS5, TS6]
        );
    }
}
