# 01 关闭窗口 + 21566 独立退避 + 退避基数对齐

Status: ready-for-agent

## 改动

- `download.rs` `persist_closed_session_intents`：新增 `hold_secs` 参数；`newly_requested` 且
  `hold_secs > 0` 时调用 `upload_session::hold_session_submit` 写 `next_submit_at = now + hold`。
- `upload_session.rs`：新增 `hold_session_submit`，只在 `next_submit_at IS NULL AND
  submit_claim_token IS NULL AND status != 'finalized'` 时写。
- `upload.rs`：`SUBMIT_RETRY_BASE_SECS` 60；新增 `SUBMIT_RATE_LIMIT_BASE_SECS`/`MAX_SECS`；
  `submit_retry_at` 加 `rate_limited` 参数；`retry_submission` 同一条 SELECT 读
  `last_submit_error`，连续频控不再 `notify_alert`。
- `endpoints.rs` `pending_submit_action`：`submit_state != 'failed'` 时的等待文案。

## 验证

- 单元测试：频控退避首个 ≥ 15 min；连续频控第二次不告警（用 `last_submit_error` 判定）；
  关闭窗口内 `reconcile_session_submission` 返回 `NotDue`，窗口过后进入门禁。
- `cargo test -p biliup-cli`。

## Comments
