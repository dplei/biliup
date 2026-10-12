# 媒体预处理

[返回根导航](../../CODE_INDEX.md) · [维护约定](../agents/code-index.md)

响度标准化、上传前时间戳检测与修复、ffmpeg 扫描、磁盘准入及进程优先级。

**从这里开始：** 先看 `normalize_for_upload`、`normalize_timestamps`；异常判据看 `ffmpeg_scan.rs`，调用顺序见上传领域。

表内路径均相对仓库根目录。职责与关系继承自已有索引；修改前仍需核对当前源码。

## 文件职责

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `crates/biliup-cli/src/server/common/audio_normalization.rs` | 上传前的双遍 `loudnorm` 响度标准化：探测、测量（顺带做时间戳诊断）、`-c copy` 视频 + 重编音频的转码，产物过严格校验后**原子替换原片**（`keep_original` 可退回旧的临时件形态），前后各有一道磁盘水位，全局单并发，任一步失败一律降级直传原片；服务端上传构造的系统 runner 显式携带 `UploadIdentity`，命令失败附件可按分段/attempt 关联。时长一律用 `content_span` 算出的**内容跨度**（直录 FLV 的 `format.duration` 是末尾时间戳，不是时长）；校验连续以同一理由失败会熔断，本进程停止发起标准化。 | `normalize_for_upload`、`NormalizationOutcome`、`NormalizedForm`、`NormalizationSettings`、`DiskBudget`、`output_is_faithful`、`OutputRejection`、`RejectionStreak`、`content_span`、`parse_probe_output`、`AudioFfmpegRunner`、`SystemAudioFfmpeg::with_context`、`parse_loudnorm_measurement`、`NORMALIZE_SLOTS` |
| `crates/biliup-cli/src/server/common/timestamp_repair.rs` | 上传前的时间戳异常检测与单级修复：`-c copy` + `setts` bsf **按流累加增量**重写 packet 时间戳（`delta_setts`：增量为负或超过 `MAX_PACKET_STEP_MS` 都压成 1ms，之后按真实增量走），回退与前跳一个表达式都修，内容一帧不丢、时长等于真实内容；只动容器元数据、payload 不解码。曾经的 `max()` 夹取 + 10 秒回退量闸门（`MAX_REPAIRABLE_BACKWARD_MS`）已随之删除——夹取会把大回退压成帧风暴，对前跳结构上无效。修复以复检为准，检测/remux/复检的进程失败分别返回带稳定原因的 `Fallback` 并直传；复检仍异常才是 `Unfixable`，**`upload.rs` 对它报错不上传、本地文件保留**（issue #62 起，此前是原片直传 + 告警）。**文本判据判干净之后还要过一遍 ffprobe 包时间戳数值判据**（`detect_packet_jump`，只在这一步跑，健康文件因此多付一遍解复用）：单调大跳变与**跨流起点差**超过 `MAX_PACKET_STEP_MS` 都报 `Anomalous { max_backward_ms: None }` 进修复；setts 首包 DTS 超过阈值即拉回 0（ffmpeg 已先减掉文件起点，这等于「本流比最早的流晚了一次跳变」），remux 只保留 `0:v:0?`/`0:a:0?`，不带 script 数据流（issue #75）；ffprobe 本身跑不起来时跳过该判据而不是升级成 `DetectFailed`。 | `normalize_timestamps`、`RepairOutcome`、`RepairFallbackReason`、`Detection`、`delta_setts`、`FfmpegRunner`、`SystemFfmpeg::with_context` |
| `crates/biliup-cli/src/server/common/ffmpeg_scan.rs` | 全片扫描类 ffmpeg 调用的 stderr 消费：边读边匹配时间戳异常并保留 64KiB 业务解析尾部，同时独立维护有界脱敏诊断附件；spawn/read/wait/非零退出按调用方 stage 发命令失败，exit 0 且所有命中行都解不出回退量时只收集这些命中行并发 `processing.diagnostic_captured`。`ScanObserver` 可接显式 owned context复用上传身份。异常行顺带解出**最大单次回退量**（认 muxer 两种措辞，未知必须保守处理）；另有 `PacketJumpScan` 不看措辞，按流扫描 ffprobe 包时间戳的大幅前跳，并给出跨流起点差（`start_skew_ms`，issue #75：各流单调、只是起点错开，流内判据全看不见）。可选 tee 保留旧 stderr；read_until 的单行业务临时缓冲尚无硬限。 | `run_scanning_stderr`、`run_packet_jump_scan`、`ScanObserver`、`StderrScan`、`PacketJumpScan`、`stderr_indicates_anomaly`、`parse_backward_ms` |
| `crates/biliup-cli/src/server/common/disk_space.rs` | 文件系统可用空间探测（unix `statvfs`，取非特权可用的 `f_bavail`）：响度标准化的准入与硬水位共用，探测不出一律返回 `None` 让调用方放行。 | `available_bytes` |
| `crates/biliup-cli/src/server/common/process_priority.rs` | 给上传前的 ffmpeg 预处理子进程设置后台 nice 与 IO 优先级，使其让路给网页请求；录制与用户 hook 不降级。 | `background`、`background_std` |

## 高信号关系

以下箭头包含控制流、数据流和跨语言边界，具体含义以边后的说明为准；不等同于完整函数调用图。

- `crates/biliup-cli/src/server/common/ffmpeg_scan.rs` → `crates/biliup-cli/src/observe/external.rs`（`timestamp_anomaly_unparsed`）：扫描成功但所有异常命中行都无法解析时，把仅含这些行的有界脱敏附件连同显式上传身份直接交给当前采集器。
- `crates/biliup-cli/src/server/common/audio_normalization.rs`、`crates/biliup-cli/src/server/common/timestamp_repair.rs`、`crates/biliup-cli/src/server/common/download.rs` → `crates/biliup-cli/src/server/common/process_priority.rs`（`background`）：预处理与分段恢复合并的 ffmpeg 一律降到后台优先级；`core/downloader` 的录制进程和用户自定义 hook 刻意不走这里。

## 跨领域继续阅读

- [录制与直播检测](recording.md)
- [日志与观测](observability.md)
