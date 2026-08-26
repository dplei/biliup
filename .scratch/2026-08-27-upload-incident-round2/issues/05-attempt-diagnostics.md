# 05 — 卡住分块的可诊断字段

Status: resolved
Blocked by: 01
Model: Sonnet 5 —— 以埋点与字段传递为主，改动面横跨 crate 边界（uploader → server），但每一步
都有明确目标，不涉及并发判断。

## 背景

对应评估报告 A-3。事故复盘时只能看到「不再出现 upload chunk acknowledged」，无法回答是哪一块、
哪条线路、请求耗时多少、报了什么错。

## 现状澄清

分块超时**已经存在**，不需要新加：单请求 240 秒
（[upos.rs:123](crates/biliup/src/uploader/line/upos.rs:123)），重试 3 次
（[`retry`](crates/biliup/src/lib.rs:15) 默认），连接超时 60 秒
（[client.rs:25](crates/biliup/src/client.rs:25)）。单块最坏阻塞约 13 分钟。清单里「每个分块必须
有明确超时」这条期望在实现上已满足，卡死的真因是 01 / 02。

缺的是可观测性：分块级失败只在 `retry` 内打印，不带线路、分块号、耗时，也不落库；页面只有
`last_error` 一个字段。

## 改动范围

- 在 attempt 维度记录：最后一次分块开始时间、分块号、线路、最近一次分块错误（脱敏走已有的
  [`sanitize_error`](crates/biliup-cli/src/server/common/upload_line_health.rs:129)）。
- `no_progress_timeout` / `total_upload_timeout` 触发时，把上述值一并写进 `last_error`，并记录
  实际已确认上传字节数。
- upos 分块重试失败时输出结构化日志：`line`、`chunk_index`、`attempt`、`elapsed_ms`、`error`。
- 新增字段若需要 migration，与 01 的 migration 合并成一张，避免连续两次改表。

## 验收

- 人为让某个分块永久无响应，watchdog 触发后能从数据库和页面同时看到：卡住的分块编号、线路、
  该请求耗时、最后错误、已确认字节数。
- 脱敏规则与现有一致，`X-Upos-Auth` 与 Cookie 不得出现在任何持久化字段里。

## Answer

已实现（字段并入 01 的 migration 16）。

- `upload_missing_segment` 新增 `last_chunk_index` / `last_chunk_started_at` / `last_chunk_error`，
  每次分块被确认时随进度一起落库。
- watchdog 的超时与上传失败路径都会写一段结构化诊断：
  `phase=... line=... chunk=... chunk_elapsed_secs=... acknowledged_bytes=...`，
  既进 `last_error` 也进 `last_chunk_error`，全部走 `sanitize_error` 脱敏。
- upos 分块重试改为逐次记日志：`line`、`chunk_index`、`attempt`、`elapsed_ms`、`chunk_bytes`、
  `timeout_secs`、`error`。为此 `Parcel`/`Upos` 带上了线路 key。
- 页面在「上传进度」列显示当前分块号与它已经跑了多久，「最后错误」列同时显示分块级诊断。

分块超时本身未改动（单请求 240 秒、重试 3 次），确认现状已满足要求。
