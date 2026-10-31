-- Schema for Epoch's PostgreSQL event repository.
--
-- Applied by `PgEventRepository::migrate`; kept here as the canonical
-- reference. Events are immutable: no update or delete paths exist.
-- The namespace (stream_type) and the typed id's rendered key are
-- separate columns, so two streams can never alias into one.

CREATE TABLE IF NOT EXISTS events (
    -- Global ordering (projections, cross-stream reads); the sole
    -- surrogate id - nothing queries a separate row id.
    global_sequence BIGSERIAL PRIMARY KEY,

    -- Stream identification. stream_type is the repository category
    -- (Epoch "stream name"); stream_key is exactly what the typed id
    -- rendered.
    stream_type VARCHAR(100) NOT NULL,
    stream_key VARCHAR(255) NOT NULL,

    -- Event metadata.
    event_type VARCHAR(255) NOT NULL,

    -- Per-stream position (optimistic concurrency), 1-based and
    -- storage-enforced.
    sequence BIGINT NOT NULL CHECK (sequence > 0),

    -- Serialized event payload.
    event_data JSONB NOT NULL,

    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT unique_stream_position UNIQUE (stream_type, stream_key, sequence)
);

CREATE INDEX IF NOT EXISTS idx_events_stream_type_global_seq
    ON events (stream_type, global_sequence);
