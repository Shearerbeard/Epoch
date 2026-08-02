use serde::{Deserialize, Serialize};

/// Per-stream sequence number backing optimistic concurrency for the
/// PostgreSQL repository. Starts at 1 for a stream's first event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PgVersion {
    sequence: i64,
}

impl PgVersion {
    pub fn sequence(&self) -> i64 {
        self.sequence
    }
}

impl From<i64> for PgVersion {
    fn from(sequence: i64) -> Self {
        Self { sequence }
    }
}
