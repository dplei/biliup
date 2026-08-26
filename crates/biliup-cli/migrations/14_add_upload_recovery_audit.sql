-- Late validated events and finalized-session rescans must remain auditable without creating a
-- new active lifecycle row or a replacement upload session.
create table if not exists upload_recovery_audit
(
    id INTEGER not null primary key,
    live_streamer_id INTEGER not null,
    streamer_info_id INTEGER not null,
    file_path TEXT not null,
    reason TEXT not null,
    created_at DATETIME not null
);

create index if not exists ix_upload_recovery_audit_streamer
    on upload_recovery_audit (live_streamer_id, streamer_info_id, created_at);
