-- Resumable journal for the legacy upload lifecycle backfill. The source upload/session rows stay
-- authoritative; this table only records checkpoints and structured diagnostics so an interrupted
-- run can continue without replaying already committed sessions.
create table if not exists upload_lifecycle_backfill
(
    name TEXT not null primary key,
    state TEXT not null default 'pending',
    last_session_id INTEGER not null default 0,
    processed_sessions INTEGER not null default 0,
    migrated_rows INTEGER not null default 0,
    synthetic_rows INTEGER not null default 0,
    conflict_rows INTEGER not null default 0,
    started_at DATETIME,
    updated_at DATETIME not null,
    completed_at DATETIME
);

create table if not exists upload_lifecycle_backfill_event
(
    id INTEGER not null primary key,
    backfill_name TEXT not null,
    upload_session_id INTEGER,
    missing_segment_id INTEGER,
    kind TEXT not null,
    detail TEXT not null,
    created_at DATETIME not null,
    constraint fk_upload_lifecycle_backfill_event_name
        foreign key (backfill_name) references upload_lifecycle_backfill (name)
        on delete cascade
);

create index if not exists ix_upload_lifecycle_backfill_event_session
    on upload_lifecycle_backfill_event (backfill_name, upload_session_id, id);
