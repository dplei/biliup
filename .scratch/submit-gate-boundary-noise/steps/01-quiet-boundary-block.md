# 01 · 门禁 quiet 判定与降级

依赖：无。状态：pending。

## 改动点

- `crates/biliup-cli/src/server/common/upload_session.rs`
  - `SubmitClaim::Blocked` 增加 `quiet: bool`。
  - `claim_complete_session`：读 `blocked_signature` 的那条 SELECT 顺带取 `submit_requested_at`；
    计算 in-flight-only 与 quiet；quiet 时 UPDATE 不带 `blocked_signature = ?`。
    `BLOCKED_RECHECK_INTERVAL` 目前在 `submission_scheduler.rs` 私有，需要挪到可共用处或
    在 `upload_session.rs` 旁边复用同一常量，不要写第二个 10 分钟字面量。
- `crates/biliup-cli/src/server/common/upload.rs` Blocked 分支：`quiet` → `info!` 且不
  `notify_alert`；否则维持原样。日志字段集合不变，方便现有指纹继续聚类。
- 11 处 `claim_complete_session` 调用/匹配点里多数是测试，按编译错误逐个补 `quiet` 字段。

## 测试

- `upload_session.rs` 测试模块：按 spec「完成标准」前两条各写一个用例，用现有的内存
  sqlite 夹具造 `upload_session` + `upload_missing_segment` 行，直接断言返回值与
  `blocked_signature` 列。
- `cargo test -p biliup-cli` 全绿。

## 回执

- PR 正文写根因为「下播边界竞态 + 首轮阻塞必告警」，附改动前后各一段日志。
- 不写 `Closes #48`；合并后按 issue-tracker 流程打 `awaiting-verification`。
