# 抖音直播下载容错与线路切换分阶段实施计划

## 1. 背景

2026-08-12 的录制日志显示，小黄人在 19:54:21 至 20:16:45 之间连续发生 38 次
`error decoding response body`：

- 每次都能拿到有效的 HTTP-FLV 响应并创建文件，随后在几十秒内读取失败；
- 直播状态检查始终返回 `Live`，Cookie 与开播状态正常；
- 重新获取的签名 URL 在该故障窗口内始终落到 `pull-flv-q13.douyincdn.com`；
- 37 个 2.2–9.4 MB 的短分段因低于 20 MB 阈值被永久删除，总计约 126.9 MiB；
- 同一时段 B 站上传从约 26.8 MB/s 降到 0.11 MB/s，并出现 500；
- 类似故障还在 18:34–18:50、23:14–次日 00:04 复发，后一次同时影响多个主播、多个抖音 CDN Host，并出现上传连接超时。

因此需要同时解决两个问题：

1. **止损**：下载链路不稳定时，已经落盘且可播放的短分段不能仅因体积小而删除。
2. **恢复**：同一路线连续失败后，应在抖音真实返回的候选 URL、协议、画质及可选网络出口之间有序切换。

## 2. 已知事实与边界

### 2.1 当前实现

- `crates/biliup/src/downloader/live/douyin.rs` 只返回选中画质下的单个 `main/flv` 或
  `main/hls` URL。
- `LiveStream.raw_stream_url` 与 `DownloadConfig.url` 都是单值，无法表达候选线路。
- 下载失败后会重新执行 `check_stream` 并刷新签名 URL，但只要检查结果仍为 `Live`，下载重试计数
  就清零，所以故障期间一直以 2 秒间隔重试同一路线。
- `FileValidator` 当前只按文件大小和扩展名判断；扩展名合法不等于媒体完整，体积小也不等于不可播放。
- HTTP-FLV 文件在 `FlvFile::drop` 时统一改名并触发分段回调，回调发生时尚未携带本次连接的终止原因。

### 2.2 CDN 线路认知

- 抖音 CDN Host 是动态调度结果，不存在可写死的固定线路数量。
- 本次日志全局观察到 5 个 Host，小黄人观察到 3 个 Host，但它们不是一次请求同时返回的候选列表。
- `flv_pull_url`、`hls_pull_url_map` 通常首先是画质映射，不应误当作多 CDN 列表。
- 不允许通过字符串替换把 `q13` 改成 `q11`/`t13`；签名可能与 Host、路径绑定。
- 只允许使用抖音响应真实提供的 URL。没有备用 FLV Host 时，优先切同画质 HLS，再考虑降画质或备用网络出口。

## 3. 目标

### 3.1 功能目标

- 网络断流后保留可播放的短分段，避免已下载内容永久丢失。
- 区分“直播状态重试”和“下载线路连续失败”，避免 `Live` 检查错误地清空线路故障计数。
- 对抖音返回的候选流进行结构化建模：Host、协议、画质、Codec、优先级。
- 同一路线连续失败后自动熔断并切换候选线路。
- FLV 不稳定时可以自动切换到同画质 HLS；必要时按明确规则降画质。
- 所有切换都有结构化日志，能回答“为何切换、从哪条切到哪条、何时恢复”。
- 所有新行为可配置关闭，并能按阶段独立回滚。

### 3.2 可靠性目标

- 单次短暂断流不得结束整场会话或提前投稿。
- 同一 Host 连续失败不得形成无限 2 秒紧密重试。
- 候选线路切换不得构造未经抖音签名的 URL。
- 路线切换失败时不影响原有下播宽限期语义。
- 小分段处理不得把 0 字节、无有效媒体轨或无法解析的垃圾文件送去上传。

## 4. 非目标

