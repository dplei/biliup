create table if not exists upload_rate_gate
(
    id INTEGER not null primary key check (id = 1),
    last_601_at DATETIME,
    cooldown_until DATETIME,
    strikes INTEGER not null default 0,
    updated_at DATETIME not null
);
