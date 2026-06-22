-- 投稿可观测性：在会话行上持久化每次下播一次性投稿的结果，使「投稿成功却无 aid」「写回失败」
-- 这类异常不随日志滚动丢失、可查可定位。submit_state 取值：ok_with_aid / ok_no_aid / failed；NULL=未投。
alter table upload_session add column submit_attempts INTEGER not null default 0;
alter table upload_session add column last_submit_at DATETIME;
alter table upload_session add column last_submit_error TEXT;
alter table upload_session add column submit_state TEXT;
