-- Queries default to native events, so the routing kind and the summary have to be columns:
-- filtering by them through the JSON payload would scan every row. Both are additive and are
-- backfilled from the payload, so a database written by the previous version stays queryable.
ALTER TABLE log_event ADD COLUMN capture_kind TEXT NOT NULL DEFAULT 'legacy_bridge';
ALTER TABLE log_event ADD COLUMN message TEXT NOT NULL DEFAULT '';

UPDATE log_event
SET capture_kind = COALESCE(json_extract(payload, '$.capture_kind'), 'legacy_bridge'),
    message = COALESCE(json_extract(payload, '$.message'), '')
WHERE json_valid(payload);

CREATE INDEX log_event_capture ON log_event(capture_kind, id);
