# 分段丢弃可观测性

来源：https://github.com/dplei/biliup/issues/36

## 状态

- 当前阶段：分析完成，待实现
- 范围：只补分段校验后实际删除的原生结构化事件；不改变过滤、保留、合并或上传行为

## 问题

`SegmentEventProcessor::process` 会把两类分段交给同一个
`remove_invalid_segment`：

1. 媒体有效、但小于 `filtering_threshold`，且短分段保留开关关闭；
2. 媒体探测结果为 `Invalid`。

当前删除函数只接收路径和自由文本原因，再启动一个 detached task 调用
`HookStep::remove_file`。因此 `SegmentInfo` 上已有的 `segment_id`、`attempt_id`，以及处理器上的
房间/场次身份都在删除边界前丢失。删除成功后只写 legacy INFO，结构化链路停在
`recording.segment_closed`。

legacy bridge 中 `file`、`reason` 被计为 rejected 不是桥接层类型故障：
`biliup-observability::Fields` 刻意只允许 `original_file`、`reason_code` 等契约字段，未知键拒收是
安全边界。修复不应放宽全局白名单。

## 决策

### 1. 在实际删除边界新增原生事件

新增 `recording.segment_discarded`，只在 `HookStep::remove_file` 成功后发出，级别 WARN：

| 字段 | 值 |
| --- | --- |
| `outcome` | `executed` |
| `reason_code` | 下方稳定词表 |
| `live_streamer_id` / `streamer_info_id` | `SegmentEventProcessor` 已持有的 `RecordingIdentity` |
| `segment_id` / `download_attempt_id` | 原 `SegmentInfo`，缺失时保持未知 |
| `original_file` | 原路径；采集层只保留脱敏 basename |
| `size_bytes` | 删除前已取得的文件大小 |
| `threshold_bytes` | 本次 `FileValidator` 的阈值 |

稳定原因词表：

- `below_filtering_threshold`
- `empty_file`
- `header_only`
- `unsupported_format`
- `malformed_container`
- `no_media_track`
- `probe_failed`

`InvalidMediaReason` 内部携带的自由错误文本只用于旧诊断，不进入 `reason_code`。

### 2. 删除必须被当前处理流程等待

把 `remove_invalid_segment` 改为 async，并由 `process` 直接 await。这样删除成功和原生事件在同一条
顺序路径中完成，不再存在 detached task 在录制结束或采集器关闭后才补日志的窗口。

删除失败仍保留原文件和既有 ERROR 行，不发 `segment_discarded`，因为没有发生内容丢弃；函数继续
吞掉删除错误，保持现有业务返回语义。

文件系统删除与事件库不可能跨资源原子提交；这里遵循既有「普通事件尽力保存」契约，保证的是
每条成功删除代码路径都同步发射事件，并消除 detached task 的额外丢失窗口。

### 3. legacy bridge 只修调用点，不改桥

保留既有文本日志，但把成功行字段改为契约名 `original_file`、`reason_code`。这能消除该行的
`rejected: 2`，同时不允许任意 legacy 字段进入结构化存储。身份与 WARN 语义由原生事件承担，
`system.legacy` 不作为覆盖证明。

## 明确不做

- 不做 `segment_created == segment_enrolled + segment_discarded` 场次对账。外部下载器允许只有
  `segment_closed`；record-only 路径不发 `segment_enrolled`；被保留、合并或延期的短分段也既未
  enrolled、也未 discarded。这条等式会正常误报。
- 不扩展 `recording.stopped`。当前已有逐分段事件足以查询丢弃数量与字节；等确有单事件看板需求
  再加汇总，避免同时维护两套口径。
- 不改变 `filtering_threshold`、`preserve_recoverable_short_segments` 或 #11 的内容恢复方案。
- 不为历史删除补造事件；历史事实缺少稳定 `segment_id`，不能可靠回填。

## 验收

1. 小于阈值且保留开关关闭的有效分段，删除后恰好产生一条 WARN
   `recording.segment_discarded`，原因是 `below_filtering_threshold`。
2. `HeaderOnly` 等无效分段映射到稳定原因，事件携带 R/S/DA、basename、大小和阈值，
   `fields.quality.rejected == 0`。
3. 删除失败时原文件仍在、既有 ERROR 仍有，且不产生成功丢弃事件。
4. 既有 valid、保留/合并、延期与 enrollment 路径不新增丢弃事件。
5. `cargo test -p biliup-cli --lib` 与 `cargo test -p biliup-observability` 通过。

## 预计改动面

- `crates/biliup-cli/src/server/common/download.rs`
- `crates/biliup-cli/src/observe.rs`
- `crates/biliup-observability/src/model.rs`
- `.scratch/structured-logging/contract-v1.md`、`coverage-ledger.md`（同步事件契约）

不需要 migration、配置项、新依赖或新模块。
