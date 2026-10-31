/// Per-stream sequence number backing optimistic concurrency for the
/// SQLite repository. Starts at 1 for a stream's first event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SqliteVersion {
    sequence: i64,
}

impl SqliteVersion {
    pub fn sequence(&self) -> i64 {
        self.sequence
    }
}

impl From<i64> for SqliteVersion {
    fn from(sequence: i64) -> Self {
        Self { sequence }
    }
}
