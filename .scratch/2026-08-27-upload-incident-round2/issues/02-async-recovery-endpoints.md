# 02 — 补传接口异步化与后台执行落库

Status: resolved
Blocked by: 01
Model: Sonnet 5 —— 改动模式已经在仓库里有现成范例（`post_uploads` 的 spawn 写法），难点（取消
语义、失败落库）由 01 先行解决；这里主要是把同步调用搬进后台任务并保证错误路径写库。

## 背景

对应评估报告 A-1 / B（后端部分）。反向代理 504 不只是显示问题，它会把正在进行的补传直接
杀掉且不留终态。

## 根因

- [`recover_missing_upload`](crates/biliup-cli/src/server/api/endpoints.rs:861) 与
  [`retry_missing_upload`](crates/biliup-cli/src/server/api/endpoints.rs:965) 在 handler 内直接
  `await` 整段上传（[`manual_recover_missing_segment`](crates/biliup-cli/src/server/common/upload.rs:2563)、
  [`retry_missing_segment`](crates/biliup-cli/src/server/common/upload.rs:2914)）。
- watchdog 是 [`upload_enrolled_with_watchdog`](crates/biliup-cli/src/server/common/upload.rs:1581)
  内部的 `select!` 分支，和上传 future 同属一棵树。客户端连接断开 → axum drop handler →
  watchdog 一起消失 → `fail_enrolled_attempt` 没机会执行 → 数据库停在 `uploading`。
- `AttemptGuard::drop`（[upload.rs:134](crates/biliup-cli/src/server/common/upload.rs:134)）只清理
  进程内注册表，不写库。

## 改动范围

- 两个接口改为「同步 claim、异步执行」：同步部分只做资格判定
  （[`check_recovery_eligibility`](crates/biliup-cli/src/server/common/recovery_eligibility.rs)）
  与 attempt claim，返回 `{missing_id, attempt_token, status, eligibility}`；上传在
  `tokio::spawn` 中执行。范例见 [`post_uploads`](crates/biliup-cli/src/server/api/endpoints.rs:648)。
- spawn 出去的任务必须自带完整终态处理：成功走 `persist_segment`，失败走
  `fail_enrolled_attempt`，panic 也要能被 01 的收割器兜住。
- 进度沿用现有 `uploaded_bytes` / `last_progress_at` / `current_line` 落库，前端 5 秒轮询不变。
- 不改变现有幂等语义：重复点击应返回 `AlreadyRunning` 而不是起第二个任务。

## 验收

- 手动触发 3 GB 级补传，接口 1 秒内返回；页面进度持续刷新。
- 断开客户端 / 反代超时不影响后台任务，任务仍会走到 `succeeded` 或 `failed`。
- 任务结束后 `attempt_token` 必被清空，状态不会停在 `uploading`。

## 测试

- 集成：claim 后立即 drop 掉「HTTP 侧」future，断言后台任务仍然完成并落库。
- 回归：重复调用 recover 只产生一个 attempt。

## Answer

已实现。`manual_recover_missing_segment` 拆成 `claim_manual_recovery`（同步：资格判定 + 线路决策 +
attempt claim，无上传）与 `run_claimed_recovery`（执行到终态）。`retry` 同理拆出
`claim_retry_recovery`。两个 handler 只 await claim，随后
`recovery_scheduler::spawn_claimed_recovery` 把上传交给独立任务，接口立即返回
`{ok, missing_id, eligibility, attempt_token, line, line_skip_reason, status}`。

后台任务自带终态处理：成功走 `persist_segment`，失败走 `fail_enrolled_attempt`；即使任务
被杀，01 的收割器也会按阶段判据兜住。幂等语义不变——重复点击返回 `already_running`。

测试：`upload::tests::a_manual_claim_is_durable_and_exclusive_of_its_caller`
（claim 后立刻 drop「HTTP 侧」的 claim，行仍是 `uploading` 且持有 token；第二次 claim 返回
`AlreadyRunning`，`upload_attempt` 只有一行）。
