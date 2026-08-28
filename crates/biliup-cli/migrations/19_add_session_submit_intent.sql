-- Durable liveness intent for one-shot session submission.
--
-- submit_requested_at is the desired end state (this broadcast is closed and must eventually be
-- submitted), submit_state is only the latest gate/submit result, submit_claim_token owns the
-- remote side effect, and next_submit_at throttles retries after a definite local/remote failure.
alter table upload_session add column submit_requested_at DATETIME;
alter table upload_session add column next_submit_at DATETIME;

-- blocked_missing_segments can only have been written by an actual submit gate check, so it is
-- safe evidence that submission was requested. NULL submit_state is deliberately not inferred:
-- old rows in that state may still belong to a live broadcast.
update upload_session
set submit_requested_at = coalesce(last_submit_at, updated_at, created_at, CURRENT_TIMESTAMP)
where status != 'finalized'
  and submit_state = 'blocked_missing_segments'
  and submit_requested_at is null;

create index if not exists ix_upload_session_submit_coordination
    on upload_session (next_submit_at, submit_requested_at, id)
    where status != 'finalized'
      and submit_requested_at is not null
      and submit_claim_token is null;
