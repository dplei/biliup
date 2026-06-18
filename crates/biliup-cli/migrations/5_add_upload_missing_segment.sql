-- Durable queue for recorded segments whose upload failed or whose part still needs to be patched into an existing archive.
create table if not exists upload_missing_segment
(
    id INTEGER not null
        constraint pk_upload_missing_segment
            primary key,
    live_streamer_id INTEGER not null,
    streamer_info_id INTEGER not null,
    upload_session_id INTEGER,
    aid INTEGER,
    file_path VARCHAR not null,
    danmaku_file_path VARCHAR,
    segment_order INTEGER not null,
    status VARCHAR not null default 'pending',
    attempts INTEGER not null default 0,
    line_index INTEGER not null default 0,
    next_retry_at DATETIME not null,
    last_error TEXT,
    created_at DATETIME not null,
    updated_at DATETIME not null,
    constraint fk_upload_missing_segment_streamer_info_id_streamerinfo
        foreign key (streamer_info_id) references streamerinfo (id)
        on delete cascade,
    constraint fk_upload_missing_segment_upload_session_id_upload_session
        foreign key (upload_session_id) references upload_session (id)
        on delete set null
);

create unique index if not exists ux_upload_missing_segment_active_file
    on upload_missing_segment (live_streamer_id, file_path)
    where status in ('pending', 'uploading', 'failed');

create index if not exists ix_upload_missing_segment_due
    on upload_missing_segment (status, next_retry_at, updated_at);

create index if not exists ix_upload_missing_segment_session_order
    on upload_missing_segment (upload_session_id, segment_order);
