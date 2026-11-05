-- Migration step 2: the feed's durable per-group progress (ADR 0010).
--
-- One row per (category, group), holding the ack cursor and the
-- delivered watermark. Both default to zero - nothing acknowledged,
-- nothing delivered. A group reading two categories holds two rows,
-- each legal watermark movement over the shared position space.
--
-- The constraints enforce the feed's contract rather than merely
-- recording it: the watermark never sits below the cursor, and the
-- cursor never goes negative.

CREATE TABLE epoch_feed_cursors (
    category TEXT NOT NULL,
    group_name TEXT NOT NULL,
    cursor INTEGER NOT NULL DEFAULT 0,
    delivered_to INTEGER NOT NULL DEFAULT 0,
    CONSTRAINT epoch_feed_cursors_scope PRIMARY KEY (category, group_name),
    CONSTRAINT epoch_feed_cursors_no_backwards_watermark
        CHECK (delivered_to >= cursor),
    CONSTRAINT epoch_feed_cursors_non_negative
        CHECK (cursor >= 0)
);
