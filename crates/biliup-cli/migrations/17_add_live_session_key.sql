-- Platform-provided live session identity. A process restart during one live stream used to
-- create a brand new `streamerinfo` row, which broke every session-continuation path and split a
-- single stream across two upload sessions. The key is redacted by construction: it carries only
-- a platform room/session identifier, never a URL, cookie or signed parameter.
alter table streamerinfo add column live_session_key TEXT;
alter table upload_session add column live_session_key TEXT;

create index if not exists ix_streamerinfo_live_session_key
    on streamerinfo (url, live_session_key);

create index if not exists ix_upload_session_live_session_key
    on upload_session (live_streamer_id, live_session_key);