- 本计划不逆向生成抖音 CDN 签名。
- 不承诺绕过整个宿主机、运营商或公网故障；线路切换只能改善单 CDN/单协议故障。
- 第一阶段不自动部署代理服务，也不内置任何第三方代理地址。
- 不在一个媒体文件内部混接不同 Codec/分辨率；切线后必须形成新分段。
- 不在本计划中重做整个上传会话模型或 B 站投稿策略。

## 5. 总体架构

```text
抖音 check_stream
  └─ 解析 StreamCandidate[]
       ├─ 同画质 FLV（主选）
       ├─ 同画质 HLS
       ├─ 次一级画质 FLV
       └─ 次一级画质 HLS

DownloadTask
  ├─ Live/offline 状态机（现有下播宽限期）
  └─ RouteHealth 状态机（新增，独立计数）
       ├─ 记录 host/protocol/quality 连续失败
       ├─ 熔断故障路线
       ├─ 选择下一个候选
       └─ 稳定运行达到阈值后恢复健康

DownloaderRuntime
  └─ 返回 DownloadOutcome
       ├─ status / error class
       ├─ connected duration / downloaded bytes
       ├─ selected candidate
       └─ finalized segments with close reason

SegmentEventProcessor
  └─ MediaValidator
       ├─ 正常分段 → 原流程上传
       ├─ 可恢复短分段 → 保留/合并/上传
       └─ 无效分段 → 删除并记录原因
```

## 6. 阶段拆分

---

## 阶段 0：补齐事实与可观测性

### 目标

不改变选流和上传行为，先确认抖音每次响应实际给出了哪些候选，并让下一次故障具备完整诊断信息。

### 主要改动

1. 在 `douyin.rs` 中增加纯解析函数，枚举选定及相邻画质下响应真实存在的：
   - `main.flv`
   - `main.hls`
   - Codec、分辨率、码率（从 `sdk_params` 解析）
   - URL Host
2. 日志只输出以下脱敏字段，不输出完整签名 URL、Cookie：
   - `candidate_count`
   - `candidate_host`
   - `protocol`
   - `quality`
   - `codec`
   - `resolution`
3. 扩充 HTTP-FLV 错误日志：
   - 完整错误链；
   - HTTP 状态、`content-encoding`、`transfer-encoding`；
   - 已接收字节数、连接存活时间、剩余缓冲；
   - 当前 Host、协议和画质。
4. 为每次连接生成 `attempt_id`，把选流、开始下载、分段完成、失败、切线串成同一条链路。

### 涉及文件

- `crates/biliup/src/downloader/live/douyin.rs`
- `crates/biliup/src/downloader/httpflv.rs`
- `crates/biliup-cli/src/server/core/downloader/stream_gears.rs`
- `crates/biliup-cli/src/server/common/download.rs`

### 测试

- 用脱敏 fixture 覆盖：只有 FLV、FLV+HLS、多画质、缺少 `sdk_params`、空 URL。
- 验证日志模型不会包含 `sign`、`expire`、Cookie 或完整 query。
- 现有抖音画质选择测试全部保持通过。

### 验收门槛

- 生产运行至少观察一场直播或一次人工 fixture 回放。
- 能明确回答一次响应内“候选数量、协议、画质、Host 是否相同”。
- 无录制行为变化，无新增敏感信息泄露。

### 回滚

仅删除新增日志与解析辅助函数，不影响现有下载流程。

---

## 阶段 1：短分段止损

### 目标

将“体积不足阈值”从直接删除条件改成待进一步判断条件，优先保住已经下载到的有效内容。

### 关键设计

引入三态验证结果：

```rust
enum MediaValidation {
    Valid,
    RecoverableShort { duration: Option<Duration> },
    Invalid { reason: InvalidMediaReason },
}
```

判断顺序：

1. 0 字节、只有容器头、无音视频 Tag/轨道：`Invalid`。
2. 扩展名或媒体探测失败：`Invalid`，但保留明确错误日志。
3. 体积低于 `filtering_threshold`，但容器可解析且含有效媒体：`RecoverableShort`。
4. 达到阈值且格式有效：`Valid`。

