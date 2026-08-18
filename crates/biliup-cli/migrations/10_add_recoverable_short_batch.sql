create table if not exists recoverable_short_batch
(
    id INTEGER not null primary key,
    recovery_batch_id VARCHAR not null unique,
    live_streamer_id INTEGER not null,
    streamer_info_id INTEGER not null,
    state VARCHAR not null,
    files_json TEXT not null,
    manifest_path VARCHAR not null,
    attempts INTEGER not null default 0,
    next_retry_at DATETIME not null,
    last_error TEXT,
    created_at DATETIME not null,
    updated_at DATETIME not null,
    constraint fk_recoverable_short_batch_streamer_info_id_streamerinfo
        foreign key (streamer_info_id) references streamerinfo (id)
        on delete cascade
);

create index if not exists ix_recoverable_short_batch_due
    on recoverable_short_batch (state, next_retry_at, updated_at);
