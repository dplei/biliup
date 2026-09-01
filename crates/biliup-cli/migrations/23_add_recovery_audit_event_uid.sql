-- The business audit row remains authoritative.  This stable UID only makes its projection into
-- the independently-retained event database idempotent across retries and process restarts.
alter table upload_recovery_audit add column event_uid TEXT;

create unique index if not exists ux_upload_recovery_audit_event_uid
    on upload_recovery_audit (event_uid)
    where event_uid is not null;
