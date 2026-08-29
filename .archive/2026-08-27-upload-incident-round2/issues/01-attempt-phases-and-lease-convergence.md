# 01 — attempt 阶段化与租约收割统一

Status: resolved
Model: Opus 5 —— 并发状态机 + 跨进程语义 + migration，判断错一处就会重新制造「双开上传」或「假死 uploading」；需要同时在脑子里维持进程内注册表、数据库 CAS 和收割循环三方时序。

## 背景

对应评估报告 A-2 / A-4。`3308d6d` 让进程内 watchdog 推迟到 transfer 才启动，但数据库侧的收割器
没有跟着改，形成了一个比原问题更隐蔽的自激循环。这是本轮所有问题里唯一会**静默作废已完成
上传**的缺陷，必须最先修。

## 根因

1. [`claim_enrolled_attempt`](crates/biliup-cli/src/server/common/upload.rs:809) 在预处理开始**之前**
   就把 `last_progress_at` 置为 claim 时刻。此后依次是音量标准化 → 时间戳修复 → 等待全局上传
   permit（[upload.rs:1317](crates/biliup-cli/src/server/common/upload.rs:1317)，容量 1）→ pre_upload
   → 才发出 `TransferStarted`。
2. [`recover_stale_upload_attempts`](crates/biliup-cli/src/server/common/missing_segment.rs:100)
   每 60 秒把 `COALESCE(last_progress_at, upload_started_at, updated_at) <= now-5min` 的 `uploading`
   行改成 `failed`、`line_index+1`、`attempt_token=NULL`、`next_retry_at=now`。
3. 收割器**不查进程内注册表**、**不触发 `CancellationToken`**
   （[`cancel_registered_attempt`](crates/biliup-cli/src/server/common/upload.rs:1745) 与它完全脱钩）。

后果链：3.32 GB 分段标准化 >5 分钟 → 被收割 → 进程内那次上传变成幽灵 attempt，跑完后
`persist_segment`（[upload.rs:911](crates/biliup-cli/src/server/common/upload.rs:911)）token CAS 失败，
整段白传 → 同时该行立刻到期被另一路径领走 → 新 attempt 卡在 permit 等待，`TransferStarted`
发不出，watchdog 保持 paused → 5 分钟后再次被收割，循环。

附带：[`record_watchdog_failure`](crates/biliup-cli/src/server/common/upload.rs:1481) 把 watchdog
超时一律记成线路 `RequestTimeout` 并累计熔断，于是「本地预处理慢」被算成「远端线路坏」，
把 `bda2` 一路冷却到 1 小时档。

## 改动范围

- 生命周期行增加显式阶段（建议 `attempt_phase`：`preprocessing` / `queued` / `transferring`）
  或独立的 `last_heartbeat_at`；需要一张新 migration（沿用现有编号序列，不改已应用文件字节）。
- 预处理与排队阶段定期续租（心跳），使收割器能区分「在干活」和「假死」。
- 收割判据分阶段：
  - `transferring`：维持 5 分钟无网络进度；
  - `preprocessing`：按文件大小给上限（需定一个默认值，见「待确认」）；
  - `queued`：单独超时，且 `last_error` 要写清是在等 permit。
- 收割前先查 `attempt_registry`：本进程仍持有该 attempt 时走 `cancel_registered_attempt` 真正
  取消再落库；确认无人持有（跨进程遗留）才直接 CAS 置 `failed`。
- `record_watchdog_failure` 只在 `transferring` 阶段计线路失败；其余阶段的超时不碰
  `upload_line_health`。
- 全局上传 permit 的等待计入独立超时（**已决策**：保留「先 claim 再排队」，给排队加超时，
  不采用「推迟 claim」）。排队期间必须写心跳并把阶段标成 `queued`，让页面能显示「排队中，
  已等待 N 分钟」而不是无声无息。

## 验收

- 让音量标准化耗时 20 分钟：全程不得被收割成 `failed`，`attempt_token` 保持不变，最终上传的是
  标准化后的成片。
