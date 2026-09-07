-- The cursor table's non-negativity pin (E19 pre-Gate-U review
-- round, finding C2): step 0004's only check keeps the watermark at
-- or above the cursor, but nothing forbade negative values, while
-- the decoder asserts non-negativity on every read. Applied steps
-- are immutable, so the pin lands as its own step; combined with the
-- existing check, `delivered_to >= cursor >= 0` bounds both columns.
ALTER TABLE epoch_feed_cursors
    ADD CONSTRAINT epoch_feed_cursors_non_negative
    CHECK (cursor >= 0);
