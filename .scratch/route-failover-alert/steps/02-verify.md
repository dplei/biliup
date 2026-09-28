# 02 · 验证

Status: ready-for-human

来源：[issue #92](https://github.com/dplei/biliup/issues/92)，设计见 [spec](../spec.md)。

## 本地

- `route_health` 单测覆盖：切到备用线路不告警、全部熔断告警一次、半开探测与 `Unavailable`
  不重复、恢复后下一轮重新告警、单候选直接告警、切线关闭不告警。
- dev 实跑不做：全部线路熔断需要 CDN 连续失败，无法人为制造；告警只改了发送时机，
  录制路径没有变化。

## 生产验收（贴到 issue 的可判定清单）

1. 出现 `circuit_opened=true` 且紧接着 `selected a different healthy stream route` 的场次，
   **不再**收到拉流告警。
2. 出现 `all routes cooling down; probing one while live`（`half_open=true`）或
   `all refreshed stream routes are cooling down` 的场次，收到一次「⚠️ 直播拉流所有线路均失败」，
   同一轮故障内不重复。
3. 旧文案「⚠️ 直播拉流线路故障，正在自动切换」在新版本上线后不再出现。
