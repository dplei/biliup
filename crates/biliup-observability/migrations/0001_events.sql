CREATE TABLE log_meta (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    pruned_through INTEGER NOT NULL DEFAULT 0,
    event_bytes INTEGER NOT NULL DEFAULT 0,
    diagnostic_bytes INTEGER NOT NULL DEFAULT 0,
    event_count INTEGER NOT NULL DEFAULT 0,
    dirty INTEGER NOT NULL DEFAULT 0,
    unclean_shutdowns INTEGER NOT NULL DEFAULT 0
);
INSERT INTO log_meta(singleton) VALUES (1);
CREATE TABLE log_event (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    event_uid TEXT NOT NULL UNIQUE,
    occurred_at_ms INTEGER NOT NULL,
    ingested_at_ms INTEGER NOT NULL,
    instance_id TEXT NOT NULL,
    level INTEGER NOT NULL CHECK(level BETWEEN 0 AND 4),
    category TEXT NOT NULL,
    event_name TEXT NOT NULL,
    live_streamer_id TEXT, streamer_info_id TEXT, upload_session_id TEXT,
    segment_id TEXT, missing_id TEXT, download_attempt_id TEXT, upload_attempt_id TEXT, task_id TEXT,
    payload TEXT NOT NULL CHECK(length(CAST(payload AS BLOB)) <= 32768),
    byte_size INTEGER NOT NULL
);
CREATE INDEX log_event_time ON log_event(occurred_at_ms, id);
CREATE INDEX log_event_recording ON log_event(instance_id, streamer_info_id, occurred_at_ms, id);
CREATE INDEX log_event_level ON log_event(level, occurred_at_ms, id);
CREATE INDEX log_event_category ON log_event(category, occurred_at_ms, id);
CREATE INDEX log_event_segment ON log_event(instance_id, segment_id, id);
CREATE INDEX log_event_submission ON log_event(instance_id, upload_session_id, id);
CREATE INDEX log_event_task ON log_event(instance_id, task_id, id);
CREATE TABLE log_diagnostic (
    event_uid TEXT PRIMARY KEY REFERENCES log_event(event_uid) ON DELETE CASCADE,
    created_at_ms INTEGER NOT NULL,
    payload TEXT NOT NULL CHECK(length(CAST(payload AS BLOB)) <= 16384),
    byte_size INTEGER NOT NULL
);
CREATE INDEX log_diagnostic_age ON log_diagnostic(created_at_ms);
CREATE TRIGGER log_event_insert AFTER INSERT ON log_event BEGIN
    UPDATE log_meta SET event_count=event_count+1, event_bytes=event_bytes+NEW.byte_size WHERE singleton=1;
END;
CREATE TRIGGER log_event_delete AFTER DELETE ON log_event BEGIN
    UPDATE log_meta SET event_count=event_count-1, event_bytes=event_bytes-OLD.byte_size,
        pruned_through=MAX(pruned_through, OLD.id) WHERE singleton=1;
END;
CREATE TRIGGER log_diagnostic_insert AFTER INSERT ON log_diagnostic BEGIN
    UPDATE log_meta SET diagnostic_bytes=diagnostic_bytes+NEW.byte_size WHERE singleton=1;
END;
CREATE TRIGGER log_diagnostic_delete AFTER DELETE ON log_diagnostic BEGIN
    UPDATE log_meta SET diagnostic_bytes=diagnostic_bytes-OLD.byte_size WHERE singleton=1;
END;
