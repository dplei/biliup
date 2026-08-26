-- v2 turns upload_missing_segment from a transient failure queue into the permanent
-- lifecycle ledger for every validated media segment. Legacy rows remain nullable and
-- unconstrained until the incident backfill migration can classify them safely.
alter table upload_missing_segment add column normalized_file_path TEXT;
alter table upload_missing_segment add column lifecycle_version INTEGER not null default 1;
alter table upload_missing_segment add column video_json TEXT;
alter table upload_missing_segment add column total_bytes INTEGER;
alter table upload_missing_segment add column uploaded_bytes INTEGER not null default 0;
alter table upload_missing_segment add column current_line TEXT;
alter table upload_missing_segment add column upload_started_at DATETIME;
alter table upload_missing_segment add column last_progress_at DATETIME;
alter table upload_missing_segment add column attempt_token TEXT;

create unique index if not exists ux_upload_segment_v2_normalized_path
    on upload_missing_segment (live_streamer_id, normalized_file_path)
    where lifecycle_version = 2 and normalized_file_path is not null;

create unique index if not exists ux_upload_segment_v2_session_order
    on upload_missing_segment (upload_session_id, segment_order)
    where lifecycle_version = 2 and upload_session_id is not null;

create index if not exists ix_upload_segment_v2_active
    on upload_missing_segment (upload_session_id, status, segment_order)
    where lifecycle_version = 2;

create index if not exists ix_upload_segment_v2_watchdog
    on upload_missing_segment (status, last_progress_at, upload_started_at)
    where lifecycle_version = 2 and status = 'uploading';
