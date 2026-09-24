# 02 · 验证

Status: ready-for-human

来源：[issue #83](https://github.com/dplei/biliup/issues/83)，设计见 [spec](../spec.md)。

## dev 实跑（不回归）

主播断流无法人为制造，dev 只验「正常录制不被半开打扰」：本地 `biliup server` + `pnpm dev`，
一个开了 `douyin_route_failover` 的抖音直播间录 ≥ 1 个完整分段。

- 日志里没有 `half_open=true`；
- `download_resilience_session_summary` 的 `half_open_probes=0`；
- 分段正常登记、上传。

半开路径本身由 step 01 的 issue 回放单测覆盖。

## 生产验收（贴到 issue 的可判定清单）

在开了切线的直播间里，等到一次「全部冷却」或「宽限期内下播又恢复」：

1. 出现 `all refreshed stream routes are cooling down` 之后，直到本场结束或重新拉流，
   日志里的 `retry_after` 都 ≤ 30s，且每 ≤ 约 35 秒（30s + 一次检查耗时）出现一次
   `half_open=true` 探测。
2. 主播恢复推流后，某条 `half_open=true` 探测之后有新分段登记（不要只找
   `stream route recovered`：探测以正常 EOF 结束时走的是 `failures=1 circuit_opened=false`）；
   这一段的 `stream_gap` 不出现 10 分钟量级的 `total_gap_ms`。
3. 若先出现 `Stream went offline，宽限期内继续复查` 再恢复为 `Stream is still live`：恢复后的
   第一次选路**不是** `all refreshed stream routes are cooling down`。
4. 本场 summary 里 `half_open_probes` 与日志中 `half_open=true` 的条数一致。

1–2 或 3 任一场景观察到一次即可关闭。

## Comments
