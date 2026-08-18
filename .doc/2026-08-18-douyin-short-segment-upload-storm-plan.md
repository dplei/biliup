# 抖音短片段上传风暴后续修改计划

## 1. 背景与结论

2026-08-17 小黄人录制日志暴露出一条完整的故障放大链路：

1. 抖音 `pull-flv-t13.douyincdn.com` 返回 HTTP 200，但 HTTP-FLV 连接在收到少量媒体数据后被远端以 HTTP/2 `PROTOCOL_ERROR` 重置；
2. 每次重连形成一个约 2–4 MB、实际媒体内容约 6 秒的短 FLV；
3. 短片段保留机制将这些文件归类为 `RecoverableShort`，避免了直接删除；
4. ffmpeg 批量合并短片段失败并返回退出码 254；
5. 当前失败回退策略把全部原始短片段逐个送入无界上传队列；
6. B 站返回 `601 上传过快` 后，上传器没有全局冷却，继续消费下一个文件，最终形成连续预上传请求和限流风暴。

本场汇总数据为：44 次连接失败、40 个可恢复短片段、短片段总计 123,653,409 字节、0 个合并产物、58 个文件进入上传队列、线路切换 0 次。

同期还观察到两个独立问题：

- B 站 UPOS 上传 CDN 证书过期，导致部分正常分段上传重试或失败，加重队列堆积；
- 抖音 Cookie 健康检查曾短暂误报“可能失效”，但随后自动恢复。故障窗口仍能持续取得 4 个候选流及有效 HTTP 200 拉流响应，因此 Cookie 不是本次 6 秒短片段的主因。

本计划是 `.doc/2026-08-14-douyin-download-resilience-plan.md` 的故障复盘补充，重点补齐原计划未覆盖充分的“短片段收敛”和“上传侧止洪”。

## 2. 目标

### 2.1 功能目标

- 抖音 FLV 持续异常时，优先切换到响应真实提供的同画质 HLS，减少短片段产生。
- 可恢复短片段先聚合、修复和合并，不因合并失败立即逐个调用上传接口。
- B 站明确返回 `601` 后，全进程上传立即进入冷却，不继续尝试队列中的下一个文件。
- 修正 FLV 时长探测，日志和策略使用片段真实媒体时长，而不是源流绝对时间戳。
- 区分 Cookie 鉴权失败、直播信息接口网络失败和视频传输失败，避免误判和无效换 Cookie。
- 日志不得输出抖音签名参数、Cookie、Token 或完整拉流 URL。

### 2.2 可靠性目标

- 一批几十个短片段最多形成一个待恢复任务，不形成几十次即时 `pre_upload`。
- 合并、修复或上传失败时，原始媒体文件在 durable 状态落库前不得删除。
- 上传冷却期间允许下载和落盘继续进行，但不得忙循环或无限占用内存。
- 所有新行为有配置开关、结构化指标、测试和独立回滚路径。

## 3. 非目标

- 不通过字符串替换伪造抖音 CDN Host 或签名 URL。
- 不把频繁刷新抖音 Cookie 作为 CDN 传输错误的修复方案。
- 不在本轮重做整个投稿模型或 B 站稿件编辑流程。
- 不在缺少证据时自动删除无法合并的短片段。
- 不承诺通过应用代码修复第三方 CDN 的过期证书，只保证正确换线、退避和保留现场。

## 4. 总体处理链路

```text
抖音候选流
  ├─ 当前 FLV 正常 → 正常分段
  └─ 当前 FLV 连续传输失败
       ├─ 熔断当前 RouteKey
       ├─ 优先切同画质 HLS
       └─ 已落盘尾段 → RecoverableShortCollector
                              ├─ 参数与时间戳探测
                              ├─ 预修复 / 重建时间戳
                              ├─ 批量合并成功 → 一个上传单元
                              └─ 合并失败 → 延迟恢复队列，不直接逐片上传

UploadRateGate
  ├─ 正常 → 按最小请求间隔放行
  ├─ 601 → 全局冷却并停止消费新上传任务
  └─ 冷却结束 → 单个探测请求成功后逐步恢复
```

## 5. 阶段拆分

---

## 阶段 0：固定故障样本与补齐可观测性

### 目标

先把 2026-08-17 的故障链路固化为可重复测试，避免后续修改只在生产日志中验证。

### 修改内容

