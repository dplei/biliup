# 拉流线路全部熔断时，在播也要能接上

来源：[issue #83](https://github.com/dplei/biliup/issues/83)。前置设计：
[short-eof-retry-failover](../../.archive/short-eof-retry-failover/spec.md)（引入 `route_health` 熔断与抖音切线）。

## 问题

`RouteHealthState` 是一个只有「关 → 开」、没有「半开」的熔断器：

1. 2 分钟窗口（`FAILURE_WINDOW`）内同一 `RouteKey` 失败 2 次 → `cooldown_until = now + 10min`。
2. `select_route` 找不到不在冷却中的候选时返回 `RouteSelection::Unavailable`，录制循环置
   `can_download = false`，只每 30 秒查一次直播间状态，**不拉流**。
3. 冷却期内只有两件事能让线路重新可用：冷却自然到期，或者该线路的一次 attempt 稳定 / 有产出
   （`record.clear()`）。而 `can_download = false` 时根本没有 attempt，后者不可能发生。
4. `RouteKey` 不含签名 query，主播恢复推流后抖音返回的仍是同一批 host，刷新候选跳不出冷却。

于是「主播跳闸、几分钟内恢复」这种最常见的短暂断流，恢复后最多要等满 10 分钟冷却才会重新拉流。

issue 里的日志链正好就是这样开的闸：上一条 FLV 连接 CDN 正常结束（`StreamEnded` 在
`is_counted_transport_failure` 里算一次失败），重连 404 再算一次 → FLV 熔断；换 HLS 连续两次
拿不到分片 → HLS 熔断 → 全部冷却。

**只影响开启了 `douyin_route_failover` 的直播间**：切线关闭时 `select_route` 直接返回
`Selected`，不看冷却。

## 待查项的结论

| issue 待查 | 结论 |
|---|---|
| 404/403 是否该计入路由故障 | **不改计数**。403 首次已走 `AuthRefresh` 不计数；404 在单个 CDN 节点失效时正是切线要处理的信号，去掉它切线就失去意义。问题不在「计多了」，而在熔断后**没有出路**；有了半开探测，多计一次的代价封顶 30 秒。 |
| 下播后在宽限期内恢复，健康状态是否重置 | **不重置**——`Ok(LiveStatus::Offline)` 分支完全不碰 `route_health`。下播过渡期的 404 被记成线路故障，恢复开播后仍被拦着。本次改为：检查结果为 Offline 时清空线路健康记录。 |
| 全部冷却且在播时是否 half-open | **是**，见方案。 |
| `retry_after` 打印截断前的值 | 半开后 `Unavailable.retry_after` 就是「距下一次探测」，天然 ≤ 30 秒，日志即实际等待。 |

## 方案

**全部冷却且直播间确认在播时，每 30 秒放行一次试探（half-open）；检查到下播则清空线路健康记录。**

### 半开探测（`route_health.rs`）

- 新常量 `HALF_OPEN_INTERVAL = 30s`（与录制循环的 `RETRY_MAX_DELAY` 对齐，即一个检查间隔）。
- `select_route` 在所有候选都冷却时：
  - 距上次探测不足 `HALF_OPEN_INTERVAL` → 照旧 `Unavailable`，`retry_after` = 距下次探测的剩余时间；
  - 否则选**冷却最早到期**的候选（同值取候选顺序靠前的）作为探测线路，标记该记录
    `half_open = true`，返回 `Selected { half_open: true, .. }`。
- 探测结果在 `observe_live_attempt` 里照常结算：
  - 稳定或有产出 → 现有的 `record.clear()`，线路恢复，后续正常选路；
  - 计数失败 → **不论窗口内累计几次，直接重新熔断**（`cooldown_until = now + ROUTE_COOLDOWN`）。
    这样被探测的线路冷却到期时间被推后，下一次探测自然轮到下一条，不会反复只探同一条。
- 风暴告警不受影响：`storm_alert_sent` 在恢复前一直为真，重新熔断不会再发告警。

### 下播清空（`route_health.rs` + `download.rs`）

- 新增 `RouteHealthState::reset_after_offline()`：清空 `routes`、`last_probe_at`，复位
  `storm_alert_sent`；**保留** `metrics` 和 `current_route_key`（前者是整场的统计，后者让恢复后
  优先回到原线路）。
- 录制循环 `Ok(LiveStatus::Offline)` 分支调用一次（幂等）。检查报错（`Err`）**不**调用——那不是下播证据。

### 可观测

- 探测时打一条 `info!`（`half_open=true`、host、protocol、quality），`route_selected` 事件用
  `("probing", "half_open_probe")`。
- `download_resilience_session_summary` 增加 `half_open_probes`。
- 「all refreshed stream routes are cooling down」日志的 `retry_after` 即实际等待时间。

用 issue 的链条套一遍：HLS 熔断后下一次选路发现全部冷却 → 立刻探测 FLV（冷却最早到期）；
主播还没恢复则 FLV 重新熔断，30 秒后探测 HLS，如此轮转。主播恢复推流后，最迟一个检查间隔
（外加一次探测失败的轮转）就能接上。若接口先报了 Offline 再恢复，健康记录已被清空，恢复后第一次
选路就正常拉流。

## 为什么不用别的办法

| 办法 | 不选的理由 |
|---|---|
| 缩短 `ROUTE_COOLDOWN` | 熔断的本意是真故障节点别反复打；缩短只是把漏录上限从 10 分钟变成 N 分钟，治标 |
| 404 不计入失败 | 单节点 404 时切线就失效了；见上表 |
| `StreamEnded` 不计入失败 | 短 EOF 重试 / 切线的判据依赖它（前置设计），改它牵动范围大，而半开已让它的副作用封顶 |
| 全部冷却时直接无视冷却、每轮都拉 | 等于关掉熔断；半开每 30 秒只放一次，保留熔断对真故障的保护 |

## 已知上限

- **恢复后可能先探到一条仍失败的线路**：候选有 N 条都在冷却时，最坏要轮 N 次探测（每次约
  30 秒）才轮到可用的那条。抖音候选一般只有 FLV/HLS 各一两条，通常 30–60 秒。
- **真下播过渡期内每 30 秒会多一次失败连接**：与切线关闭时的重试频率相当，不增加额外压力。
- 探测线路第一次拿到 403 走 `AuthRefresh` 时不算失败也不算恢复，下一轮仍在冷却中，会等下一个
  探测间隔。罕见，不单独处理。

## 拆解

| step | 内容 | 依赖 |
|---|---|---|
| [01](steps/01-half-open-and-offline-reset.md) | 半开探测 + 下播清空 + 日志/汇总字段 | — |
| [02](steps/02-verify.md) | 回放测试 + dev 不回归实跑 + 生产验收 | 01 |

## 不做

- 调整 `FAILURE_WINDOW` / `ROUTE_COOLDOWN` / 失败计数口径：半开之后这些只影响切线的激进程度，
  不再决定漏录时长，没有证据前不动。
- 让 B 站等其它平台也走切线：与本题无关。