### 实施策略

1. 第一小步先采用“保留并进入正常分段通道”，不立即做自动拼接；验证内容不会再被删除。
2. 为避免大量短分段直接占满投稿分 P：
   - 同会话短分段先进入内存/数据库待合并集合；
   - 后续稳定分段到达或本场结束时再收敛；
   - Codec、分辨率、音频参数一致时使用 concat/remux；
   - 不一致时保留独立文件并进入缺失补传/人工处理队列。
3. 删除动作必须发生在“确认无有效媒体”之后，不能由异步任务抢先执行。
4. 记录保留、合并、删除的原因和原始文件列表。

### 生命周期调整

当前 `FlvFile::drop` 会直接完成改名和回调，无法携带终止原因。阶段 1 需要先完成以下重构：

- 增加 `SegmentCloseReason`：`TimedSplit`、`SizeSplit`、`StreamEnded`、`TransportError`、
  `Cancelled`、`Unknown`；
- 将关闭原因带入 `SegmentInfo`；
- 正常分段继续即时处理；
- 连接异常产生的尾分段在错误分类完成后再进入验证流程；
- `Drop` 只负责兜底关闭，不承担业务判定。

### 涉及文件

- `crates/biliup/src/downloader/util.rs`
- `crates/biliup/src/downloader/flv_writer.rs`
- `crates/biliup/src/downloader/hls.rs`
- `crates/biliup-cli/src/server/core/downloader.rs`
- `crates/biliup-cli/src/server/core/downloader/stream_gears.rs`
- `crates/biliup-cli/src/server/common/util.rs`
- `crates/biliup-cli/src/server/common/download.rs`
- 可能复用 `crates/biliup-cli/src/server/common/timestamp_repair.rs` 的 ffmpeg 执行抽象

### 测试

- 0 字节 FLV 被删除。
- 只有 FLV Header 的文件被删除。
- 2–10 MB 且含完整音视频 Tag 的 FLV 被保留。
- 正常 30 分钟分段行为不变。
- 连续 37 个可恢复短分段不会被静默删除。
- 合并只接受参数兼容的片段；不兼容片段保持独立且不丢失。
- 删除、上传、合并失败均保留原文件。

### 验收门槛

- 用故障 fixture 重放时，可恢复内容保留率为 100%。
- 不产生 0 字节投稿分 P。
- 原有正常分段、上传、后处理测试通过。

### 回滚

通过配置 `preserve_recoverable_short_segments=false` 恢复旧的体积过滤行为；已保留文件不自动清理。

---

## 阶段 2：拆分状态机与退避策略

### 目标

让“直播仍在线”不再清空下载线路故障历史，避免同一坏路线无限 2 秒重试。

### 状态拆分

保留现有：

- `offline_since`
- `offline_retry_count`
- 下播宽限期 `grace`

新增：

- `consecutive_transport_failures`
- `last_failure_at`
- `stable_since`
- `current_route_key`

`LiveStatus::Live` 只重置下播状态，不重置传输失败计数。传输计数仅在以下条件之一满足后清零：

- 当前连接稳定运行达到 5 分钟；
- 正常完成一个配置分段；
- 显式切到新路线后，新路线达到稳定阈值。

### 初始策略

- 首次传输失败：2 秒后刷新签名并重试。
- 同一路线连续失败：2s、4s、8s、16s，最高 30s。
- 若存在未熔断候选，优先立即切候选，不等待长退避。
- `404 + Offline` 进入下播判断；传输错误 + `Live` 进入线路健康判断。
- 手动取消不计线路失败。

### 涉及文件

- `crates/biliup-cli/src/server/common/download.rs`
- `crates/biliup-cli/src/server/core/downloader.rs`
- `crates/biliup-cli/src/server/core/downloader/stream_gears.rs`

### 测试

- 连续 `Error + Live` 不会重置传输失败计数。
- `Offline` 宽限期行为与当前版本一致。
- 稳定连接达到阈值后恢复健康。
- 手动取消不会触发切线或告警。
- 无候选时按退避重试，不形成忙循环。

