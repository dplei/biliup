# 01 拉流与分段处理并发执行

Status: ready-for-human

## 改动

[`download.rs`](../../../crates/biliup-cli/src/server/common/download.rs)：

- `DownloadTask::download`：`select!` 循环改为 `join!`，拉流结束后 `segment_rx.close()`。
- `SegmentEventProcessor::process`：`validate` 改为 `spawn_blocking`。
- `flush_pending_short_segments`：`merge_compatible_segments` 改为 `spawn_blocking`。

## 验收

- [x] dev 环境实录抖音 FLV A/B（2026-09-24，见下方「dev 验证」）。
- [ ] 上线后生产日志：`close_reason=TimedSplit` 之后的下一段不再是 `queueing recoverable short
  media segment ... close_reason=StreamEnded`。

## dev 验证（2026-09-24）

同一个抖音直播间（HTTP-FLV），分段设为 120s。在 `FileValidator::validate` 里对大于 20MB 的文件
临时加 20s sleep，模拟生产上完整段约 20s 的校验耗时（验证完已还原，没有提交）。新旧两个
二进制各跑 2 次定时切分：

| | 切分后新段 | 连接 |
|---|---|---|
| 旧版（`origin/dev`） | 2 次都只有 3982ms / 1983ms，`close_reason=StreamEnded` | 切分后 20.1s `httpflv connection closed outcome="stream_ended" silent_ms=0 since_last_split_ms=20127 / 20105`，随后重连 |
| 新版（本分支） | 录满 120s 进入下一次 `TimedSplit` | 2 次切分都没有断连，没有 `stream_gap` |

旧版把生产现象完整复现：短段时长、约 20s 后断开、`silent_ms=0` 全部吻合。新版在校验跑满 20s
期间持续读流。

新版那场的分段在校验后打出了 `late validated segment belongs to a finalized session`。这是
测试环境的副作用：两场之间服务停了约 12 分钟，旧版那场的会话已经按 delay 投稿收尾。与本修复无关。
