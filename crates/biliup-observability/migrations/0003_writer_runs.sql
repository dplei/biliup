CREATE TABLE log_writer_run (
    process_run_id TEXT PRIMARY KEY,
    instance_id TEXT NOT NULL,
    started_at_ms INTEGER NOT NULL,
    heartbeat_at_ms INTEGER NOT NULL CHECK(heartbeat_at_ms >= started_at_ms),
    closed_at_ms INTEGER CHECK(closed_at_ms IS NULL OR closed_at_ms >= started_at_ms),
    stale_detected_at_ms INTEGER
);
CREATE INDEX log_writer_run_state ON log_writer_run(closed_at_ms, heartbeat_at_ms);

-- The old singleton owner can only preserve one conservative unknown window during migration.
UPDATE log_meta
SET unclean_shutdowns = unclean_shutdowns + dirty,
    dirty = 0
WHERE singleton = 1;