### 验收门槛

- 同一路线持续失败时请求频率符合退避策略。
- 不发生一场直播被错误拆成多场投稿。
- 下播确认时间不超出既有配置语义。

### 回滚

配置 `route_health_enabled=false` 时继续使用旧重试流程。

---

## 阶段 3：候选线路模型

### 目标

让下载任务能够持有并选择抖音真实返回的多个候选，而不是单个 `raw_stream_url`。

### 数据模型

```rust
struct StreamCandidate {
    url: String,
    host: Option<String>,
    protocol: StreamProtocol,
    quality: Option<String>,
    codec: Option<String>,
    resolution: Option<String>,
    priority: u16,
}

enum StreamProtocol {
    Flv,
    Hls,
}
```

给 `LiveStream` 增加带 `#[serde(default)]` 的候选字段，并继续保留 `raw_stream_url` 作为兼容主选：

```rust
pub stream_candidates: Vec<StreamCandidate>
```

非抖音平台默认空列表，行为不变。

### 候选排序

默认顺序：

1. 请求画质或实际最高可用画质的 FLV；
2. 同画质 HLS；
3. 次一级画质 FLV；
4. 次一级画质 HLS；
5. 继续逐级降画质，直到配置允许的最低档。

若同一画质、同一协议真实返回多个不同 Host，则按响应顺序保存，不自行改写 URL。

### 配置

建议新增：

```toml
douyin_route_failover = true
douyin_protocol_fallback = true
douyin_quality_fallback = false
douyin_min_fallback_quality = "hd"
```

默认允许同画质 FLV → HLS；自动降画质默认关闭，避免用户无感知掉档。启用降画质时复用现有画质告警。

### 涉及文件

- `crates/biliup/src/downloader/live/mod.rs`
- `crates/biliup/src/downloader/live/douyin.rs`
- 所有 `LiveStream` 构造点（非抖音填空列表）
- `crates/biliup-cli/src/server/config.rs`
- `crates/biliup-cli/src/server/infrastructure/context.rs`
- `crates/biliup-cli/src/server/core/downloader.rs`
- 抖音前端配置组件

### 测试

- 候选只包含响应中真实存在的 URL。
- 完整 URL 不出现在普通 INFO 日志。
- 同画质 FLV 排在同画质 HLS 前。
- 关闭协议回退时不产生 HLS 候选。
- 关闭画质回退时不产生低画质候选。
- 非抖音平台序列化、反序列化和下载行为不变。

### 验收门槛

- 候选列表与阶段 0 观察到的响应结构一致。
- 单候选房间完全保持旧行为。
- 配置关闭时不发生协议或画质切换。

### 回滚

保留候选字段但关闭 `douyin_route_failover`，继续只使用 `raw_stream_url`。

---

## 阶段 4：线路熔断与自动切换

### 目标

当当前候选连续失败时，有序切到其他 Host、协议或允许的画质。

### 路线标识

`RouteKey = (host, protocol, quality, codec)`。

签名 query 不参与 Key，避免每次刷新 URL 都被视为全新路线。

### 建议熔断规则

- 同一 `RouteKey` 在 2 分钟内连续失败 2 次：熔断 10 分钟。
- 以下错误计入传输失败：
  - response body decode/read error；
  - connection reset/timeout；
  - incomplete frame；
  - HTTP 5xx；
  - 连接建立后短时间内反复结束。
- 401/403：先刷新直播信息和签名；刷新后仍失败再计入路线故障。
- 404：结合 `check_stream` 判定在线/离线，不直接跨过下播逻辑。
- 候选稳定运行 5 分钟或完成正常分段后，关闭对应熔断并恢复优先级。

### 切换顺序

```text
当前 FLV 失败
  → 同画质、不同 Host 的 FLV（若响应真实提供）
  → 同画质 HLS
  → 刷新一次候选列表
  → 配置允许时降一档画质并重复上述顺序
  → 无可用候选时按退避等待
```

