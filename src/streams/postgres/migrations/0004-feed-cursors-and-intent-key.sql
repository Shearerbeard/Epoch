-- The feed's durable per-group progress (ADR 0010): one row per
-- (category, group), holding the ack cursor and the delivered
-- watermark. Both default to zero - nothing acknowledged, nothing
-- delivered. A group reading two categories holds two rows, each
-- legal watermark movement over the shared position space.
CREATE TABLE epoch_feed_cursors (
    category TEXT NOT NULL,
    group_name TEXT NOT NULL,
    cursor BIGINT NOT NULL DEFAULT 0,
    delivered_to BIGINT NOT NULL DEFAULT 0,
    CONSTRAINT epoch_feed_cursors_scope PRIMARY KEY (category, group_name),
    CONSTRAINT epoch_feed_cursors_no_backwards_watermark
        CHECK (delivered_to >= cursor)
);

-- The intent-key uniqueness the saga runner's reaction identity
-- rests on (ADR 0010 keyed envelope seam): within the outbox
-- category, an intent key may appear on exactly one event, so a
-- redelivered source event re-appending the same intent is rejected
-- by storage as a duplicate. The category name is indicative until
-- the runner card's gate A pins final naming; a rename lands as its
-- own migration step. Events carrying no intent key are untouched -
-- unique indexes treat the NULL expression as distinct.
CREATE UNIQUE INDEX stream_events_outbox_intent
    ON stream_events ((event_metadata->>'intent'))
    WHERE category = 'saga-outbox';
