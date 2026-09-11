# 01 · 门禁 quiet 判定与降级

依赖：无。状态：代码与单测完成；dev 实录验证与 PR 待做。

## 改动点

- `crates/biliup-cli/src/server/common/upload_session.rs`
  - `SubmitClaim::Blocked` 增加 `quiet: bool`。
  - `claim_complete_session`：读 `blocked_signature` 的那条 SELECT 顺带取 `submit_requested_at`；
    计算 in-flight-only 与 quiet；quiet 时 UPDATE 不带 `blocked_signature = ?`。
    `BLOCKED_RECHECK_INTERVAL` 目前在 `submission_scheduler.rs` 私有，需要挪到可共用处或
    在 `upload_session.rs` 旁边复用同一常量，不要写第二个 10 分钟字面量。
- `crates/biliup-cli/src/server/common/upload.rs` Blocked 分支：`quiet` → `info!` 且不
  `notify_alert`；否则维持原样。日志字段集合不变，方便现有指纹继续聚类。
- `upload.rs` `stop_missing_segment_attempt`：`attempt_token` 为 None 时不再返回 `NotRunning`，
  改为 CAS `status='uploading' AND attempt_token IS NULL` → `failed`（写 `last_error`、
  `next_retry_at`、`updated_at`），成功返回 `Stopped`，`rows_affected == 0` 才返回 `NotRunning`。
  不碰 `fail_enrolled_attempt_with_outcome`（它的 WHERE 限定 v2 + token，语义不同）。
- 11 处 `claim_complete_session` 调用/匹配点里多数是测试，按编译错误逐个补 `quiet` 字段。

## 测试

- `upload_session.rs` 测试模块：按 spec「完成标准」前两条各写一个用例，用现有的内存
  sqlite 夹具造 `upload_session` + `upload_missing_segment` 行，直接断言返回值与
  `blocked_signature` 列。
- `upload.rs` 测试模块：插一条 v1 无 token `uploading` 行，`stop_missing_segment_attempt` 返回
  `Stopped` 且行为 `failed`；再调一次返回 `NotRunning{status:"failed"}`。
- `cargo test -p biliup-cli` 全绿。

## 回执

- PR 正文写根因为「下播边界竞态 + 首轮阻塞必告警」，附改动前后各一段日志。
- 不写 `Closes #48`；合并后按 issue-tracker 流程打 `awaiting-verification`。

## 回执（2026-09-11）

- `upload_session.rs`：`BLOCKED_RECHECK_INTERVAL` 迁入并 `pub`；`SessionCompleteness::is_in_flight_only`；
  `claim_complete_session` 读 `submit_requested_at` 判 quiet，quiet 时 UPDATE 用 `CASE WHEN` 保留
  `blocked_signature`；`SubmitClaim::Blocked` 新增 `quiet`。
- `upload.rs`：Blocked 分支 quiet → `info!` 且不 `notify_alert`，日志多一个 `quiet` 字段，消息文本不变；
  `stop_missing_segment_attempt` 无 token 行 CAS 到 `failed`，退避与 v2 释放一致（10 分钟）。
- `submission_scheduler.rs`：改 import 常量。
- 单测 4 个新增（quiet→loud 边界、actionable 永不 quiet、结构性 reason/无 intent 不 quiet、
  无 token 行可强停），`cargo test -p biliup-cli` 390 passed。
- 未做：dev 环境实录一场验证 INFO 日志与无 webhook。
