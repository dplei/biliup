# 01 · 全部冷却时半开探测，下播时清空线路健康

Status: ready-for-agent

来源：[issue #83](https://github.com/dplei/biliup/issues/83)，设计见 [spec](../spec.md)。

分支：`fix/issue83-<YYMMDD>-<HHMMSS>`（开工前现取时间戳）。

## 改什么

全部在 `crates/biliup-cli/src/server/common/`。

### `route_health.rs`

- `pub const HALF_OPEN_INTERVAL: Duration = Duration::from_secs(30);`
- `RouteRecord` 加 `half_open: bool`（`clear()` 走 `Default` 自然复位）。
- `RouteHealthState` 加 `last_probe_at: Option<Instant>`、`half_open_probes: u64`；
  `RouteHealthSnapshot` 加 `half_open_probes`。
- `RouteSelection::Selected` 加字段 `half_open: bool`；正常路径和切线关闭路径都填 `false`。
- `select_route`：`selected_index` 为 `None` 时（全部冷却）——
  1. `last_probe_at` 距 `now` 不足 `HALF_OPEN_INTERVAL` → `all_routes_backoffs += 1`，返回
     `Unavailable { retry_after: HALF_OPEN_INTERVAL - elapsed }`；
  2. 否则在候选里取 `cooldown_until` 最小者（`min_by_key`，同值保留靠前的——`Iterator::min_by_key`
     本就返回第一个最小值）；该记录 `half_open = true`，`last_probe_at = Some(now)`，
     `half_open_probes += 1`；之后与正常选中走同一段代码（`apply_candidate`、切线计数、
     `pending_switch`），返回 `Selected { half_open: true, .. }`。
  - 原先 `Unavailable` 里「取最早冷却到期」的计算不再需要，删掉。
- `observe_live_attempt`：计数失败分支里，`record.half_open` 为真时 `circuit_opened = true`
  （不看 `consecutive_failures`），随后 `half_open = false`。其余逻辑不动。
- 新增：

  ```rust
  /// 直播间已报下播：过渡期里记下的 404 等不是线路的错，恢复开播后从头算。
  pub fn reset_after_offline(&mut self) {
      self.routes.clear();
      self.last_probe_at = None;
      self.storm_alert_sent = false;
  }
  ```

  `metrics` 与 `current_route_key` 保留。

### `download.rs`

- `RouteSelection::Selected { ref key, changed, half_open }`：`half_open` 为真时打
  `info!(url, half_open = true, host, protocol, quality, "all stream routes cooling down; probing one while live")`。
- `route_selection_event` 加参数 `half_open: bool`，为真时直接返回
  `Some(("probing", "half_open_probe"))`（不要求上一轮失败——探测往往发生在一次 `Unavailable`
  等待之后，那一轮没有 attempt）。更新现有单测的调用。
- `Ok(LiveStatus::Offline)` 分支开头调用 `route_health.reset_after_offline()`。
- `download_resilience_session_summary` 加 `half_open_probes = health_metrics.half_open_probes`。
- `Unavailable` 的 `(false, Some(retry_after.min(RETRY_MAX_DELAY)))` 保留（现在已是 no-op，
  留作上限保护）；日志字段 `retry_after` 不改名。

## 测试（`route_health.rs` `mod tests`）

- 改写 `cooling_route_is_not_selected_and_all_open_routes_back_off`：两条都熔断后第一次选路返回
  `Selected { half_open: true }` 且是 FLV（冷却最早到期）；同一时刻再选一次返回
  `Unavailable`，`retry_after` 在 `(0, 30s]`。
- `failed_half_open_probe_reopens_and_rotates`：探测 FLV → 一次 404（离上次失败已超出
  `FAILURE_WINDOW`，按旧逻辑不会熔断）→ 断言 `circuit_opened = true`；30 秒后再选 → 探测 HLS。
- `productive_half_open_probe_recovers_route`：探测 attempt `productive_attempt = true` →
  `Recovered`；下一次选路 `half_open = false`、同一线路。
- `offline_reset_lets_current_route_resume`：全部熔断 → `reset_after_offline()` → 选路
  `Selected { half_open: false, changed: false }`。
- **issue 回放**：FLV `StreamEnded`、FLV 404 → 切 HLS → HLS 404 ×2 → 此后每 30 秒选路一次；
  断言任意相邻两次 `Selected` 的间隔 ≤ 30 秒（修改前这段是 10 分钟的 `Unavailable`）。

`download.rs` 只改 `route_selection_event` 的单测调用并补一条 `half_open` 用例。录制循环本身
没有 harness，不为此造。

## 验收

- `cargo test -p biliup-cli` 通过，`cargo clippy -p biliup-cli` 无新增告警。
- 切线关闭时行为不变（现有 `failover_rollback_keeps_the_refreshed_primary_route` 通过）。

## Comments
