create table if not exists recording_lease
(
    id INTEGER not null primary key,
    live_streamer_id INTEGER not null
        references livestreamers(id) on delete cascade,
    expires_at DATETIME not null,
    customer_note TEXT not null,
    state TEXT not null
        check (state in ('scheduled', 'grace_current_session', 'expired_paused', 'superseded', 'cancelled')),
    grace_streamer_info_id INTEGER,
    grace_live_session_key TEXT,
    pause_owned_by_lease INTEGER not null default 0,
    effective_paused_at DATETIME,
    notification_status TEXT not null default 'not_ready'
        check (notification_status in ('not_ready', 'pending', 'sending', 'failed', 'sent', 'not_configured')),
    notification_claim_token TEXT,
    notification_claimed_at DATETIME,
    notification_attempts INTEGER not null default 0,
    next_notification_at DATETIME,
    last_notification_error TEXT,
    notified_at DATETIME,
    created_at DATETIME not null,
    updated_at DATETIME not null
);

create unique index if not exists ux_recording_lease_active_streamer
    on recording_lease (live_streamer_id)
    where state in ('scheduled', 'grace_current_session', 'expired_paused');

create index if not exists ix_recording_lease_due
    on recording_lease (state, expires_at);

create index if not exists ix_recording_lease_notification
    on recording_lease (notification_status, next_notification_at);
