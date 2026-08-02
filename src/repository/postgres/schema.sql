-- Schema for Epoch's PostgreSQL event repository.
--
-- Applied by `PgEventRepository::migrate`; kept here as the canonical
-- reference. Events are immutable: no update or delete paths exist.

CREATE TABLE IF NOT EXISTS events (
    id BIGSERIAL PRIMARY KEY,

    -- Stream identification. stream_name is `{stream_type}-{entity_id}`;
    -- stream_type is the repository category (Epoch "stream name").
    stream_name VARCHAR(255) NOT NULL,
    stream_type VARCHAR(100) NOT NULL,

    -- Event metadata.
    event_type VARCHAR(255) NOT NULL,

    -- Per-stream ordering (optimistic concurrency) and global ordering
    -- (projections, cross-stream reads).
    sequence BIGINT NOT NULL,
    global_sequence BIGSERIAL NOT NULL,

    -- Serialized event payload.
    event_data JSONB NOT NULL,

    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT unique_stream_sequence UNIQUE (stream_name, sequence)
);

CREATE INDEX IF NOT EXISTS idx_events_stream_type_global_seq
    ON events (stream_type, global_sequence);
