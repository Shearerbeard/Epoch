-- Schema for Epoch's SQLite event repository.
--
-- Applied by `SqliteEventRepository::migrate`; kept here as the
-- canonical reference. Events are immutable: no update or delete
-- paths exist. The rowid doubles as the global sequence; AUTOINCREMENT
-- keeps ids monotonic and unreused. Keying mirrors the redesign's
-- stream_events table: the namespace (stream_type) and the typed id's
-- rendered key are separate columns, so no concatenation can alias
-- two streams and hierarchical keys stay unambiguous.

CREATE TABLE IF NOT EXISTS events (
    stream_type TEXT NOT NULL,
    stream_key TEXT NOT NULL,

    event_type TEXT NOT NULL,

    -- Per-stream position (optimistic concurrency), 1-based and
    -- storage-enforced; the rowid is the global ordering (projections,
    -- cross-stream reads).
    sequence INTEGER NOT NULL CHECK (sequence > 0),
    global_sequence INTEGER PRIMARY KEY AUTOINCREMENT,

    -- Serialized event payload.
    event_data TEXT NOT NULL CHECK (json_valid(event_data)),

    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),

    CONSTRAINT unique_stream_position UNIQUE (stream_type, stream_key, sequence)
);

CREATE INDEX IF NOT EXISTS idx_events_stream_type_global_seq
    ON events (stream_type, global_sequence);
