-- Submission attempts count actual remote requests. Preparation failures still need durable
-- exponential backoff, so keep their retry progression in a separate counter.
alter table upload_session add column submit_retry_attempts INTEGER not null default 0;
