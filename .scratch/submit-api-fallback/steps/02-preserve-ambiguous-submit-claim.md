# 02 · 不确定投稿结果保留 claim

Status: resolved

依赖：01

## 目标

持久投稿协调器只有在远端明确拒绝或本地明确失败时才释放 claim 自动重试；请求可能已经被远端
接受但结果无法确认时，持久化为人工核对状态并停止自动重投。

## 改动

- `crates/biliup-cli/src/server/common/upload.rs`
  - `reconcile_session_submission` 按 step 01 的类型化结果分流：成功沿用现有 finalize；明确拒绝调用
    普通 `retry_submission`；本地前置失败按未发远端请求处理；不确定结果调用
    `mark_submit_anomaly(..., release_claim=false)` 并返回 `ManualInspectionRequired`。
  - 不确定结果使用稳定状态名 `unknown_remote_result`，诊断只保存脱敏后的接口和错误类别。
  - 删除 `SUBMIT_RATE_LIMIT_*`、`SUBMIT_RATE_LIMIT_MARKER`、`is_submit_rate_limited` 及专用告警分支；
    21566 最终未被 fallback 消化时使用现有 60 s 起、30 min 封顶的普通退避。
- `crates/biliup-cli/src/server/common/upload_session.rs`
  - 更新 `mark_submit_anomaly` 注释/状态约束；保持“不释放 claim、`next_submit_at = NULL`”的已有事务。
  - `SessionSubmitReadiness::Claimed` 把 `unknown_remote_result` 与 `ok_no_aid` 一样映射为
    `ManualInspectionRequired`，不能降级成普通 `ClaimedElsewhere`。
- `crates/biliup-cli/src/server/common/submission_scheduler.rs`
  - 防御性排除无 claim 的 `unknown_remote_result`，避免损坏或人工改动后的行被扫描器重新投稿。
- `crates/biliup-cli/src/server/api/endpoints.rs`
  - 待投稿页面把该状态显示为“远端结果不确定，系统不会自动重投”；不提供危险重试入口。

不新增恢复按钮或自动查询创作中心。解除不确定 claim 属于需要远端核对后的独立人工操作，不在本任务
里猜测稿件是否已经创建。

## 测试

- App 在发出请求后超时：不调用 Web，`submit_state = unknown_remote_result`，claim 保留，
  `next_submit_at IS NULL`，结果为 `ManualInspectionRequired`。
- App 21566 后 Web 超时：不调用其他接口，同样保留 claim。
- App 其他明确非零码：claim 释放，进入普通退避；21566 显式 App 失败也不再使用小时级退避。
- 重启/周期扫描不会选中 `unknown_remote_result`；即使 claim 被人工清空也由状态防御性排除。
- API 将有 claim 和无 claim 的 `unknown_remote_result` 都显示为人工核对，不显示立即投稿/恢复入口。
- 现有 `ok_no_aid`、写回失败、普通 retry、claim 竞争与 finalized 测试保持通过。

## 验收

- `cargo test -p biliup-cli submit --lib`
- `cargo test -p biliup-cli submission_scheduler --lib`
- `cargo test -p biliup-cli pending_submit --lib`
- `cargo test -p biliup-cli --lib`
- `rustfmt --edition 2024 --check crates/biliup-cli/src/server/common/upload.rs \
  crates/biliup-cli/src/server/common/upload_session.rs \
  crates/biliup-cli/src/server/common/submission_scheduler.rs \
  crates/biliup-cli/src/server/api/endpoints.rs`
- `python3 scripts/check_code_index.py`

## 回执

- 传输或响应解析不确定时写 `submit_state=unknown_remote_result`、保留 claim、清空自动重试时间，
  返回 `ManualInspectionRequired`；明确拒绝与本地失败仍走可重试路径。
- 周期扫描器、空会话清理、待投稿视图和整场恢复入口都把 `unknown_remote_result` 当成人工核对状态；
  即使 claim 被异常清空也不会自动投稿。
- 删除 21566 字符串匹配、15 min～4 h 专用退避与账号冷却告警；最终明确拒绝统一使用 60 s 起、
  30 min 封顶的普通投稿退避。
- 验证：`submission_scheduler` 3 passed；`pending_submit` 4 passed；`biliup-cli --lib` 405 passed、
  9 ignored；`python3 scripts/check_code_index.py` 通过（119 files，63 relationships）；
  `git diff --check` 通过。

## Comments

- 全仓 `cargo fmt --all -- --check` 当前会报告与本任务无关的既有差异；本 step 只检查触达文件。
- 2026-09-21：实现与全库回归完成；真实 21566 → Web 成功链留待合并后的生产验收。