1. 制作脱敏 fixture，覆盖：
   - HTTP 200 后只发送约 6 秒媒体帧，连接保持约 30 秒后远端重置；
   - 连续产生 10–40 个 2–4 MB 的有效短 FLV；
   - ffmpeg 合并返回非零；
   - 第一个 `pre_upload` 返回 B 站 `601`。
2. 为短片段增加可观测字段：
   - `media_duration_ms`；
   - `connected_ms`；
   - `received_bytes`；
   - `first_media_timestamp_ms`、`last_media_timestamp_ms`；
   - `recovery_batch_id`。
3. ffmpeg 调用改为捕获有上限的 stderr，日志记录退出码、失败阶段和脱敏摘要。
4. 增加上传指标：队列深度、等待冷却数量、`601` 次数、冷却剩余时间、每分钟 `pre_upload` 次数。

### 涉及文件

- `crates/biliup/src/downloader/httpflv.rs`
- `crates/biliup-cli/src/server/common/download.rs`
- `crates/biliup-cli/src/server/common/upload.rs`
- `crates/biliup-cli/src/server/common/util.rs`
- 测试 fixture 目录

### 验收标准

- 单元/集成测试可稳定复现“短片段合并失败后上传风暴”。
- 日志能够区分连接存活时间和真实媒体时长。
- ffmpeg 失败不再只有退出码 254。

### 回滚

只新增测试和诊断字段，不改变生产策略，可独立回滚。

---

## 阶段 1：上传侧 `601` 全局熔断与节流

### 目标

即使上游一次释放几十个文件，也不能在 B 站限流后继续快速调用预上传接口。

### 设计

新增进程级 `UploadRateGate`，统一约束正常分段、短片段、缺失补传和手动恢复入口。

建议状态：

```rust
enum UploadGateState {
    Ready,
    CoolingDown { until: Instant, strikes: u32 },
    Probing,
}
```

策略：

- 正常情况下，两个新文件的 `pre_upload` 之间保持可配置的最小间隔；
- 收到 `UploadRateLimited { code: 601 }` 后立即进入全局冷却；
- 冷却时间指数增长，例如 60s、120s、300s，最高 30 分钟，并加入少量抖动；
- 冷却期间不调用线路探测和 `pre_upload`，任务保留在有界队列或数据库；
- 冷却结束只放行一个探测任务，成功后恢复，仍为 601 则继续延长冷却；
- 601 不应被当作普通“当前文件失败”后立即跳到下一个文件；
- 应用重启后可从数据库中最近一次 601 时间恢复剩余冷却，避免通过重启制造新一轮风暴。

### 建议配置

```toml
upload_rate_gate_enabled = true
upload_min_request_interval_secs = 2
upload_601_initial_cooldown_secs = 60
upload_601_max_cooldown_secs = 1800
```

具体默认值在 fixture 和灰度环境中验证后确定。

### 涉及文件

- `crates/biliup/src/error.rs`
- `crates/biliup/src/uploader/line.rs`
- `crates/biliup-cli/src/server/common/upload.rs`
- `crates/biliup-cli/src/server/config.rs`
- 上传任务数据库模型及迁移（若需要持久化冷却）
- 前端配置组件

### 测试

- 第一个文件返回 601 后，冷却期内其他 39 个文件产生 0 次 `pre_upload`。
- 冷却结束只放行一个探测任务。
- 连续 601 会增长冷却时间且不超过上限。
- 普通单文件网络错误不错误触发全局 601 熔断。
- 正常上传吞吐不因并发竞争绕过最小间隔。
- 缺失补传和手动恢复同样受全局 Gate 约束。

### 验收标准

- 故障 fixture 中不再出现同一秒连续 5 次 `pre_upload`。
- 单进程所有上传入口都无法绕过 601 冷却。
- 冷却状态和剩余时间可从日志或状态接口查看。

### 回滚

关闭 `upload_rate_gate_enabled` 可恢复旧行为；数据库中的待上传任务保持不变。

---

## 阶段 2：短片段由“逐片上传”改为“合并或延迟恢复”

### 目标

短片段合并失败时保住文件，但不得立即把每个原片转换成独立上传请求。

### 状态模型

```rust
enum RecoveryBatchState {
    Collecting,
    ReadyToMerge,
    Merging,
    ReadyToUpload,
    Deferred,
    Completed,
}
```

