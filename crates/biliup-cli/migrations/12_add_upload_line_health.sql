create table if not exists upload_line_health (
    line_key TEXT primary key not null,
    consecutive_failures INTEGER not null default 0,
    cooldown_until DATETIME,
    last_failure_kind TEXT,
    last_error TEXT,
    updated_at DATETIME not null
);

create index if not exists ix_upload_line_health_cooldown
    on upload_line_health (cooldown_until);
