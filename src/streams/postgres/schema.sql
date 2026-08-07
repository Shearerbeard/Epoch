-- Schema for Epoch's PostgreSQL `EventStreams` backend (ADR 0003).
--
-- Applied by `PgEventStreams::migrate`; kept here as the canonical
-- reference. Namespaced away from the old repository surface's `events`
-- table so both can coexist until teardown is assigned. Events are
-- immutable: no update or delete paths exist.

CREATE TABLE IF NOT EXISTS stream_events (
    global_sequence BIGSERIAL PRIMARY KEY,

    -- The repository owns namespacing (ADR 0004): `category` is the
    -- repository's namespace and `stream_key` is exactly what the typed
    -- id rendered, so a stored key round-trips through
    -- `StreamId::parse_key` without a prefix to strip.
    category VARCHAR(255) NOT NULL,
    stream_key VARCHAR(255) NOT NULL,

    event_type VARCHAR(255) NOT NULL,

    -- 1-based per-stream position (ADR 0003), enforced in storage so a
    -- zero or negative position cannot be written at all. The unique
    -- constraint is the backstop behind the append path's advisory
    -- lock: a lost update would have to write a position already taken.
    sequence BIGINT NOT NULL CHECK (sequence > 0),

    event_data JSONB NOT NULL,

    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT stream_events_position UNIQUE (category, stream_key, sequence)
);

CREATE INDEX IF NOT EXISTS idx_stream_events_category
    ON stream_events (category, global_sequence);