### 策略

- 同一直播会话、Codec、分辨率、音频参数兼容的短片段进入同一 `recovery_batch_id`；
- 收到稳定正常分段、达到批次时长/体积阈值或会话结束时触发合并；
- 合并成功只产生一个上传单元；
- 合并失败时状态改为 `Deferred`，保留原文件并记录错误，不再执行“逐个上传 originals”；
- 延迟恢复任务由后台低频重试，并同样受 `UploadRateGate` 约束；
- 不兼容片段拆成不同批次，但仍不在故障瞬间批量调用上传接口；
- 设置每场批次数量、磁盘占用和保留时间告警，超限只告警和暂停自动处理，不删除原片。

### 建议配置

```toml
recoverable_short_segment_mode = "merge_or_defer"
recoverable_short_batch_target_secs = 300
recoverable_short_batch_max_files = 60
recoverable_short_retry_interval_secs = 900
```

### 涉及文件

- `crates/biliup-cli/src/server/common/download.rs`
- `crates/biliup-cli/src/server/common/upload.rs`
- `crates/biliup-cli/src/server/common/timestamp_repair.rs`
- 短片段恢复任务数据库模型及迁移
- `crates/biliup-cli/src/server/config.rs`

### 测试

- 40 个兼容短片段合并成功时只产生 1 个上传任务。
- 合并返回 254 时产生 1 个 `Deferred` 批次和 0 个即时上传任务。
- 应用重启后可恢复未完成批次，不重复入队。
- 原始文件在合并产物成功上传并 durable 落库前不会删除。
- 不兼容媒体参数不会被强制拼接，也不会形成即时上传风暴。

### 验收标准

- “合并失败 → 上传 originals”路径完全移除或只保留显式人工操作入口。
- 单场短片段数量不再直接等于预上传请求数量。
- 磁盘保留、重试和最终处理结果可审计。

### 回滚

关闭自动批次处理后，文件和数据库任务仍保留，允许人工恢复；不得回滚为自动逐片上传。

---

## 阶段 3：修复短片段时间戳探测与合并流程

### 目标

让 6 秒短片被正确识别，并提高连续 FLV 尾段的合并成功率。

### 修改内容

1. 修正 `probe_flv`：
   - 扫描第一个和最后一个有效音视频媒体 Tag；
   - `duration = last_timestamp - first_timestamp`；
   - 处理时间戳回绕、倒退和异常跳变；
   - 第一个 Tag 的绝对时间戳仅作为诊断字段，不再作为 duration。
2. 合并前对每个短片段执行轻量预检：
   - 音视频轨存在；
   - Codec、分辨率、采样率等兼容；
   - 首尾时间戳及关键帧情况可解析。
3. 合并分层回退：
   - 第一层：修正时间戳后 concat + stream copy；
   - 第二层：逐片 remux 到统一中间容器后 concat；
   - 第三层：仅在显式配置允许时重编码；
   - 全部失败：转为 `Deferred`，不逐片上传。
4. 每层都捕获 stderr 摘要和耗时，临时产物失败后清理，原文件保留。

### 涉及文件

- `crates/biliup-cli/src/server/common/util.rs`
- `crates/biliup-cli/src/server/common/download.rs`
- `crates/biliup-cli/src/server/common/timestamp_repair.rs`

### 测试

- 首个媒体时间戳为 9,192 秒、真实内容为 6 秒时，探测结果必须约为 6 秒。
- 连续绝对时间戳短片可在归零后正确合并。
- 时间戳倒退、回绕和缺少关键帧有明确分类。
- 合并失败日志包含有界 stderr 摘要且不泄露文件外敏感数据。

### 验收标准

- 日志 `media_duration_ms` 与 ffprobe 结果在允许误差内一致。
- 2026-08-17 fixture 的短片段合并成功率显著提高；仍失败时安全进入延迟队列。

### 回滚

可关闭自动 remux/重编码，但真实时长探测修复应保留。

---

## 阶段 4：灰度启用抖音 FLV → HLS 故障切换

### 目标

从源头减少同一故障 FLV 路线反复生成短片段。

### 修改与配置方向

