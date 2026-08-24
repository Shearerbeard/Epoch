-- The keyed event envelope (ADR 0010): an opaque metadata map rides
-- every event across the write path and the feed's delivery. Bare
-- events carry the empty object; reaction keys (the saga runner's
-- intent keys) are the envelope's first rider. The outbox stream's
-- uniqueness index over the intent key arrives with the
-- allocation-ledger step.
ALTER TABLE stream_events
    ADD COLUMN event_metadata JSONB NOT NULL DEFAULT '{}'::jsonb;
