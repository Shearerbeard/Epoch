-- Migration step 2: carry the check constraint to databases created
-- before step 1 included it. IMMUTABLE once applied (see step 1).
--
-- The guard matches the constraint's NAME and its definition, so a
-- hand-mangled database holding a same-named but different constraint
-- still gets the real one added - and the duplicate name makes that
-- path fail loudly rather than silently record the step.

DO $m$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
        WHERE conrelid = 'stream_events'::regclass
          AND conname = 'stream_events_sequence_check'
          AND pg_get_constraintdef(oid) = 'CHECK ((sequence > 0))'
    ) THEN
        ALTER TABLE stream_events
            ADD CONSTRAINT stream_events_sequence_check
            CHECK (sequence > 0);
    END IF;
END
$m$