- 保持只使用抖音响应真实返回的候选 URL；
- 显式开启 `route_health_enabled`、`douyin_route_failover` 和 `douyin_protocol_fallback`；
- 当前 FLV 在短窗口内连续发生两次远端 reset、body decode error 或读超时后熔断；
- 优先切同画质 HLS；只有响应真实提供不同 FLV Host 时才切不同 Host FLV；
- 自动降画质继续默认关闭；
- 切换后必须新建媒体分段；稳定运行达到阈值后再恢复路线健康；
- 当前日志中的 4 个候选是协议/画质组合，不应表述为 4 条独立 CDN 路线。

### 涉及文件

- `crates/biliup/src/downloader/live/douyin.rs`
- `crates/biliup-cli/src/server/common/route_health.rs`
- `crates/biliup-cli/src/server/common/download.rs`
- `crates/biliup-cli/src/server/core/downloader/stream_gears.rs`
- `crates/biliup-cli/src/server/config.rs`
- 前端配置组件

### 测试

- FLV 每 30 秒被远端 reset，第二次失败后切到同画质 HLS。
- HLS 稳定时不再继续探测故障 FLV。
- 所有候选失败时进入有界退避，不形成忙循环。
- 直播真实下播仍按原有离线宽限期结束。
- 配置关闭时保持当前单路线行为。

### 灰度顺序

1. 只对小黄人开启同画质 FLV → HLS；
2. 连续观察至少 3 场或 7 天；
3. 确认没有错误下播、重复投稿和明显画质下降后扩展到其他抖音主播；
4. 自动降画质另行评估，不与本阶段同时启用。

### 验收标准

- 同一 FLV RouteKey 连续失败后 `route_switches > 0`。
- 切换成功后短片段数量和估算缺失时长明显下降。
- 不再出现 `route_failover_enabled=false` 却误以为已启用切换的部署状态。

### 回滚

按主播关闭 `douyin_route_failover`，保留线路诊断和上传止洪能力。

---

## 阶段 5：Cookie 健康分类与敏感日志脱敏

### 目标

避免把普通网络失败误判为 Cookie 过期，并阻止签名参数出现在日志中。

### 修改内容

- Cookie 健康状态至少区分：
  - 明确鉴权失败：401/403、登录态无效、风控挑战；
  - 网络/证书/DNS/超时失败；
  - 服务端 5xx；
  - 返回结构异常。
- 只有明确鉴权失败或多次可重复的业务拒绝才提示更新 Cookie；
- 普通网络错误不得直接显示“Cookie 可能失效”；
- 所有错误链中的 URL 在进入日志前统一脱敏，删除 query 或至少遮盖：
  - `msToken`；
  - `a_bogus`；
  - `verifyFp`；
  - `sign`、`signature`、`expire`；
  - 其他 Cookie/Token 字段。
- Cookie 更新建议由健康状态驱动，不设置无依据的高频定时刷新。

### 涉及文件

- `crates/biliup-cli/src/server/common/cookie_health.rs`
- `crates/biliup-cli/src/server/core/monitor.rs`
- `crates/biliup/src/downloader/live/douyin.rs`
- 通用 URL/错误脱敏辅助模块

### 测试

- 网络超时不会产生 Cookie 失效告警。
- 401/403 或明确登录态错误会产生一次聚合告警。
- 测试日志中不得出现 `msToken=`、`a_bogus=`、Cookie 或完整签名 URL。
- Cookie 恢复后健康状态正确复位。

### 验收标准

- 生产日志不再泄露抖音签名参数。
- Cookie 告警可以明确说明是鉴权失败还是网络失败。

### 回滚

健康分类可回滚，但 URL 脱敏不得回滚。

---

## 阶段 6：灰度、指标与默认值评估

### 上线顺序

1. 阶段 0：fixture、指标、ffmpeg stderr；
2. 阶段 1：上传 601 熔断，先阻止接口风暴；
3. 阶段 2：短片段 merge-or-defer；
4. 阶段 3：真实时长和合并增强；
5. 阶段 4：对小黄人灰度 FLV → HLS；
6. 阶段 5：Cookie 分类和日志脱敏可提前独立上线；
7. 观察稳定后再评估默认开启范围。

### 核心指标

- 每场 `connection_failures`、`route_switches`、切换成功率；
- 每场短片段数量、真实总时长、合并成功率；
- `Deferred` 批次数、磁盘占用和最长等待时间；
- 每分钟 `pre_upload` 次数；
- B 站 601 次数、冷却次数、冷却后恢复成功率；
- 每场最终上传分 P 数量；
- Cookie 明确鉴权失败与网络失败分别计数；
- 日志敏感信息扫描结果。

