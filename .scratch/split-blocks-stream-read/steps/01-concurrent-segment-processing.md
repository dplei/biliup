# 01 拉流与分段处理并发执行

Status: ready-for-human

## 改动

[`download.rs`](../../../crates/biliup-cli/src/server/common/download.rs)：

- `DownloadTask::download`：`select!` 循环改为 `join!`，拉流结束后 `segment_rx.close()`。
- `SegmentEventProcessor::process`：`validate` 改为 `spawn_blocking`。
- `flush_pending_short_segments`：`merge_compatible_segments` 改为 `spawn_blocking`。

## 验收

- [ ] dev 环境实录抖音 FLV，把定时分段设为 60–120s，跑过至少 3 次切分：切分后不再紧跟一个
  `StreamEnded` 的几秒短段，`httpflv connection closed` 不再出现在切分后约 20s。
- [ ] 上线后生产日志：`close_reason=TimedSplit` 之后的下一段不再是 `queueing recoverable short
  media segment ... close_reason=StreamEnded`。
