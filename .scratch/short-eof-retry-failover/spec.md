# 短寿命上游的重试退避与抖音线路切换

来源：[dplei/biliup#39](https://github.com/dplei/biliup/issues/39)

本轮只完成代码核对、方案取舍与实施拆解，不修改业务代码。issue 中的生产标识和精确统计不在
本目录复述；这里只保留可由源码和脱敏日志结论支持的事实。

## 结论

issue 描述了两个独立缺陷，源码均可确认：

1. 一次连接即使已经产出可解析媒体，只要未持续五分钟、也未完成配置分段，仍会被当作连续失败；
   `route_failure_count` 因此只增不减，重试永久停在指数退避上限。
2. 抖音自动切线在调用端被永久关闭：`plugin.name()` 返回 `"Douyin"`，而
   `DownloadTask::execute` 用 `platform == "douyin"` 计算 `failover_enabled`。线路健康仍然记失败，
   但 `select_route(..., false)` 直接保留当前线路，所以不会切换，也不会进入“全部冷却”分支。

第二点排除了 issue 提出的 B1/B2：状态机本身的累计与 `RouteKey` 对账不是这次“零切换”的根因。
现有 `route_health` 的 14 条定向测试全部通过，其中已覆盖“两次失败后换到另一 Host”；缺口位于
状态机外的开关组装。

另一个容易误读的地方是：`douyin stream candidate` 记录的是 `observations`，而实际可供切换的列表是
`stream_candidates`；总览日志分别记录 `candidate_count` 和 `enabled_candidate_count`。不能只凭前者的
条数断言有同样多条已启用候选。

## 当前调用链

1. `DownloadTask::download` 返回 `DownloadAttempt { result, connected_for,
   completed_configured_segment, silent_for }`。
2. `DownloadTask::execute` 在复查仍为 Live 后，把前三项交给
   `RouteHealthState::observe_live_attempt`。
3. `observe_live_attempt` 仅用“五分钟连接”或“完成配置分段”定义 `stable_attempt`。短 EOF 会进入
   `is_counted_transport_failure`，更新同一 `RouteRecord` 的连续失败数和冷却时间。
4. 调用端把返回的失败数直接交给 `exponential_backoff`。`Recovered` 会显式清零；稳定但仍以
   transport failure 结束的 attempt 则先清 `RouteRecord`、再把本次失败记为 1，因此也会回到基础
   等待。短寿命但有媒体产出的 attempt 目前没有这条有效复位路径。
5. 刷新候选后调用 `select_route`。但抖音的大小写比较使 `failover_enabled` 恒为 false，函数在检查
   冷却记录前就提前返回 `Selected { changed: false }`。

## 方案

### 1. 把“有产出”与“线路稳定”分开

不要把“收到一些网络字节”直接并入 `stable_attempt`。这个变量还负责稳定尝试指标和切线成功指标；
把短连接算成稳定，会改变线路健康语义，也可能把只收到协议头的连接误判为成功。

新增独立的 `productive_attempt` 判据：本次下载至少产生一个被现有 `FileValidator` 判为
`Valid` 或 `RecoverableShort` 的分段。`HeaderOnly`、空文件、容器损坏和无媒体轨仍不算有产出。
具体做法是在 `SegmentEventProcessor` 内维护一个私有、单调递增的媒体分段计数，
`DownloadTask::download` 在一次 attempt 前后取差值；不新增解析器、字节阈值或配置项。

`RouteHealthState::observe_live_attempt` 同时接收这个布尔值，但保持两套用途：

- `stable_attempt` 仍只由五分钟阈值或完成配置分段决定，用于 `stable_attempts`、切线成功等指标；
- `stable_attempt || productive_attempt` 可以清掉此前的连续失败/冷却，再按本次终止状态重新计数。

因此“产出媒体后干净 EOF”仍如实记为一次 transport failure，但下一轮失败数始终从 1 开始，
`exponential_backoff(1)` 就是现有的两秒基础等待；真正连不上或只产出无效头部时仍按
2/4/8/16/30 秒增长，并可打开熔断。

这条改动已经覆盖 issue 建议中的“短连接退避单独收敛”，不再叠加第二套退避上限分支。

### 2. 用机器字段启用抖音切线

切线开关改用 `LiveStream.platform == "douyin"`，不要再用面向展示的 `LivePlugin::name()`。
同一函数稍后已经用 `stream.platform == "douyin"` 判断画质降级通知，这也是现成口径。

不改 `RouteKey::from_stream` / `from_candidate`。现有测试已覆盖签名 query、Host、协议、画质和
codec 的键语义；本次没有证据说明键对账错误。

### 3. 给失败与“保持原线路”补原生事件

legacy warn 的自定义字段会被 allowlist 全部拒收。沿用 #38 的做法，保留旧日志，同时从
`observe.rs` 发原生事件：

- `recording.route_health_changed`：`outcome=failed`，`reason_code` 区分
  `transport_failure` / `circuit_opened`，`count` 保存连续失败数；
- `recording.route_selected`：仅在上一 attempt 判失败时发，`outcome` 使用
  `fallback` / `skipped` / `waiting`，`reason_code` 区分 `route_changed`、
  `failover_disabled`、`no_candidate`、`current_route_retained`、`all_routes_cooling`。

两者携带现有录制身份、`platform` 和 `host`。`circuit_opened` 不新增布尔字段，直接编码进稳定
`reason_code`；失败数复用现有 `count`。allowlist 只需新增一个通用的 `host` 文本字段，不为
protocol/quality/codec 一次性扩张契约。

## 实施拆解

| step | 内容 | 依赖 |
| --- | --- | --- |
| [01](steps/01-reset-failure-streak-after-media.md) | 用现有媒体校验结果识别 productive attempt，保持短 EOF 退避在基础值 | — |
| [02](steps/02-enable-douyin-route-selection.md) | 修正抖音平台判据，让既有线路状态机真正接管候选选择 | — |
| [03](steps/03-emit-route-decision-events.md) | 为线路失败和保留/切换决策发原生结构化事件 | 01、02 |

01 与 02 都可单独提交、独立验证；03 等最终决策语义稳定后再补观测。

## 验收边界

- 状态机单测重复二十轮“产出可解析媒体后 `StreamEnded`”，每轮连续失败数均为 1、不开熔断，
  对应退避始终为基础值。
- 无媒体进展的重复失败仍在第二轮打开熔断，现有切线、全部冷却、认证刷新测试继续通过。
- 抖音机器平台值 + 两个配置开关开启时，第二次无进展失败能选择不同健康候选。
- 原生线路事件的 `fields.quality.rejected == 0`，并能读到 circuit 原因、Host、失败数和选择结论。
- `cargo test -p biliup-cli --lib` 与 `cargo test -p biliup-observability` 通过。

不安排等待真实上游复现作为合入前置。二十轮短 EOF 的核心退避性质由确定性单测覆盖；真实环境只
用于合并后的观察，不把不可控的 CDN 行为塞进测试套件。

## 不做

- 不新增可调的“有效字节阈值”：现有媒体校验已经比裸字节数更准确。
- 不新增第二套 retry counter 或 backoff 算法：复用当前 `route_failure_count` 即可闭环。
- 不修改 `ROUTE_STABLE_THRESHOLD`：五分钟仍是线路健康与切线成功的稳定标准。
- 不先修 `RouteKey`：这次零切换已有更早、更确定的开关根因。
- 不在本任务处理短分段的保留/合并策略；那是独立的数据保全问题。