### 默认值调整门槛

- 至少 7 天或 20 场抖音直播灰度；
- 601 后无额外预上传请求穿透冷却；
- 合并失败不会触发逐片上传；
- FLV → HLS 不导致错误下播或重复投稿；
- 待恢复文件磁盘占用有告警且可控；
- 日志脱敏测试持续通过。

## 6. 建议提交拆分

每个提交应可独立测试和回滚：

1. `test(download): reproduce douyin short segment storm`
2. `fix(media): report actual flv media duration`
3. `fix(download): capture short segment merge diagnostics`
4. `feat(upload): add global pre-upload rate gate`
5. `fix(upload): cool down globally after bilibili 601`
6. `refactor(download): persist recoverable segment batches`
7. `fix(download): defer originals when recovery merge fails`
8. `fix(download): normalize flv timestamps before concat`
9. `feat(douyin): gray-roll flv to hls failover`
10. `fix(cookie): classify auth and transport health separately`
11. `fix(logging): redact douyin signed query parameters`
12. `docs: document short segment recovery operations`

禁止把上传熔断、数据库迁移、短片合并重构和线路切换压在同一个提交中。

## 7. 全量验证清单

### 自动测试

```bash
cargo fmt --all -- --check
cargo test -p biliup httpflv -- --nocapture
cargo test -p biliup uploader -- --nocapture
cargo test -p biliup-cli download -- --nocapture
cargo test -p biliup-cli upload -- --nocapture
cargo test -p biliup-cli route_health -- --nocapture
cargo test -p biliup-cli cookie_health -- --nocapture
cargo check -p biliup-cli
```

### 故障注入

- HTTP 200 后发送 6 秒媒体并保持连接 30 秒，再发送 HTTP/2 reset；
- 连续生成 40 个兼容短片段；
- ffmpeg concat 返回 254；
- 首次预上传返回 601；
- 冷却后探测仍返回 601；
- FLV 失败、同画质 HLS 正常；
- FLV/HLS 全部失败但直播状态仍为 Live；
- B 站 UPOS 证书错误或连接失败；
- 抖音直播信息接口网络超时与明确 403 分别发生；
- 应用在冷却或短片段合并中途重启。

### 必须满足的结果

- 601 冷却期内没有新预上传请求；
- 合并失败后没有自动逐片上传；
- 原始文件没有丢失；
- 重启后任务不重复、不丢失；
- FLV 故障可在配置允许时切到 HLS；
- 日志中没有 Cookie、Token 和完整签名 URL。

## 8. 风险与应对

| 风险 | 应对 |
|---|---|
| 601 冷却过长导致投稿延迟 | 状态可见、单探测恢复、允许人工查看但不绕过安全 Gate |
| 延迟恢复导致磁盘堆积 | 有界批次、磁盘水位告警、保留策略只暂停自动处理而不删除 |
| 时间戳修复消耗 CPU | stream copy 优先，重编码显式开启并限制并发 |
| HLS 回退增加延迟或分片差异 | 仅故障时切换、强制新分段、继续媒体验证 |
| 多进程部署绕过进程级 Gate | 将冷却状态持久化或使用数据库锁，部署前确认运行模型 |
| Cookie 网络误报继续发生 | 按错误类别计数，只有明确鉴权错误触发更新提醒 |
| 错误链重新带出完整 URL | 在日志边界统一脱敏，并增加敏感字段扫描测试 |

## 9. 推荐优先级

### P0：先阻止再次触发上传接口风暴

1. B 站 601 全局冷却；
2. 合并失败改为 `Deferred`，禁止自动逐片上传；
3. 捕获 ffmpeg stderr，保留原始文件。

### P1：减少短片段产生并提高恢复率

1. 修复 FLV 真实时长探测；
2. 时间戳归零和分层合并；
3. 小黄人灰度开启 FLV → HLS 切换。

### P2：运维与安全收尾

1. Cookie 健康错误分类；
2. 抖音签名 URL 脱敏；
3. 指标、磁盘告警和默认值评估。

P0 完成前，不建议扩大 `preserve_recoverable_short_segments` 的启用范围；否则任何新的抖音传输故障都可能再次把“内容止损”放大成上传接口风暴。
