# 02 · 把上传身份传入外部命令失败事件

Status: ready-for-agent
Blocked by: 01
优先级：P0

## 目标

让服务端上传预处理的 `processing.command_failed` 与同次 `processing.completed` 使用相同的
分段、session、missing 和 attempt 身份，从现有事件页按关联 ID 查询时能同时看到失败过程与
最终降级结论。

## 改哪里

[`crates/biliup-cli/src/observe.rs`](../../../crates/biliup-cli/src/observe.rs)：

- 按 `RecordingIdentity::context` 的现有写法增加 `UploadIdentity::context()`，只复制调用方已经
  持有的字段，不做推断。

[`crates/biliup-cli/src/server/common/ffmpeg_scan.rs`](../../../crates/biliup-cli/src/server/common/ffmpeg_scan.rs)：

- `ScanObserver` 接受可空的显式 `Context`。
- 发失败事件时合并该 context 与扫描文件名；有上传身份时以身份中的稳定 `original_file` 为准，
  没有身份时保持当前“扫描路径 basename”的行为。
- 继续直接写本次 collector，不改成 tracing span。

[`crates/biliup-cli/src/server/common/audio_normalization.rs`](../../../crates/biliup-cli/src/server/common/audio_normalization.rs)、
[`timestamp_repair.rs`](../../../crates/biliup-cli/src/server/common/timestamp_repair.rs) 与
[`upload.rs`](../../../crates/biliup-cli/src/server/common/upload.rs)：

- 系统 runner 保存 owned context，上传编排构造 runner 时传入同一份 `UploadIdentity` 快照。
- 响度测量、响度转码、时间戳检测和时间戳 remux 共用这条显式传递；样片、测试和 hook 没有
  上传身份时继续用空 context。

## 验收

- 扩展现有 `nonzero_scan_emits_bounded_native_diagnostic`，在显式 context 中放入假的
  `segment_id` / `upload_attempt_id`，断言失败事件完整保留两者与脱敏后的 basename。
- 复用现有仓储查询测试，不另建一套查询框架；字段存在后现有等值过滤即可命中。
- 确认无身份的 `ScanObserver` 调用仍通过原测试。
- 跑 `cargo test -p biliup-cli`。

## 契约回写

实现完成后同步更新：

- `.scratch/structured-logging/contract-v1.md`：删除已不存在的 `timestamp_reencode` stage，补充
  timestamp repair 的三个 fallback reason。
- `.scratch/structured-logging/coverage-ledger.md`：C06/C13 写明服务端上传预处理显式携带
  `UploadIdentity`；未知身份仍允许为空。
- `.scratch/structured-logging/receipts/P3.md`：记录回归命令与结果。

## 不做

- 不改查询 API、SQLite schema 或前端。
- 不给自定义 hook、样片工具等没有上传账本身份的路径造 ID。
