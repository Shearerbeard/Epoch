-- Migration step 1: the original stream_events table (ADR 0003).
--
-- IMMUTABLE once any database has applied it: this file is frozen as
-- step 1 of the versioned migrations (the migrations module embeds it
-- verbatim), and every schema change lands as a NEW numbered step,
-- never as an edit here. Namespaced away from the old repository
-- surface's `events` table so both can coexist until teardown is
-- assigned. Events are immutable: no update or delete paths exist.

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
