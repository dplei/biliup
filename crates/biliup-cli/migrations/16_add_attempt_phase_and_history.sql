-- Attempt lease phases + per-attempt diagnostics + a queryable attempt history.
--
-- The stale-lease reaper used to judge every `uploading` row by a single "no network progress for
-- five minutes" rule, but a claimed attempt spends its first minutes in local preprocessing and
-- then in the global upload queue, where no network byte can possibly move. Recording the phase
-- and a liveness heartbeat lets the reaper apply the right deadline to each phase instead of
-- killing healthy work.
alter table upload_missing_segment add column attempt_phase TEXT;
alter table upload_missing_segment add column phase_started_at DATETIME;
alter table upload_missing_segment add column last_heartbeat_at DATETIME;
-- Why this attempt ended up on `current_line`: configured / manual / fallback / auto_probe.
alter table upload_missing_segment add column line_source TEXT;
-- Stuck-chunk diagnostics: which chunk, when it started, and the last chunk-level error.
alter table upload_missing_segment add column last_chunk_index INTEGER;
alter table upload_missing_segment add column last_chunk_started_at DATETIME;
alter table upload_missing_segment add column last_chunk_error TEXT;

create index if not exists ix_upload_segment_v2_phase
    on upload_missing_segment (attempt_phase, last_heartbeat_at, phase_started_at)
    where lifecycle_version = 2 and status = 'uploading';

-- One row per attempt, always with a terminal outcome. The lifecycle row keeps only the current
-- attempt, so line-switch history and post-mortems need their own append-only table.
create table if not exists upload_attempt
(
    id INTEGER not null primary key,
    missing_id INTEGER not null,
    attempt_token TEXT not null,
    line_key TEXT,
    line_source TEXT,
    started_at DATETIME not null,
    ended_at DATETIME,
    phase_reached TEXT,
    outcome TEXT,
    uploaded_bytes INTEGER not null default 0,
    last_chunk_index INTEGER,
    error TEXT
);

create unique index if not exists ux_upload_attempt_token
    on upload_attempt (missing_id, attempt_token);

create index if not exists ix_upload_attempt_missing
    on upload_attempt (missing_id, id desc);
