# 03 — 缺口统计口径修正与可观测

Status: implemented / 待生产验收（2026-08-29）

## 背景

`estimated_missing` 已经存在（[`download.rs:921/1118/1153`](../../../crates/biliup-cli/src/server/common/download.rs)），
但只累加 `check_elapsed + backoff`——**从报错之后**开始算。
真实缺口的大头是"上游停发数据到报错"那 ~20 秒静默，它结构上看不见。
结果是整场丢近 5 分钟，日志里的 `estimated_missing_ms` 只有 30 秒量级，
问题只能靠人肉翻日志或看成片跳针才发现。

## 改动范围

### 1. `Connection` 记录最后收到字节的时刻

`Connection` 已有 `received_bytes` 与 `started_at`（[`httpflv.rs:300-345`](../../../crates/biliup/src/downloader/httpflv.rs)）。
增加 `last_chunk_at: Instant`（构造时等于 `started_at`，每次成功 `chunk()` 后更新），
并在 `ConnectionDiagnostics` 增加 `silent_for: Duration`。

`httpflv download failed` 的 `warn!` 里补上 `silent_ms`。这一条本身就能让"上游何时停发"
在日志里直接可读，不必再靠字节数反推。

### 2. 缺口口径改为三段式

每次重连打一条结构化日志：

```
event="stream_gap"
silent_ms=       上游最后一个字节 → 连接判死
detect_to_retry_ms=  判死 → 新文件首次写入（含 check_stream 与 backoff）
total_gap_ms=    两者之和
```

`estimated_missing` 累加 `total_gap_ms` 而不是现在的 `check_elapsed + backoff`。

注意：`silent_ms` 会天然包含"正常分段后到上游停发"的那 ~1.5 秒有效数据时间，
按缺口口径这部分不算丢失（`02` 落地后确实没丢）。实现时用
"最后一个**写入文件**的 tag 时刻"作为起点更准确；若拿这个值的成本过高，
用 `last_chunk_at` 近似并在日志字段名上如实反映（`silent_ms` 而非 `lost_ms`），
不要用一个偏乐观的口径去糊弄自己。

### 3. 会话汇总与对外暴露

- `download_resilience_session_summary` 增加 `stream_gap_count`，
  `estimated_missing_ms` 语义随新口径更新；
- 健康接口与补传页展示"本场断流 N 次 / 累计缺失约 X 秒"。
  先确认现有接口里最合适的挂载点（`missing_segment_health` 或 session 详情），
  不要为此新开一个端点。

## 验收标准

1. 单测：fake connection 在收到若干 chunk 后停止产出，断言 `diagnostics().silent_for`
   与实际静默时长一致（容差 100 ms）。
2. 单测：`Connection::new` 后立即取 `silent_for` 得到接近 0 的值，而不是未初始化的大数。
3. 单测：缺口三段式的算术——给定 silent/detect_to_retry 的构造值，
   `total_gap_ms` 等于两者之和，且 `estimated_missing` 按次累加。
4. `cargo test -p biliup -p biliup-cli` 全绿。

生产验收：

5. 一场直播结束后，`estimated_missing_ms` 与人工核对结果（逐个边界取上下段媒体时间戳差求和）
   的误差 < 10%。这是本 ticket 唯一真正重要的判据——口径对不对，只能这样验。
6. `stream_gap_count` 等于日志中 `httpflv download failed` 的次数。
7. 页面能看到本场断流次数与累计缺失秒数。

## 备注

本 ticket 与 `01`/`02`/`04` 无代码依赖，但**应该先于它们上线**：
没有可信的缺口口径，后面三个改动的收益无法量化验证。

## 实现记录（2026-08-29）

- `Connection` 增加 `last_chunk_at`，`ConnectionDiagnostics` 增加 `silent_for` 与 `stall_timeout`；
- `StreamGapReport` 由 `StreamGears` 在连接结束时记录，`DownloaderRuntime::take_last_gap()`
  交给重连循环。非 FLV 路径拿不到这个口径，返回 `None` 时退回旧算法，行为与改动前一致；
- 三段式日志已落地：`event="stream_gap"`，字段 `silent_ms` / `detect_to_retry_ms` /
  `total_gap_ms` / `silent_measured` / `gap_index`；`estimated_missing` 改为累加 `total_gap_ms`；
- 会话汇总增加 `stream_gap_count`。

口径按原文的诚实要求命名为 `silent_ms`（起点是最后一个字节，不是最后一个落盘 tag），
另外补了一条更强的判据：`httpflv_connection_closed` 里带 `first_timestamp_ms` /
`last_timestamp_ms`。因为抖音的 FLV 时间戳是**流级绝对基准**
（见 [`findings-cdn-behavior.md`](../findings-cdn-behavior.md) 第 2 节），
把本次连接的 `last_timestamp_ms` 与下一次连接的 `first_timestamp_ms` 相减，
得到的就是**边界处真实丢失的媒体时长**，不再需要人工逐个分 P 比对。
验收第 5 条（误差 < 10%）现在可以直接用日志算。

### 未做

第 3 条的「健康接口与补传页展示」没做。`stream_gap_count` 与新口径的
`estimated_missing_ms` 已进结构化日志，页面挂载点等口径在生产上验准之后再说——
先展示一个还没验过的数字没有意义。