每次切换必须新建媒体分段，禁止在同一文件中混接协议、Codec 或分辨率。

### 防抖与防振荡

- 失败路线未过冷却期不参与选路。
- 新路线至少稳定 5 分钟才允许因“原路线恢复”主动切回。
- 不主动追求切回高优先级；优先保持当前稳定路线直到下一次分段边界或连接失败。
- 每场直播限制告警频率，避免一次故障产生几十条通知。

### 涉及文件

- 新建 `crates/biliup-cli/src/server/common/route_health.rs`
- `crates/biliup-cli/src/server/common/mod.rs`
- `crates/biliup-cli/src/server/common/download.rs`
- `crates/biliup-cli/src/server/core/downloader.rs`
- `crates/biliup-cli/src/server/core/downloader/stream_gears.rs`
- `crates/biliup-cli/src/server/infrastructure/context.rs`

### 测试

- `q13/flv` 连续失败两次后切到不同 Host FLV。
- 没有不同 Host 时切同画质 HLS。
- 关闭 HLS 回退时不切协议。
- 路线冷却期间不会被再次选择。
- 所有路线熔断时进入退避，不 panic、不忙循环。
- Codec/分辨率变化会产生新分段。
- 成功稳定后清空故障计数。
- 一次故障风暴只发一条聚合告警。

### 验收门槛

- 使用可控 mock server 模拟 FLV 每 30 秒断开，系统能在阈值内切到稳定 HLS。
- 故障期间无可播放短分段被删除。
- 单候选、全候选失败、直播真实下播三条路径都可预测收敛。

### 回滚

运行时关闭 `douyin_route_failover` 即停止自动切线；已产生的分段继续按正常上传处理。

---

## 阶段 5：备用网络出口（可选）

### 目标

处理“多个 CDN 与 B 站上传同时异常”的宿主机/运营商路径故障。该阶段独立于 CDN 切线，不作为前四阶段上线前置条件。

### 配置模型

只配置用户自有出口，不内置第三方地址：

```toml
download_egresses = [
  { name = "direct", type = "direct" },
  { name = "proxy-a", type = "http", url = "http://..." },
  { name = "proxy-b", type = "socks5", url = "socks5://..." },
]
```

敏感 URL 在日志和 API 中必须脱敏。

### 策略

- CDN Host/协议候选都失败后，才切换网络出口。
- 出口健康与 CDN 路线健康分开计数。
- 直播状态 API 与视频流应允许绑定到同一出口，避免地区/IP 不一致导致签名失效。
- 默认不改变 B 站上传出口；上传链路另行评估。
- 直连恢复后不立即切回，避免网络振荡。

### 测试

- 直连超时、代理成功时自动切换。
- 代理配置错误不会泄露密码。
- 所有出口失败时回到有界退避。
- 配置为空时行为与当前版本一致。

### 验收门槛

- 在故障注入环境中能从直连切到备用出口并持续录制。
- 日志、前端和错误响应均不暴露代理凭证。

### 回滚

清空 `download_egresses` 或仅保留 `direct`。

---

## 阶段 6：灰度发布与默认值调整

### 灰度顺序

1. 只开启阶段 0 日志。
2. 开启短分段保留，但不自动切线。
3. 对单个测试主播开启同画质 FLV → HLS 切换。
4. 扩展到全部抖音主播，仍不自动降画质。
5. 根据数据决定是否允许自动降画质。
6. 有可靠自有代理后再灰度网络出口。

### 观测指标

- 每场连接失败次数；
- 每个 RouteKey 的失败率和平均稳定时长；
- 自动切线次数与成功率；
- FLV → HLS 后的持续录制时长；
- 可恢复短分段数量、总字节、最终保留/合并/上传数量；
- 因无效媒体删除的文件数量；
- 单场最终缺失时间估算；
- B 站上传同期错误率，用于区分 CDN 故障和公共网络故障。

