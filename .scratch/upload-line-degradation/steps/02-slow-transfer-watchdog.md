# 02 · Transferring 阶段的滑窗速率判据

补 issue #17 的缺口 1：让 watchdog 抓得住「爬」，不只是「断」。依赖 step 01 的 `avg_mbps` 基线。

## 判据

在 `AttemptPhase::Transferring` 期间维护一个滑动窗口：窗口起点的 `(Instant, uploaded_bytes)`，
每次 `Progress` 算一次窗口吞吐。满足**全部三条**才中止：

1. 窗口时长 ≥ `SLOW_WINDOW`（`Duration::from_secs(90)`）——短于此不判，避免抓到分片边界抖动。
2. 窗口吞吐 < `baseline / SLOW_RATIO`（与 step 01 同一个 `SLOW_RATIO = 4.0`，同一个基线来源）。
3. `uploaded_bytes < total_bytes / 2`——**只在前半段中止**。

第 3 条是本 step 的核心取舍：`Parcel::upload_with_observer` 没有断点续传（消费 `self`，`parts`
只在内存），中止 = 已传字节全部作废。传了 60% 再中止，重来的代价超过忍完剩下的 40%。
issue 建议 4 想用「复用已上传分片」消掉这个代价，那是 UPOS 协议层的工程；这里用一条不等式换掉它。

窗口滚动：每次 `Progress` 后，若窗口时长已超过 `SLOW_WINDOW` 且**未**判慢，就把窗口起点推进到
当前点（滚动而非累计），这样一次开头的卡顿不会永久压低后续窗口。

基线在 `TransferStarted` 时读一次并缓存进 `AttemptWatch`（`Option<f64>`）；读不到就是 `None`，
判据整个关闭。传输中不再查库。

## 改什么

### `upload.rs`

- `AttemptWatch` 加三个字段：`baseline_mbps: Option<f64>`、`window_started_at: Instant`、
  `window_start_bytes: u64`。
- 判据抽成**纯函数**，便于单测且不碰 `select` 循环：
  ```rust
  enum SlowVerdict { Continue, Roll, Abort }
  fn classify_transfer_rate(
      baseline_mbps: Option<f64>,
      window_elapsed: Duration,
      window_bytes: u64,
      uploaded_bytes: u64,
      total_bytes: u64,
  ) -> SlowVerdict
  ```
- `AttemptEvent::Activity(Progress)` 分支里调它。`Abort` 走**已有**的失败出口，与
  `AttemptEvent::PhaseDeadline` 同形：`record_watchdog_failure(context, RequestTimeout,
  "slow_transfer")` + `record_chunk_diagnostics` + `observe::upload_failed` + 返回 `Err`。
  不新增出口分支，把 `PhaseDeadline` 那段抽成一个 `async fn abort_attempt(kind, ...)` 复用。
- `AttemptEvent::Activity(TransferStarted)` 分支里读基线、初始化窗口。

**为什么中止走 `record_watchdog_failure`（即真失败梯度）而不是 step 01 的慢冷却**：这一次
attempt 是真的被打断了、需要重试，与「传完了但慢」不是一回事。`RequestTimeout` 的
`ordinary_cooldown` 从 1 分钟起步，重试会换线，符合预期。

### `attempt_lease.rs`

`StaleReason` 加 `SlowTransfer`，`blames_upload_line()` 返回 `true`（网络阶段的慢，归咎线路成立）。
收割循环 `classify_stale_lease` **不需要**改——那是跨进程按心跳收割的判据，本判据只在持有
attempt 的进程内生效。

## 测试

纯函数单测，不需要 DB：

- `baseline = None` → 任何输入都 `Continue`（冷启动安全）。
- 窗口 60s / 低吞吐 → `Continue`（未到窗口时长）。
- 窗口 90s / 2.3 MB/s / 基线 26 / 已传 20% → `Abort`。
- 同上但已传 60% → `Continue`（后半段保护）。
- 窗口 90s / 26 MB/s → `Roll`（推进窗口起点）。

再补一条 `upload.rs` 现有 `mod tests` 风格的事件级测试：喂 `TransferStarted` + 一串低速
`Progress`，断言循环返回 `slow_transfer` 错误。

## 验收

- `cargo test -p biliup-cli` 通过。
- 日志能看到 `watchdog=slow_transfer` 与窗口吞吐、已传比例，事后能复原判定过程。
