# 拉流切线告警：只在切线失败时发

来源：[issue #92](https://github.com/dplei/biliup/issues/92)。前置：
[route-cooldown-resume](../route-cooldown-resume/spec.md)（半开探测）。

## 问题

`observe_live_attempt` 在线路**熔断的那一刻**置 `alert`，这时备用线路还没开始尝试。
大多数情况下切线是成功的，这条告警就成了噪音；而备用线路也全部失败时，风暴告警
已经发过了，不会再提醒。

## 方案

告警时机从「某条线路熔断」挪到「选路时发现所有候选都在冷却」，也就是切线没能恢复录制：

- `HealthUpdate::Failure` 去掉 `alert` 字段。
- `select_route` 走到 `selected_index.is_none()`（全部冷却）时，若本轮故障还没告警，
  就置位 `storm_alert_sent` 并在返回值里带上 `alert: true`。这一步既可能返回半开探测的
  `Selected { half_open: true }`，也可能返回 `Unavailable`，两个变体都带 `alert`。
- 复位条件不变：线路稳定或有产出（`observe_live_attempt` 里的 `recovered`），或下播
  （`reset_after_offline`）。
- 切线关闭或候选为空时 `select_route` 提前返回，不告警（与原先
  `alert && failover_enabled` 一致）。
- 只有一个候选时，它熔断就等于全部冷却，照样告警，因为此时根本没有备用线路。

告警文案改为「所有拉流线路均失败」，正文带上候选数，说明在播期间每 30 秒会探测一条线路。

## 不做

- **不缩短断档**：见 issue 里的核对。首次失败约 83% 在同线路重连后恢复，
  不提前切线；并行竞速代价大、收益取决于源站，不做。
- 不加「已恢复」通知：没人提出需要。
