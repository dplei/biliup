# 时间戳修复事件不能把失败降级记成无异常

来源：[dplei/biliup#26](https://github.com/dplei/biliup/issues/26)

关联：#2（结构化日志）、#25（时间戳修复链路）

Status: resolved

> 本目录的 `steps/` 是实施步骤，不是 GitHub issue 编号。

## 一句话

时间戳检测、重写或复检的进程失败都会降级上传原片，但当前状态机把这些结果复用为
`RepairOutcome::Clean`，最终误记成 `processing.completed executed/no_anomaly`；同时，同一条
上传链上的 `processing.command_failed` 没有接入已有的 `UploadIdentity`，按分段或 attempt
查询时会漏掉真正的失败事件。

## 当前基线

#25 已删除整段 x264 重编码，当前自动修复只有：

```text
全片检测 → 回退量闸门 → -c copy + setts → 全片复检
```

因此 issue 正文里的 `timestamp_reencode` / `reencode_failed` 已经过时，本 effort 不恢复该
阶段，也不为不存在的路径保留原因码。仍然存在的错误出口是：

- 初次检测进程失败；
- remux 进程失败或产物无效；
- remux 后的复检进程失败。

## 根因

### R1. `Clean` 同时表达事实与降级策略

[`normalize_timestamps`](../../crates/biliup-cli/src/server/common/timestamp_repair.rs) 在输入确实
无异常时返回 `RepairOutcome::Clean`，但检测、remux、复检失败时也返回同一个值。

[`upload_single_file_with_repair`](../../crates/biliup-cli/src/server/common/upload.rs) 无法再区分
这些来源，只能把所有 `Clean` 映射为 `executed/no_anomaly`。错误发生在状态机丢失信息的地方，
不能靠解析旧告警或 `processing.command_failed` 反推。

### R2. 共享 FFmpeg 采集器只接受文件路径

[`ScanObserver`](../../crates/biliup-cli/src/server/common/ffmpeg_scan.rs) 只携带 `stage`、
`original_file` 和是否 tee stderr。`run_scanning_stderr` 直接发附件事件，所以不能依赖 tracing
span；它目前也没有入口接收上传编排已经持有的 `UploadIdentity`。

结果是 `processing.completed` 带完整分段身份，而对应的 `processing.command_failed` 只有文件名。
这是显式上下文没有传到直接 emitter，不是查询 API 的问题。

## 方案

### 1. 把失败降级从 `Clean` 中拆出来

保留现有四种业务结果：

- `Clean`
- `Repaired(PathBuf)`
- `Fallback(RepairFallbackReason)`
- `Unfixable`

`RepairFallbackReason` 只包含当前真实存在的三个原因：`DetectFailed`、`RemuxFailed`、
`VerificationFailed`。事件映射固定为：

| 实际结果 | `outcome` | `reason_code` |
| --- | --- | --- |
| 输入确实无异常 | `executed` | `no_anomaly` |
| setts 修复并复检通过 | `executed` | `repaired` |
| 初次检测进程失败，上传原片 | `fallback` | `detect_failed` |
| remux 失败或产物无效，上传原片 | `fallback` | `remux_failed` |
| 修复后复检进程失败，上传原片 | `fallback` | `verification_failed` |
| 回退超限、无法解析或修复后仍异常 | `failed` | `unfixable` |

`Fallback` 与原来的失败分支保持同一业务行为：不阻断上传，删除本次临时修复件，后续上传原片
或已经生成的响度标准化产物。`Unfixable` 的告警与清理行为不在本 issue 改写。

### 2. 把上传身份显式传给直接 emitter

沿用 [`RecordingIdentity::context`](../../crates/biliup-cli/src/observe.rs) 的现有模式，为
`UploadIdentity` 提供 owned `Context` 快照。上传编排把它交给本次使用的系统 FFmpeg runner，
runner 再放进 `ScanObserver`，最终由 `processing.command_failed` 原样写入。

同一上传链已知的以下字段必须保留：

- `task_id`
- `live_streamer_id`
- `streamer_info_id`
- `upload_session_id`
- `segment_id`
- `missing_id`
- `upload_attempt_id`
- `original_file`

`ffmpeg_scan` 仍为没有业务身份的调用保留只带扫描文件名的默认行为；样片处理、自定义 hook 等
不知道的字段继续为空，不能从路径或 ambient span 补造。服务端上传已经持有身份，因此响度与
时间戳两个预处理 runner 一起接入，避免修完 timestamp 后留下同根的 loudnorm 漏链。

## 明确不做

- 不恢复 `timestamp_reencode`，不增加 `reencode_failed`。
- 不新增事件名、数据库 migration 或 schema version。
- 不改日志查询 API；字段进入事件后，复用现有 `segment_id` / `upload_attempt_id` 过滤。
- 不用文件名推断分段身份，也不从 tracing span 搜索上下文。
- 不改变 `Unfixable` 的上传、告警与本地文件清理策略。

## 验收

- 三个进程失败出口分别返回稳定的 `Fallback` 原因，且不再产生 `no_anomaly`。
- 真正干净的输入仍是 `executed/no_anomaly`；修复成功与 `Unfixable` 映射不变。
- 受控 FFmpeg 非零退出产生的 `processing.command_failed` 同时带 `segment_id` 与
  `upload_attempt_id`；现有查询过滤无需改动即可命中。
- 无身份的既有调用仍能编译并保持原行为。
- `cargo test -p biliup-cli` 与结构化日志契约检查通过。

## 实施步骤

| # | 步骤 | 优先级 | 阻塞于 | 状态 |
| --- | --- | --- | --- | --- |
| 01 | [拆分时间戳修复的失败降级结果](./steps/01-fallback-outcome.md) | P0 | — | ✅ resolved |
| 02 | [把上传身份传入外部命令失败事件](./steps/02-command-failure-context.md) | P0 | 01 | ✅ resolved |
