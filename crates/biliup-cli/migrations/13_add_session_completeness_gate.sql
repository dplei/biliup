-- Durable ownership and diagnostics for the strict pre-submit completeness gate.
-- The token deliberately has no automatic expiry: once the remote submit may have started,
-- silently stealing a stale claim is more dangerous than requiring operator inspection.
alter table upload_session add column submit_claim_token TEXT;
alter table upload_session add column submit_claimed_at DATETIME;
alter table upload_session add column blocked_signature TEXT;
alter table upload_session add column blocked_count INTEGER not null default 0;

create index if not exists ix_upload_session_submit_claim
    on upload_session (submit_state, submit_claimed_at)
    where status != 'finalized';
