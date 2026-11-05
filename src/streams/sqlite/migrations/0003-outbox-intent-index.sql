-- Migration step 3: the intent-key uniqueness index (ADR 0010's keyed
-- envelope seam).
--
-- Within the outbox category, an intent key may appear on exactly one
-- event, so a redelivered source event re-appending the same intent
-- is rejected by storage as a duplicate. The category and the metadata
-- key are framework-owned; existing duplicate keys in that category
-- fail index creation rather than being silently accepted. A rename
-- needs its own migration.
--
-- The indexed expression must be deterministic, or SQLite refuses the
-- index outright: `json_extract` qualifies, `random()` does not.

CREATE UNIQUE INDEX stream_events_outbox_intent
    ON stream_events (json_extract(event_metadata, '$.intent'))
    WHERE category = 'saga-outbox';