- 模拟一个分块永久无响应：在 5 分钟阈值后释放租约、状态转 `failed` 并换线重试。
- 任何时刻同一 `missing_id` 只有一个进程内 attempt 在跑；收割一个正在运行的 attempt 必须先
  取消它、等它退出，再改状态。
- 预处理阶段的超时不得增加任何线路的 `consecutive_failures`。

## 测试

- 单元：三种阶段各自的收割判定；收割与进程内注册表联动（持有 / 不持有两种分支）。
- 集成：扩充 `crates/biliup-cli/tests/upload_reliability_incident.rs`，加一条「长预处理不被收割」
  和一条「收割正在运行的 attempt 会先取消」的回归。

## 已决策的参数（主人 2026-08-27 定）

取「保守 + 可观测」路线：宁可多等，不可误杀；每个阶段都要能从页面看出它在干什么。

**预处理阶段：按文件大小折算的硬上限 + 输出文件心跳，两者并存。**

- 心跳（主判据）：定期观察标准化/修复的输出文件（`.audio-normalized-*.part.flv` 等）字节增长，
  有增长就续租。只要 ffmpeg 还在写盘，就永远不该被收割。
- 硬上限（兜底）：`10 分钟 + 每 GB 10 分钟`，按源文件大小算。事故里的 3.32 GB → 约 43 分钟。
  参照实测速率（5 分钟写入约 1.2 GB，约 4 MB/s）留了 3 倍余量，宁可宽。
  超过硬上限说明 ffmpeg 真的挂死，此时置 `failed` 并把「预处理超时」写进 `last_error`，
  不计线路失败。
- 心跳与硬上限的关系：心跳只能把租约续到硬上限为止，不能无限续。

**排队（等 permit）阶段：单独超时 2 小时。**

- 与 `TOTAL_UPLOAD_TIMEOUT` 取齐。全局 permit 容量为 1，前一段 3 GB 级分段传一小时是正常的，
  排队超时短了会把正常等待误杀。
- 超时后置 `failed`，`last_error` 必须写明是在等 permit（而不是网络无进度），不计线路失败。
- 排队期间刷新心跳，页面据此显示已等待时长。

**传输阶段：维持现有 5 分钟无网络进度 + 2 小时总时长。** 只有这个阶段的超时才计线路失败。

## Answer

已实现（migration `16_add_attempt_phase_and_history.sql`）。

- `upload_missing_segment` 新增 `attempt_phase` / `phase_started_at` / `last_heartbeat_at`。
  claim 时不再把 `last_progress_at` 置为当前时刻——那正是把「正在转码」误判成「网络停了」的根源；
  它现在只表示「远端确认了字节的时刻」，预处理与排队期间的存活由心跳表达。
- 判据抽成纯函数 `attempt_lease::classify_stale_lease`：`transferring` 维持 5 分钟无网络进度；
  `preprocessing` 按 `10 分钟 + 每 GB 10 分钟`（3.32 GB → 50 分钟）；`queued` 2 小时。
  任何阶段心跳超过 3 分钟没更新都判为「持有者进程已死」。无阶段的历史行沿用旧行为，滚动升级安全。
- 收割器改为逐行判定，且**先查进程内注册表**：本进程仍持有时走 `cancel_registered_attempt`
  真正取消并等它退出（等不到就跳过本轮，绝不重发租约），确认无人持有才直接 CAS。
- `record_watchdog_failure` 只在 `transferring` 阶段触发，预处理/排队超时不计线路失败。
- 健康接口 `missing_segment_health` 复用同一份判据，页面显示与后台行为不会再各说各话。

测试：`attempt_lease` 六条单元测试覆盖三阶段与心跳丢失；
`upload::tests::reaping_a_locally_running_attempt_cancels_it_first` 覆盖「先取消再落库」；
集成 `target_07_long_preprocessing_is_not_reaped_while_its_owner_is_alive`、
`target_07_a_lease_whose_owner_died_is_converged_in_any_phase`。
