# 01 · 整机限速时忍住慢传输

Status: ready-for-agent

来源：[issue #82](https://github.com/dplei/biliup/issues/82)，设计见 [spec](../spec.md)。

## 改什么

全部在 `crates/biliup-cli/src/server/common/`。

### `upload_line_health.rs`

新增一个纯函数，放在 `active_cooldowns` 附近，用来判断是不是整机限速：

```rust
/// 另有线路正因「慢」处于冷却 → 慢的是整机出口，不是这条线路；换线无益。
pub fn machine_wide_slowness<'a>(
    current_line: &str,
    active: &'a [UploadLineHealth],
) -> Vec<&'a str>   // 返回作为证据的 line_key，空 = 无证据
```

条件：`line_key != current_line`，而且 `last_failure_kind` 是 `slow_transfer`
（`UploadFailureKind::SlowTransfer.as_str()`）或 `SLOW_THROUGHPUT`。`active` 由调用方用现有的
`active_cooldowns(pool, now)` 取，函数本身不查库，方便单测。

### `upload.rs`

- `AttemptWatch` 加一个 `slowness_tolerated: bool`，默认 `false`。`enter_phase` **不重置**它：
  一次 attempt 只进一次 Transferring，锁定语义是「本次 attempt」。
- `SlowVerdict::Abort` 分支，在调用 `fail_attempt` 之前：
  1. 查 `active_cooldowns`，调用 `machine_wide_slowness(&context.line_key, ...)`；
  2. 如果有证据：把 `slowness_tolerated` 设为 `true`，滚动窗口，打
     `warn!(missing_id, watchdog = "slow_transfer", verdict = "tolerate", reason = "machine_wide", evidence_lines = ?..., window_mbps, baseline_mbps, uploaded_bytes, total_bytes)`，
     然后**不返回**，继续传；
  3. 如果查库失败：按**无证据**处理，维持原来的中止（fail-closed 回到旧行为），并打 warn；
  4. 如果没有证据：走原来的中止路径不变，在现有的 `upload watchdog fired` 日志里补一个字段
     `verdict = "abort"`，让两种结果在日志里能对照。
- `slowness_tolerated == true` 时，`Progress` 分支直接跳过 `classify_transfer_rate`（不再查库，也不再判慢）。
- `TotalUploadTimeout` 分支：`slowness_tolerated == true` 时不中止，打一条 `info!`
  （`reason="machine_wide_throttle"`、已传与总字节），然后把 `total` 重新 reset 一个
  `TOTAL_UPLOAD_TIMEOUT` 继续传。这样一来，锁定期间真卡死只由 `NO_PROGRESS_TIMEOUT` 兜底。

`classify_transfer_rate` 的签名和语义不动，现有 5 个单测不用改。

## 测试

纯函数单测，放在 `upload_line_health.rs` 的 `mod tests` 里：

- 其它线路有 `slow_transfer` 冷却 → 返回这条线路。
- 其它线路有 `slow_throughput` 冷却 → 返回这条线路。
- 只有当前线路自己的慢冷却 → 空。
- 其它线路的冷却类型是 `request_timeout` / `probe_failure` → 空（这些不能证明「慢」是整机的）。
- 空列表 → 空。

用 issue 里的链条补一个回放测试（真 pool，照 step 04 组合测试的写法）：先
`record_failure(estx, SlowTransfer)`，再对 `tx` 取 `active_cooldowns` 并调用判据，
断言证据为 `["estx"]`；把时间推进 31 分钟，断言证据为空（冷却过期后回到「第一次」语义）。

锁定和总时长豁免在 `select` 循环里，目前没有能跑整个循环的 harness（step 02 of
upload-line-degradation 已记录这一点）。不专门为它造 harness，交给 step 02 的 dev 限速实跑验证。

## 验收

- `cargo test -p biliup-cli` 通过。
- `cargo clippy -p biliup-cli` 没有新增告警。
- 日志里能区分两种结果：`verdict="abort"` 和 `verdict="tolerate" reason="machine_wide" evidence_lines=[...]`。

## Comments
