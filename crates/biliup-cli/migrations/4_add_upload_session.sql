-- 增量投稿：每场直播一行，记录 B 站稿件号与已投稿视频列表，用于崩溃/重启后续接同一稿件。
-- live_streamer_id：配置直播间(room)稳定 id，跨重启匹配用。
-- streamer_info_id：当前挂接的会话 id，重启续接时更新为新会话。
-- 行总在首段建稿成功后插入，故 aid 非空、status 取 submitted；下播收尾置 finalized。
create table if not exists upload_session
(
    id INTEGER not null
        constraint pk_upload_session
            primary key,
    live_streamer_id INTEGER not null,
    streamer_info_id INTEGER not null,
    aid INTEGER,
    bvid VARCHAR,
    videos_json TEXT not null default '[]',
    status VARCHAR not null default 'submitted',
    created_at DATETIME not null,
    updated_at DATETIME not null
);
create index if not exists ix_upload_session_room
    on upload_session (live_streamer_id, status, updated_at);
