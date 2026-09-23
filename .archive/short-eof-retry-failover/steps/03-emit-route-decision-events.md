# 03 · 发出可查询的线路失败与选择事件

Status: resolved

Blocked by: 01, 02

## 目标

不依赖 legacy warn 的被拒字段，也能从结构化事件区分：失败是否开熔断，以及刷新候选后为什么
切换、保留当前线路或等待冷却。

## 改动

- `crates/biliup-cli/src/observe.rs`
  - 增加 `recording.route_health_changed` emitter：失败数写入 `count`，熔断状态编码进
    `reason_code`，携带录制身份、`platform`、`host`。
  - 增加 `recording.route_selected` emitter：只在上一 attempt 判为线路失败时发，使用既有
    `outcome` 和稳定 `reason_code` 表达选择结果。
- `crates/biliup-cli/src/server/common/download.rs`
  - 保留现有 legacy 日志。
  - 在 `HealthUpdate::Failure` 和随后的 `RouteSelection` 边界调用 emitter；保留当前线路也必须有
    事件，正常无故障循环不发，避免噪声。
  - `failover_disabled`、`no_candidate` 可由调用端已有值直接判定；不要扩张 `RouteSelection` 枚举。
- `crates/biliup-observability/src/model.rs`
  - allowlist 只增加 `host` 文本字段；失败数复用 `count`，熔断复用 `reason_code`。

## 稳定 reason

- 健康：`transport_failure`、`circuit_opened`
- 选择：`route_changed`、`failover_disabled`、`no_candidate`、`current_route_retained`、
  `all_routes_cooling`

## 测试

- 原生失败事件：断言 `capture_kind=Native`、`reason_code=circuit_opened`、`count` 与 `host` 存在，
  `fields.quality.rejected == 0`。
- 选择事件至少覆盖 `route_changed` 和 `current_route_retained`；正常无失败的一轮不发选择事件。
- `cargo test -p biliup-observability` 保证新增 `host` 不破坏字段约束。

## 验收

- `cargo test -p biliup-cli --lib`
- `cargo test -p biliup-observability`

## 回执

- `observe.rs` 新增 `route_health_changed`（WARN、`outcome=failed`，熔断编码进 `reason_code`，
  连续失败数走 `count`）与 `route_selected`（INFO，稳定 `outcome`/`reason_code`），两者都携带
  录制身份、`platform`、`host`。
- `model.rs` allowlist 只新增 `host` 文本字段；未为 protocol/quality/codec 扩张契约。
- `download.rs` 保留全部 legacy 日志；`HealthUpdate::Failure` 记下失败标记与 Host 后发健康事件，
  选择结论由纯函数 `route_selection_event(previous_attempt_failed, changed, failover_enabled,
  has_candidates)` 映射为稳定二元组，未扩张 `RouteSelection` 枚举；上一轮未判失败时返回 `None`，
  正常循环不发选择事件。
- 新增 `route_event_tests`：熔断事件断言 `capture_kind=Native`、`reason_code=circuit_opened`、
  `count`、`host` 与 `fields.quality.rejected == 0`；另覆盖 `transport_failure`、`route_changed`
  事件字段，以及五种选择 reason 与「无失败不发事件」。
- `cargo test -p biliup-cli --lib`：通过（373 passed，8 ignored）。
- `cargo test -p biliup-observability`：通过（18 passed）。

## Comments
