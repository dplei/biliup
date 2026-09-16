# dev 实录验证（2026-09-16）

环境：`data/` 复制到临时目录起 `target/debug/biliup server`（分支 `fix/issue54-260916-095401`），
`segment_time=00:01:00`、`preserve_recoverable_short_segments=true`、`filtering_threshold=20`、
显式上传线路，仅自己可见的投稿模板；抖音在播房间，origin 画质约 8 Mbps。

步骤：录满两个 60 s 分段，第三个分段开始约 10 s 后 `PUT /v1/streamers/<id>/pause`。

日志链（同一文件）：

```text
queueing recoverable short media segment  file_bytes≈17MB media_duration_ms=9985 close_reason=Cancelled
finished downloading result=Ok(Cancelled)
enqueueing lone recoverable short segment as its own part  file_bytes≈17MB close_reason=Cancelled
segment validated and enrolled  segment_order=2
upload attempt completed  segment_order=2
submit_attempt：开始下播一次性投稿 n_videos=3 trigger="segment_persisted"
APP接口投稿成功
download_resilience_session_summary ... recoverable_short_segments=1 merged_recovery_outputs=0 deferred_recovery_batches=0 lone_short_segments_uploaded=1
```

结论：

- 短片段按顺序成为该 session 第 3 个分 P（`segment_order=2`），稿件 3P 投稿成功。
- 没有 `recoverable segment deferred without immediate upload`；`recoverable_short_batch` 0 行；
  工作目录没有 `.biliup-recovery-*.json`。
- 走的是 `finish()` 的 flush 路径（暂停触发 `Cancelled`）。`process` 里「下一个 Valid 触发
  flush」的路径逻辑相同（先 flush 再 enqueue），本次未单独实跑。

留下的东西：临时目录整个删除，仓库 `data/` 未动。