### 默认值调整条件

- 至少经过 7 天或 20 场抖音直播灰度；
- 自动切线没有导致错误下播、重复投稿或不可播放分 P；
- 短分段保留没有造成无法控制的分 P 数量或磁盘堆积；
- HLS 回退成功率和画质符合预期。

## 7. 建议提交拆分

每个提交应可独立测试和回滚：

1. `test(douyin): add sanitized stream candidate fixtures`
2. `feat(download): add structured transport diagnostics`
3. `refactor(download): carry segment close reason`
4. `fix(download): preserve playable short segments`
5. `refactor(download): separate offline and transport retry state`
6. `feat(douyin): expose signed stream candidates`
7. `feat(download): add route health circuit breaker`
8. `feat(douyin): fail over from flv to hls`
9. `feat(douyin): add opt-in quality fallback`
10. `feat(download): add optional egress failover`
11. `docs: document douyin download resilience rollout`

禁止把短分段生命周期重构、候选模型、熔断和代理出口压在一个提交中。

## 8. 全量验证清单

### Rust

```bash
cargo fmt --all -- --check
cargo test -p biliup douyin -- --nocapture
cargo test -p biliup httpflv -- --nocapture
cargo test -p biliup-cli download -- --nocapture
cargo test -p biliup-cli route_health -- --nocapture
cargo check -p biliup-cli
```

### 故障注入场景

- FLV 建连后 30 秒返回损坏 chunk；
- FLV 建连后超时；
- FLV 500、HLS 正常；
- 同 Host FLV/HLS 都失败，备用 Host 正常；
- 所有候选失败但直播状态仍为 Live；
- 故障期间主播真实下播；
- 切线时 Codec/分辨率发生变化；
- 连续生成几十个 2–10 MB 有效短分段；
- 生成 0 字节、只有 Header、损坏 FLV；
- 下载与上传同时网络异常。

### 回归场景

- 非抖音平台下载不受候选模型影响；
- 正常抖音 FLV 始终使用第一优先级，不发生无意义切线；
- 原有分段时长、文件大小分段、弹幕滚动保存正常；
- 手动停止、暂停、下播宽限期和投稿行为正常；
- 缺失补传和时间戳修复流程正常。

## 9. 风险与应对

| 风险 | 应对 |
|---|---|
| 抖音响应没有备用 Host | 回退同画质 HLS；仍无候选时退避，不伪造 URL |
| HLS 与 FLV Codec/时间戳不同 | 切换时强制新分段；上传前继续时间戳检测 |
| 短分段过多导致分 P 过多 | 先聚合/合并；设置每场上限；超限进入人工队列但不删除 |
| ffprobe/ffmpeg 不可用 | 保守保留文件并告警，不因探测工具缺失删除 |
| 路线频繁振荡 | 熔断冷却、稳定窗口、禁止主动即时切回 |
| 动态签名过期 | 每次选择候选前刷新直播信息，不缓存完整 URL 超过有效期 |
| 宿主机公网故障 | 阶段 5 备用出口；线路切换失败时明确记录“跨 Host/协议均失败” |
| 日志泄露签名/Cookie/代理凭证 | 只记录 Host 和脱敏标识；增加专门测试 |

## 10. 推荐实施顺序

按以下顺序执行，不跨阶段并行上线：

1. 阶段 0：先获得真实候选结构和完整错误链。
2. 阶段 1：先解决内容被删除的问题。
3. 阶段 2：修正重试状态机，为熔断提供可靠计数。
4. 阶段 3：建立候选模型，默认仍只使用主选 URL。
5. 阶段 4：灰度启用同画质线路/协议切换。
6. 阶段 5：仅在确认存在自有备用出口时实施。
7. 阶段 6：根据指标调整默认值。

阶段 0–2 完成后，即使抖音没有提供可切换 CDN，也能显著降低内容损失并避免重试风暴；阶段 3–4
建立在真实响应证据之上，不提前假设抖音一定返回多 Host 候选。
