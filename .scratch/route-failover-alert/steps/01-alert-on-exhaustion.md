# 01 · 告警挪到全部冷却时

Status: resolved

来源：[issue #92](https://github.com/dplei/biliup/issues/92)，设计见 [spec](../spec.md)。

- `route_health.rs`：`HealthUpdate::Failure` 去掉 `alert`；`RouteSelection` 两个变体加
  `alert`，在全部冷却分支中置位。
- `download.rs`：告警从 `HealthUpdate::Failure` 分支挪到选路结果处理里。
- 单测：一条线路熔断、切到备用线路时不告警；备用线路也熔断时告警一次；半开探测失败和
  `Unavailable` 不重复告警；线路恢复后下一轮再次告警。
