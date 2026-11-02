-- Migration step 1: the stream_events table (ADR 0003).
--
-- IMMUTABLE once any database has applied it: this file is frozen as
-- step 1 of the versioned migrations (the migrations module embeds it
-- verbatim), and every schema change lands as a NEW numbered step,
-- never as an edit here. Namespaced away from the old repository
-- surface's `events` table so both can coexist until teardown is
-- assigned. Events are immutable: no update or delete paths exist.
--
-- The SQLite substitutes for PostgreSQL's types, and the reasoning is
-- in the design note:
--   * BIGSERIAL becomes INTEGER PRIMARY KEY AUTOINCREMENT. The
--     keyword is not needed for the feed's argument - a rolled-back
--     id is reused either way, because the high-water mark reverts
--     with the transaction - and it is kept only so a reader
--     comparing the two schemas does not have to re-derive that the
--     difference is immaterial.
--   * JSONB becomes TEXT with a json_valid check.
--   * TIMESTAMPTZ becomes TEXT written by strftime.
--   * VARCHAR(255) becomes TEXT with an explicit length check, so the
--     two backends accept and reject the same keys.

CREATE TABLE stream_events (
    global_sequence INTEGER PRIMARY KEY AUTOINCREMENT,

    -- The repository owns namespacing (ADR 0004): `category` is the
    -- repository's namespace and `stream_key` is exactly what the
    -- typed id rendered, so a stored key round-trips through
    -- `StreamId::parse_key` without a prefix to strip.
    category TEXT NOT NULL,
    stream_key TEXT NOT NULL,

    event_type TEXT NOT NULL,

    -- 1-based per-stream position (ADR 0003), enforced in storage so a
    -- zero or negative position cannot be written at all. The unique
    -- constraint is the backstop behind the append path's write lock:
    -- a lost update would have to write a position already taken.
    sequence INTEGER NOT NULL CHECK (sequence > 0),

    event_data TEXT NOT NULL CHECK (json_valid(event_data)),

    -- The keyed event envelope (ADR 0010): an opaque metadata map
    -- rides every event across the write path and the feed's
    -- delivery. Bare events carry the empty object.
    event_metadata TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(event_metadata)),

    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),

    CONSTRAINT unique_stream_position UNIQUE (category, stream_key, sequence),

    -- PostgreSQL's VARCHAR(255) counted characters, so this counts
    -- characters too: the two backends accept and reject the same
    -- keys.
    CONSTRAINT stream_events_text_bounds CHECK (
        length(category) <= 255
        AND length(stream_key) <= 255
        AND length(event_type) <= 255
    )
);

CREATE INDEX idx_stream_events_category
    ON stream_events (category, global_sequence);
