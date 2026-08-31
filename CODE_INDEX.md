# Code Index

这是按实际代码探索路径增量维护的文件级导航，不追求一次性覆盖全仓。查代码时先按领域词、
路径片段或符号名查本页；未命中再使用代码图索引。维护规则见
[`docs/agents/code-index.md`](docs/agents/code-index.md)。

文件职责和跨文件关系刻意分开记录，避免摘要相互交织。

## 进程与语言入口

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `crates/biliup-cli/src/main.rs` | Rust CLI 二进制入口：初始化日志，解析命令并分派登录、上传、下载、Web 服务和封面预览等子命令。 | `main` |
| `crates/biliup-cli/src/observe.rs` | 业务原生事件的唯一发射点：`RecordingIdentity`/`UploadIdentity`/`SubmissionIdentity` 显式携带房间、场次、分段、attempt 与投稿会话身份（不建 span，避免改变旧输出），按契约发录制、预处理、上传、补传、投稿事件；空字符串表示「调用方没有这个身份」。 | `RecordingIdentity`、`UploadIdentity`、`UploadIdentity::from_missing_row`、`UploadIdentity::with_attempt`、`SubmissionIdentity`、`EVENT_TARGET`、`recording_started`、`segment_enrolled`、`processing_decided`、`upload_started`、`upload_failed`、`recovery_decided`、`submission_decided`、`submission_completed` |
| `crates/biliup-cli/examples/upload_pilot.rs` | P3/13 的受控后处理演练：本地 sqlite 跑真实登记、投稿判定与补传资格判定，两个 sink 同时输出，并生成证据包请求与预期事实清单。无账号、无网络。 | `drill`、`write_evidence_request` |
| `crates/biliup-cli/src/downloader.rs` | 独立 CLI 下载入口：插件提取后按媒体类型走 HTTP-FLV/HLS，拒绝需 Streamlink/YtDlp 运行时的路径，另支持 FLV 转 JSON 诊断。 | `download`、`download_stream`、`generate_json` |
| `crates/biliup-cli/src/lib.rs` | Web 服务启动与主播配置导入的编排层：建立 SQLite 连接、组装服务、恢复主播任务并启动 Axum。 | `run`、`import_config_streamers`、`import_database_streamers` |
| `crates/stream-gears/src/lib.rs` | Rust/Python 的 PyO3 边界，向 Python 暴露下载、上传、登录和 CLI 主循环；下载回调与上传函数各自安装局部控制台/文件日志订阅器。 | `stream_gears`、`main_loop`、`download_with_callback`、`download_with_hook`、`upload` |
| `crates/stream-gears/src/server.rs` | Python wheel 的实际 CLI 分派与日志初始化入口，同时向 Python 暴露进程内共享配置。 | `_main`、`config_bindings`、`ConfigState`、`CONFIG` |
| `biliup/__main__.py` | `python -m biliup` 的极薄入口，把执行权交给扩展模块的 `main_loop`。 | `main` |

## Rust 核心库

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `crates/biliup/src/lib.rs` | `biliup` 核心 crate 的公共门面，并提供带指数退避和抖动的通用异步重试。 | `retry`、`retry_with_config` |
| `crates/biliup/src/downloader/httpflv.rs` | HTTP-FLV 拉流与逐 tag 解析：按关键帧刷盘并在关键帧边界切分段，维护 onMetaData/序列头缓存，用可配置的单次 chunk 读超时（停顿看门狗，默认 30s）兜住码流停顿，断连时产出连接寿命、静默时长、分段进度与媒体时间戳的诊断。**切片关闭原因必须在计数复位之前取值**（否则恒为 `Unknown`）；DTS 倒退在源端按分段汇总（首条 1:1，其余计数/首末/极值）。 | `parse_flv`、`Connection`、`Connection::with_stall_timeout`、`DEFAULT_STALL_TIMEOUT`、`read_frame`、`ConnectionDiagnostics`、`FlvProgress`、`download_with_context`、`DtsBackwardRollup` |
| `crates/biliup/src/downloader/flv_writer.rs` | 录制落盘的 FLV 写入器：写 FLV 头、按 tag 原样写回时间戳（**不做任何偏移重基**，CDN 给什么就写什么），分段时刷盘换文件并交给 `LifecycleFile` 收尾。 | `FlvFile`、`FlvFile::write_tag`、`FlvFile::write_tag_header`、`FlvFile::create_new`、`FLV_HEADER` |
| `crates/biliup/src/downloader/util.rs` | 分段判据 `Segmentable`（时间/大小任一超限即切）与录制文件生命周期 `LifecycleFile`；时间判据用媒体时间戳做 `current - start` 的**饱和减法**，时间戳倒退时 elapsed 归零。**文件创建时分配稳定 `SegmentIdentity`**（关闭回调与登记账本都用它），并就地发 `recording.segment_created/closed` 原生事件。 | `Segmentable`、`Segmentable::needed`、`elapsed_time`、`set_time_position`、`set_start_time`、`LifecycleFile`、`LifecycleFile::create`、`SegmentIdentity`、`RecordingOwner`、`allocate_id`、`close_reason_code`、`SegmentCloseReason` |
| `crates/biliup/src/downloader/live/douyin.rs` | 抖音选流：解析房间信息后枚举各档位候选（含每档 `bitrate` 元数据），按请求档位就近选档并下发 flv/hls 地址与鉴权头；档位仅按名称在 `QUALITY_CODES` 内上下查找，**不参考码率**。 | `Douyin`、`DouyinLive::check_stream`、`select_quality_code`、`QUALITY_CODES`、`build_stream_candidates` |
| `crates/danmaku/src/lib.rs` | 弹幕录制 crate 的公共门面，组织客户端、协议、消息和 XML 输出模块并重导出稳定 API。 | `DanmakuRecorder`、`RecorderConfig`、`create_platform`、`XmlWriter` |

## Web 前端入口

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `app/layout.tsx` | Next.js 根布局，设置中文页面语言、全局样式和应用级 `body` 容器。 | `RootLayout` |
| `app/(app)/page.tsx` | 应用根页面的路由入口，直接把用户重定向到直播管理页。 | `Home` |
| `app/(app)/layout.tsx` | `(app)` 分组的侧边导航布局：Semi `Layout`/`Nav` 骨架、路由映射、折叠态与主题切换入口；只渲染布局结构，`html`/`body` 由根布局独占。 | `Layout`、`isSub` |
| `app/(app)/streamers/page.tsx` | 直播管理卡片列表：聚合主播运行状态与画质标签，并提供编辑、暂停、删除和单房间配置覆写入口。 | `Home` |
| `app/ui/StreamerActions/RecordingLeaseModal.tsx` | 录制期限的创建、延期、清除与状态/通知反馈弹窗，负责把浏览器本地选择转换为明确 UTC 时间点。 | `RecordingLeaseModal` |
| `app/ui/StreamerActions/CheckStreamButton.tsx` | 直播管理卡片上的「立即检查直播流」按钮：调一次主动检查接口并按结论提示，随后刷新主播列表。 | `CheckStreamButton` |
| `app/ui/StreamerActions/PauseButton.tsx` | 以显式目标状态暂停/恢复单个直播间；到期暂停的恢复入口会禁用并提示先处理期限。 | `PauseButton`、`setRecordingState` |
| `app/(app)/missing/page.tsx` | 缺失补传与待投稿控制页：独立轮询待投稿会话和分段列表，展示后端给出的投稿五态、attempt 阶段/进度/线路健康/完整性与线路历史，并触发会话恢复、空会话逻辑终结、补传、换线重投、停止、删除与本场补扫。 | `MissingRecovery`、`AttemptHistoryPanel` |
| `app/lib/api-streamer.ts` | 前端统一的 fetch 封装与错误处理边界：401 跳登录，JSON 错误透传，HTML/空正文按状态码翻译成中文提示。 | `fetcher`、`sendRequest`、`handleResponse`、`describeError` |

## 日志与存储基础设施

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `crates/biliup-observability/src/lib.rs` | 独立结构化日志 crate 门面，不依赖 Web 服务或业务数据库；目前以默认关闭的旁路接入各入口，旧文件与旧页面仍是权威来源。 | `Runtime`、`Emitter`、`CaptureLayer` |
| `crates/biliup-observability/src/model.rs` | 定义 v1 不可变事件、稳定/运行身份、显式上下文与扁平字段允许列表，区分原生与桥接来源。 | `Event`、`EventData`、`Fields`、`Context`、`Draft` |
| `crates/biliup-observability/src/capture.rs` | 从父子 span、延迟 record 和事件字段构建快照，以独立层过滤控制原生/桥接采集并排除内部 SQL。 | `CaptureLayer`、`legacy_output` |
| `crates/biliup-observability/src/sanitize.rs` | 对允许字段执行有界格式化、敏感线索整值脱敏和控制字符处理，不序列化未知 Debug 对象。 | `clean`、`Bounded`、`debug` |
| `crates/biliup-observability/src/diagnostic.rs` | 流式捕获外部诊断：跨 chunk 按有界行脱敏，保留首致命摘要、有限尾部和原始字节/截断信息。 | `DiagnosticCapture`、`Diagnostic` |
| `crates/biliup-observability/src/runtime.rs` | 提供条数/字节双限队列、重要级别预留、可替换后台消费者、提交后高水位、独立健康与限时关闭。 | `Runtime`、`Emitter`、`Consumer`、`Health`、`Options` |
| `crates/biliup-observability/src/sqlite.rs` | 独立 SQLite migrations 与单写入器、事件/附件幂等事务、只读游标查询、保留/WAL/低盘保护和一致性备份。 | `SqliteStore`、`StoreOptions`、`Repository`、`Query`、`MIGRATOR` |
| `crates/biliup-observability/src/shadow.rs` | 把独立采集以可关闭旁路接进各入口：启动时读环境开关、同库多次调用共享一个 run、把 runtime worker 绑定到同一 dispatch，并保留嵌入宿主已有 subscriber。 | `Shadow`、`Config::from_env`、`block_on_inherited`、`health_snapshot`、`Inherited` |
| `crates/biliup-observability/examples/shadow_acceptance.rs` | 新旧双路同时开启的合成负载入口：同时测量发射延迟、排空、两侧丢弃与新旧合计磁盘占用，并产出可导出的证据请求。 | `main`、`ticks` |
| `crates/biliup-observability/examples/acceptance.rs` | 隔离的日志预算验收入口：以合成双路事件测量发射、调度延迟、排空、分页和磁盘占用，不启动业务服务。 | `main`、`workload` |
| `crates/biliup-cli/src/server/api/ws.rs` | 基于文件的实时日志 WebSocket：白名单逻辑文件名映射到最新滚动文件，初连取末尾记录，再轮询新增内容并发送保活。 | `ws_logs`、`websocket_logs`、`resolve_latest_log`、`send_last_lines` |
| `app/(app)/logviewer/page.tsx` | 按日志文件切换 tab 的实时日志页：通过 WebSocket 追加文本、管理滚动/连接状态，并提供静态文件下载入口。 | `LogViewer`、`LogContent` |
| `app/ui/plugins/developer.tsx` | 开发者日志设置表单，编辑旧 `LOGGING` 配置及后端动态日志过滤使用的 `loggers_level`。 | `Developer` |
| `crates/biliup-cli/src/server/infrastructure/connection_pool.rs` | 创建固定 SQLite 类型的业务连接池（上限 2）并执行业务 migrations，另提供测试用迁移后临时库。 | `ConnectionPool`、`ConnectionManager::new_pool`、`test_support::migrated_pool` |

## 录制调度

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `crates/biliup-cli/src/server/core/monitor.rs` | 轮询各房间开播状态，命中开播时按平台场次键复用或新建本场 `streamer_info`，并在下载许可下拉起录制流程；单次检查抽成共用实现，供轮询与「主动检查」按钮各调一次。 | `start_monitor`、`check_room_once`、`check_now`、`CheckOutcome` |
| `crates/biliup-cli/src/server/common/download.rs` | 录制主流程与分段事件处理：拉流、断线重试、切片校验，把有效分段登记后交给上传管道，并在尾段 durable enrollment 后持久关闭会话投稿意图。 | `start_download_workflow`、`DownloadTask`、`SegmentEventProcessor`、`persist_closed_session_intents` |
| `crates/biliup-cli/src/server/core/downloader.rs` | 下载器类型分发与下载配置定义：`DownloaderType` 到具体实现的映射，**只有显式选 `Ffmpeg` 才走 `FfmpegDownloader`，其余一律回落到自研 FLV 解析的 `StreamGears`**——判断某个能力是否依赖 ffmpeg 录制路径时先看这里。 | `DownloaderType`、`DownloaderRuntime`、`DownloadConfig`、`parse_duration` |
| `crates/biliup-cli/src/server/core/downloader/stream_gears.rs` | 服务端拉流的具体执行器：按下载配置建 HTTP 客户端与 `Connection`，按后缀分流 FLV/HLS，读帧头失败即分类为可重试的传输错误，并给每次尝试打上 `attempt_id`/`stream_host` 便于串联断连诊断。 | `StreamGears`、`start_download`、`classify_download_error`、`classify_reqwest_error` |
| `crates/biliup-cli/src/server/common/util.rs` | 录像分段落盘后的有效性判据：容器探测、`HeaderOnly`（FLV ≤13 字节）与小于阈值的可恢复短分段分类，决定丢弃、入队合并还是登记上传。 | `FileValidator`、`MediaValidation`、`InvalidMediaReason`、`probe_flv` |
| `crates/biliup-cli/src/server/core/download_manager.rs` | 单平台下载编排的持有者：建 `Monitor` 与上传 Actor 池，并把房间增删、暂停入队/出队、主动检查转发给监控 Actor。 | `DownloadManager`、`add_room`、`make_waker`、`check_room_now` |
| `crates/biliup-cli/src/server/infrastructure/context.rs` | 持有单房间 Worker 的运行状态、画质与活动录制快照，并以 `Context` 传递本场 `streamer_info` 身份和业务依赖。 | `Context`、`Worker`、`ActiveRecordingSnapshot`、`WorkerStatus`、`Stage` |
| `crates/biliup-cli/src/server/infrastructure/models/live_streamer.rs` | 定义持久化直播间配置及其新增载荷；该模型的全字段更新语义要求独立运行状态不要混入主播配置。 | `LiveStreamer`、`InsertLiveStreamer` |

## Web 服务边界

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `crates/biliup-cli/src/server/app.rs` | 组装会话、认证、CORS、业务路由与静态回退，启动 Axum，并在退出信号后清理服务资源。 | `ApplicationController::serve`、`shutdown_signal` |
| `crates/biliup-cli/src/server/api/stream_check.rs` | 主动检查直播流的端点：把监控层的检查结论翻译成中文提示，正常状态回 200 + `outcome`，检查失败与建会话失败如实报错。 | `check_stream_now`、`CheckStreamResponse` |
| `crates/biliup-cli/src/server/common/cookie_health.rs` | 维护平台 Cookie 健康快照，并为钉钉、企微和通用 URL 提供共享 Webhook 告警分发。 | `record_success`、`record_error`、`notify_alert`、`snapshot` |
| `crates/biliup-cli/src/server/common/recording_lease.rs` | 实现录制租约的持久状态机、纯到期/准入决策、CAS 扫描、下播收敛和带 claim/退避的通知投递。 | `RecordingLease`、`due_action`、`admit_detected_session`、`complete_grace_session`、`scan_due_recording_leases` |
| `crates/biliup-cli/src/server/api/recording_lease.rs` | 提供租约创建/替换/清除与幂等录制状态接口，并将输入校验、乐观并发和到期恢复守卫映射为 400/404/409。 | `put_recording_lease`、`delete_recording_lease`、`put_recording_state` |
| `crates/biliup-cli/src/server/common/upload.rs` | 编排直播分段上传、会话级幂等投稿协调、缺失补传、attempt lease/watchdog（分阶段计时）、线路决策入口以及远端结果落库；分段成功会在事务外唤醒有 durable 投稿意图的父会话。 | `process_with_upload`、`reconcile_session_submission`、`spawn_session_submission`、`persist_segment`、`decide_upload_line`、`upload_enrolled_with_watchdog`、`claim_manual_recovery`、`run_claimed_recovery`、`stop_missing_segment_attempt`、`segment_part_title` |
| `crates/biliup-cli/src/server/common/attempt_lease.rs` | 定义 attempt 的三个阶段与各自的收割判据，提供心跳/阶段落库和 `upload_attempt` 历史表的读写。 | `AttemptPhase`、`classify_stale_lease`、`preprocess_deadline`、`record_heartbeat`、`close_attempt_history` |
| `crates/biliup-cli/src/server/common/upload_line_selection.rs` | 全仓唯一的上传线路决策：纯函数规划（配置/手动优先、冷却回退、auto 兜底）加一步 probe 解析。 | `plan_upload_line`、`resolve_planned_line`、`LinePlan`、`LineSource`、`cooling_lines` |
| `crates/biliup-cli/src/server/common/recovery_scheduler.rs` | 到期补传的主动扫描循环与后台执行：按会话串行、按 `segment_order` 顺序领取，接口只负责 claim。 | `start_due_recovery_scan`、`recover_due_segments`、`spawn_claimed_recovery` |
| `crates/biliup-cli/src/server/common/submission_scheduler.rs` | 数据库驱动的待投稿会话启动/周期扫描：只选有持久投稿意图、无 claim 且已到期的会话，以有界并发唤醒统一协调器并分类记录结果。 | `start_submission_reconciliation_scan`、`scan_due_submissions`、`due_submission_session_ids` |
| `crates/biliup-cli/src/server/common/segment_enrollment.rs` | 在有效媒体进入内存队列前原子登记 session/分段 identity，按场次键续接会话（时钟窗口只作缺键兜底），并在数据库不可用时写 fsync outbox。 | `enroll_validated_segment`、`find_or_create_session`、`import_outbox_once` |
| `crates/biliup-cli/src/server/common/recovery_eligibility.rs` | 统一补扫、静默恢复和人工恢复的只读资格判定，并负责把消失源文件收敛为终态、记录 finalized 审计。 | `check_recovery_eligibility`、`mark_source_missing`、`record_recovery_audit` |
| `crates/biliup-cli/src/server/common/upload_session.rs` | 维护投稿会话恢复（场次键优先、时钟窗口兜底）、单调持久投稿意图与退避时间，并以生命周期账本检查完整性、确定性重建分 P 和原子 claim/finalize；关闭态零基线会话在同一事务中逻辑终结为 `discarded_empty`。 | `SessionCompleteness`、`request_session_submit`、`session_submit_readiness`、`discard_empty_session`、`schedule_submit_retry`、`select_recovery_candidate`、`reusable_streamer_info`、`touch_session_activity`、`claim_complete_session` |
| `crates/biliup-cli/src/server/common/lifecycle_backfill.rs` | 把历史会话的 videos_json 与遗留 missing 行回填成 v2 生命周期账本，合并重复源、生成 legacy:// 基线并按会话断点续跑。 | `run_lifecycle_backfill`、`plan_session`、`BackfillPlan` |
| `crates/biliup-cli/src/server/common/upload_line_health.rs` | 分类上传网络错误并持久维护单线路失败次数、冷却和到期单探测租约。 | `UploadFailureKind`、`acquire_line`、`record_failure`、`record_success` |
| `crates/biliup-cli/src/server/common/audio_normalization.rs` | 上传前的双遍 `loudnorm` 响度标准化：探测、测量（顺带做时间戳诊断）、`-c copy` 视频 + 重编音频的转码，产物过严格校验后**原子替换原片**（`keep_original` 可退回旧的临时件形态），前后各有一道磁盘水位，全局单并发，任一步失败一律降级直传原片。时长一律用 `content_span` 算出的**内容跨度**（直录 FLV 的 `format.duration` 是末尾时间戳，不是时长）；校验连续以同一理由失败会熔断，本进程停止发起标准化。 | `normalize_for_upload`、`NormalizationOutcome`、`NormalizedForm`、`NormalizationSettings`、`DiskBudget`、`output_is_faithful`、`OutputRejection`、`RejectionStreak`、`content_span`、`parse_probe_output`、`AudioFfmpegRunner`、`parse_loudnorm_measurement`、`NORMALIZE_SLOTS` |
| `crates/biliup-cli/src/server/common/timestamp_repair.rs` | 上传前的时间戳异常检测与两级修复（copy 重封装 → 保画质重编码），每一步都以复检为准，进程层面失败一律降级直传。 | `normalize_timestamps`、`RepairOutcome`、`FfmpegRunner`、`SystemFfmpeg` |
| `crates/biliup-cli/src/server/common/ffmpeg_scan.rs` | 全片扫描类 ffmpeg 调用的 stderr 消费：边读边匹配时间戳异常并保留 64KiB 尾部，但 read_until 的单行临时缓冲尚无硬限。 | `run_scanning_stderr`、`StderrScan`、`stderr_indicates_anomaly` |
| `crates/biliup-cli/src/server/common/disk_space.rs` | 文件系统可用空间探测（unix `statvfs`，取非特权可用的 `f_bavail`）：响度标准化的准入与硬水位共用，探测不出一律返回 `None` 让调用方放行。 | `available_bytes` |
| `crates/biliup-cli/src/server/common/process_priority.rs` | 给上传前的 ffmpeg 预处理子进程设置后台 nice 与 IO 优先级，使其让路给网页请求；录制与用户 hook 不降级。 | `background`、`background_std` |
| `crates/biliup-cli/src/server/router.rs` | 组装主要 v1 业务 HTTP 路由和静态文件服务；认证与日志 WebSocket 另由应用启动层挂载。 | `router`、`static_file_router` |
| `crates/biliup-cli/src/server/api/endpoints.rs` | 实现 Web API 业务端点：缺失分段列表、独立待投稿会话五态、补传/重投/停止/按会话恢复、严格空会话逻辑终结、attempt 历史与上传健康查询。分段补传端点只 claim、不等上传；按会话恢复持久化人工投稿授权，并在无分段工作时异步唤醒统一投稿协调器。 | `get_missing_uploads`、`get_pending_submit_sessions`、`recover_missing_upload`、`stop_missing_upload`、`recover_session_uploads`、`discard_empty_upload_session`、`get_missing_upload_attempts` |
| `crates/biliup-cli/src/server/common/missing_segment.rs` | 缺失分段的补传状态机：入队、按状态计数与 stale-uploading 健康快照、后台自愈租约（先取消进程内 attempt 再落库）与重试延迟。⚠️ `upload_missing_segment` 是**补救账本**而非全量分段账本——走正常上传路径的会话在表中没有任何行，「无行」是健康状态；查询前先读 [`scripts/README.md`](./scripts/README.md#consistency-auditsh)。 | `missing_segment_health`、`recover_stale_upload_attempts`、`start_stale_attempt_recovery`、`enqueue_pending_segment` |

## 上传核心

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `crates/biliup/src/uploader/line.rs` | 定义 B 站上传线路、健康过滤后的自动探测、pre-upload 与服务端确认分块进度。 | `Line`、`Probe::probe_excluding`、`Parcel::upload_with_observer`、`UploadProgress` |
| `crates/biliup/src/uploader/line/upos.rs` | upos 协议的分块 PUT 实现：并发窗口、单请求超时与有限重试（每次失败带线路/分块号/耗时的结构化日志），最后汇总分片完成上传。 | `Upos::upload_stream`、`CHUNK_REQUEST_TIMEOUT`、`Upos::get_ret_video_info` |

## 仓库维护

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `scripts/check_code_index.py` | 对本索引做结构校验，防止失效路径、重复条目和悬空关系逐渐累积。 | `main` |
| `scripts/dev.sh` | 本机开发环境启动脚本：按需构建前端产物与后端二进制，可选带起 Next.js 热重载，绑 127.0.0.1 起服务。 | — |
| `scripts/normalization-disk-sample.py` | 采样响度标准化中间件的数量与字节峰值并判定是否超过上限；只读，直播中可跑。 | `scan`、`Peaks` |
| `scripts/structured_logging/evidence.py` | 有界只读双源证据导出与确定性校验：固定高水位分批、原生/桥接分列、旧文件代次与字节边界、批内一致匿名映射，manifest 记录不完整原因。 | `export`、`validate`、`Bundle`、`Budget`、`readonly` |
| `scripts/structured_logging/recording_pilot.py` | P3/12 的受控录制演练：本地回环发合成 FLV（含人工注入的 DTS 倒退与中途截断），跑真实 Python 下载入口，核对身份链与分段/断连事件，并生成证据包请求与预期事实清单。 | `inject_dts_backward`、`serve`、`check`、`expectations` |
| `scripts/structured_logging/reconcile.py` | 按证据包生成互不可见的双源分析视图、校验报告引用合法性，并单列桥接文本传输结论（永不为原生覆盖计分）。 | `prepare`、`cross`、`check_report`、`bridge_transport` |
| `scripts/structured_logging/smoke_entries.py` | 手工触发的入口冒烟：5 个支持入口 × 采集关闭/开启/新库不可用三态加一次关闭回退，全程合成媒体与空业务库，不做账号或网络动作。 | `main`、`child` |
| `scripts/consistency-audit.sh` | 只读巡检：比对每个已投稿会话的 `videos_json` 与本地分段账本，按投稿意图是否为空切成 legacy/current 两组，找出重复进稿、上传未进稿和序号不连续三类错位。 | — |

## 高信号关系

- `crates/biliup-observability/src/capture.rs` → `crates/biliup-observability/src/runtime.rs`（`Emitter::submit`）：tracing 回调只交付已脱敏快照，SQL 不在采集线程执行。
- `crates/biliup-observability/src/runtime.rs` → `crates/biliup-observability/src/sqlite.rs`（`Consumer::write`）：宿主以 factory 选择独立 SQLite 消费者，只有事务提交后才更新健康高水位。
- `crates/biliup-observability/examples/acceptance.rs` → `crates/biliup-observability/src/sqlite.rs`（`Repository::query`）：隔离负载完成后按游标检查持久化完整性与查询预算。
- `crates/stream-gears/src/server.rs` → `crates/biliup-observability/src/shadow.rs`（`Shadow::from_env`、`shadow::block_on`）：wheel server 入口只组合一个 subscriber，旧 sink 走 per-layer 过滤，新采集默认关闭。
- `crates/biliup-cli/src/main.rs` → `crates/biliup-observability/src/shadow.rs`（`Shadow::layer`）：Rust CLI 同样以旁路层接入，宿主已装 subscriber 时不 panic 也不替换。
- `crates/stream-gears/src/lib.rs` → `crates/biliup-observability/src/shadow.rs`（`inherited_dispatch`、`block_on_inherited`）：Python 局部宿主路径在保留原有 subscriber 的前提下补一条采集链。
- `scripts/structured_logging/reconcile.py` → `scripts/structured_logging/evidence.py`（`validate`）：组视图与交叉包前先做确定性校验，校验失败的包不进入比较。
- `biliup/__main__.py` → `crates/stream-gears/src/lib.rs`（`main_loop`）：Python 模块入口进入 Rust 扩展的 CLI 主循环。
- `crates/stream-gears/src/lib.rs` → `crates/stream-gears/src/server.rs`（`server::_main`）：PyO3 主循环规范化参数后委派实际 CLI 执行。
- `crates/biliup-cli/src/main.rs` → `crates/biliup-cli/src/lib.rs`（`run`）：`server` 子命令进入 Web 服务启动编排。
- `crates/biliup-cli/src/lib.rs` → `crates/biliup-cli/src/server/infrastructure/connection_pool.rs`（`ConnectionManager::new_pool`）：服务启动时创建业务 SQLite 连接池并运行迁移。
- `crates/biliup-cli/src/lib.rs` → `crates/biliup-cli/src/server/app.rs`（`ApplicationController::serve`）：服务依赖与主播任务恢复完成后创建并运行 HTTP 应用。
- `crates/biliup-cli/src/server/app.rs` → `crates/biliup-cli/src/server/api/ws.rs`（`ws_logs`）：应用启动层单独挂载日志 WebSocket；检查认证边界时不能只读业务 router。
- `app/(app)/logviewer/page.tsx` → `crates/biliup-cli/src/server/api/ws.rs`（`/v1/ws/logs`）：文件 tab 以 `file` 参数订阅文本流，前端逐条追加显示。
- `app/ui/plugins/developer.tsx` → `crates/biliup-cli/src/server/api/endpoints.rs`（`loggers_level`、配置保存）：表单值保存后通过 reload handle 修改 tracing 过滤器。
- `crates/biliup-cli/src/server/common/upload.rs` → `crates/biliup-cli/src/server/common/upload_line_selection.rs`（`decide_upload_line`）：录制期上传、页面整场上传、静默补传和手动补传共用同一个线路决策，回退原因随决策一起写日志并返回给页面。
- `crates/biliup-cli/src/server/common/upload_line_selection.rs` → `crates/biliup-cli/src/server/common/upload_line_health.rs`（`cooling_lines`）：决策把持久冷却状态当作唯一的「这条线不能用」依据；成功、网络错误或传输阶段的 watchdog 超时反过来更新它。
- `crates/biliup-cli/src/server/common/missing_segment.rs` → `crates/biliup-cli/src/server/common/attempt_lease.rs`（`classify_stale_lease`）：收割循环与健康接口共用同一份分阶段判据，避免页面显示与后台行为不一致。
- `crates/biliup-cli/src/server/common/missing_segment.rs` → `crates/biliup-cli/src/server/common/upload.rs`（`cancel_registered_attempt`）：收割一条仍被本进程持有的租约时，先真正取消并等它退出，再 CAS 落库，杜绝幽灵上传。
- `crates/biliup-cli/src/server/common/recovery_scheduler.rs` → `crates/biliup-cli/src/server/common/upload.rs`（`claim_manual_recovery`）：主动扫描与按会话恢复复用手动补传的资格判定和 claim，只是把执行搬到后台任务。
- `crates/biliup-cli/src/server/common/submission_scheduler.rs` → `crates/biliup-cli/src/server/common/upload.rs`（`reconcile_session_submission`）：启动与周期扫描按数据库投稿意图选出到期会话，以有界并发唤醒同一协调器；跨事件/扫描去重仍由持久 submit claim 保证。
- `app/(app)/missing/page.tsx` → `crates/biliup-cli/src/server/api/endpoints.rs`（`get_missing_uploads`、`get_pending_submit_sessions`、`recover_session_uploads`、`discard_empty_upload_session`）：页面分别读取分段聚合视图和独立待投稿会话；投稿操作状态由后端给出，不确定 claim 不显示危险重试入口，严格零基线会话可逻辑终结而不删除历史身份。
- `crates/biliup-cli/src/server/api/stream_check.rs` → `crates/biliup-cli/src/server/core/download_manager.rs`（`check_room_now`）：接口把主动检查转交监控 Actor，摘队列与检查是同一步原子操作，避免和轮询同时检查同一个房间、把一场直播拉起两次录制。
- `crates/biliup-cli/src/server/core/monitor.rs` → `crates/biliup-cli/src/server/common/upload_session.rs`（`reusable_streamer_info`）：开播检测先用平台场次键找同场未 finalize 的 `streamer_info`，重启不再为同一场直播造出第二个身份。
- `crates/biliup-cli/src/server/common/upload.rs` → `crates/biliup-cli/src/server/common/upload_session.rs`（`session_submit_readiness`、`claim_complete_session`、`schedule_submit_retry`）：分段成功和多类结束事件只唤醒统一协调器；协调器按意图/退避预检并共用严格完整性 claim 闸门，明确失败释放 claim，不确定远端结果保留 claim。
- `crates/biliup-cli/src/server/common/download.rs` → `crates/biliup-cli/src/server/common/upload_session.rs`（`request_session_submit`）：尾段均已 durable enrollment 后先持久化「本场已关闭、最终必须投稿」，再关闭上传 channel，避免上传尾段期间退出时丢失目标状态。
- `crates/biliup-cli/src/server/common/segment_enrollment.rs` → `crates/biliup-cli/src/server/common/upload_session.rs`（`submit_claim_token`）：提交 claim 关闭 enrollment 写入窗口；迟到分段在 finalized 边界写入审计而不污染正在投稿的分 P 快照。
- `crates/biliup-cli/src/server/common/upload.rs` → `crates/biliup-cli/src/server/common/segment_enrollment.rs`（`rescan_local_valid_segments` → `enroll_validated_segment`）：本地补扫先只读验证候选，首条有效未知媒体才通过统一 enrollment 事务创建或复用会话；零有效候选不写会话表。
- `crates/biliup-cli/src/server/common/upload.rs` → `crates/biliup-cli/src/server/common/recovery_eligibility.rs`（`check_recovery_eligibility`）：补扫、静默补传和人工操作共用 finalized/source-missing/succeeded 的准入结果，防止 closed session 产生新任务。
- `crates/biliup-cli/src/server/common/lifecycle_backfill.rs` → `crates/biliup-cli/src/server/common/upload_session.rs`（`session_completeness`）：回填的目标就是让历史会话的账本能被严格完整性闸门判为完整，冲突行则以未知状态持续阻塞投稿。
- `crates/biliup-cli/src/server/common/upload.rs` → `crates/biliup/src/uploader/line.rs`（`Probe::probe_excluding`、`Parcel::upload_with_observer`）：恢复与自动模式把冷却线路排除在实际探测请求之外；上传返回的 `Video` 标题由上传文件名兜底，而喂进去的是响度标准化/时间戳修复的中间件，因此 `upload_single_file_with_repair` 必须用原始录像名覆盖分P标题。
- `crates/biliup-cli/src/server/api/endpoints.rs` → `crates/biliup-cli/src/server/common/upload_line_health.rs`（`get_upload_line_health`）：健康接口与缺失列表读取同一份持久冷却状态。
- `crates/biliup-cli/src/server/core/downloader/stream_gears.rs` → `crates/biliup/src/downloader/util.rs`（`LifecycleFile::with_owner`、`SegmentIdentity`）：服务端把房间/场次/attempt 身份交给文件层，关闭回调再把同一 `segment_id` 放进 `SegmentInfo` 交给上传管道；`recording.reconnected` 只在帧头读通后才发，退避阶段只存缺口。
- `crates/biliup-cli/src/server/common/download.rs` → `crates/biliup-cli/src/observe.rs`（`RecordingIdentity`、`segment_enrolled`）：录制循环与分段处理器共用同一身份对象，登记结果（成功/重复/outbox/finalized/源丢失）逐一映射为原生事件的 outcome/reason。
- `crates/biliup-cli/src/server/core/downloader/stream_gears.rs` → `crates/biliup/src/downloader/httpflv.rs`（`Connection::with_stall_timeout`、`read_frame`、`diagnostics`）：服务端录制走这条拉流路径；`read_frame` 的单次 chunk 读超时是「上游停发数据」的唯一检测手段，超时长度（`stream_stall_timeout_secs`，缺省 30s）直接决定断连后空等多久。调用方保留 `Connection` 所有权，下载返回后读 `diagnostics().silent_for` 存进 `StreamGapReport`。
- `crates/biliup-cli/src/server/core/downloader/stream_gears.rs` → `crates/biliup-cli/src/server/common/download.rs`（`DownloaderRuntime::take_last_gap`）：把「上游最后一个字节 → 判死」的静默时长传回重连循环，与 `check_elapsed + backoff` 合成三段式缺口（`event="stream_gap"`）并累加进 `estimated_missing`；只有 FLV 自研解析路径测得到，其余下载器退回旧口径。
- `crates/biliup-cli/src/server/common/download.rs` → `crates/biliup-cli/src/server/common/util.rs`（`FileValidator::validate`）：分段关闭后按同一份判据分流——`Invalid` 直接删除，`RecoverableShort` 仅在`preserve_recoverable_short_segments` 开启时进入合并管线，否则同样删除；进入管线后还要满足同组 `group.len() > 1` 才会 `merge_compatible_segments`，**单个短片段一律走 `defer_recovery_batch` 落库等待处理，不进成片**。
- `crates/biliup-cli/src/server/core/monitor.rs` → `crates/biliup-cli/src/server/common/download.rs`（`start_download_workflow`）：开播检测插入 `streamer_info` 后，以该行 id 作为本场 Context 身份进入录制与上传流水线。
- `crates/biliup-cli/src/server/core/monitor.rs` → `crates/biliup-cli/src/server/common/recording_lease.rs`（`admit_detected_session`）：直播检测在写入新场次和启动下载前读取活动租约；到期后只允许可证明匹配的持久 grace 场次。
- `crates/biliup-cli/src/server/common/download.rs` → `crates/biliup-cli/src/server/common/recording_lease.rs`（`complete_grace_session`）：确认下播并处理尾段后，先 CAS 到期暂停再决定是否把 Worker 放回轮询队列。
- `crates/biliup-cli/src/server/app.rs` → `crates/biliup-cli/src/server/common/recording_lease.rs`（`start_recording_lease_tasks`）：Web 服务同级运行五秒到期扫描和可靠通知扫描，并在 shutdown 时中止二者；通知投递复用全局配置的 `cookie_health_webhook`，没有独立的租约 webhook 字段，未配置时到期租约停在 `not_configured`。
- `app/ui/StreamerActions/RecordingLeaseModal.tsx` → `crates/biliup-cli/src/server/api/recording_lease.rs`（租约 mutation）：弹窗提交明确 UTC 时间、当前租约 id 和客户备注，后端返回权威状态与服务器时间。
- `crates/biliup/src/uploader/line.rs` → `crates/biliup/src/uploader/line/upos.rs`（`Upos::upload_stream`）：线路对象把实际分块传输委派给 upos 协议实现，观察者回调据此产生已确认字节进度。
- `crates/biliup-cli/src/server/common/upload.rs` → `crates/biliup-cli/src/server/common/audio_normalization.rs`、`timestamp_repair.rs`（`normalize_for_upload`、`normalize_timestamps`）：预处理顺序是先标准化、后时间戳检测，检测的对象是标准化产物而不是原片。标准化的测量遍已经完整 demux 过原片并顺带扫了时间戳，原片干净时 `upload_single_file_with_repair` 跳过对产物的整片扫描；诊断缺失或原片异常时照常走完整的检测/修复链路。
- `crates/biliup-cli/src/server/common/audio_normalization.rs`、`timestamp_repair.rs`、`download.rs` → `crates/biliup-cli/src/server/common/process_priority.rs`（`background`）：预处理与分段恢复合并的 ffmpeg 一律降到后台优先级；`core/downloader` 的录制进程和用户自定义 hook 刻意不走这里。
- `crates/biliup-cli/src/server/api/endpoints.rs` → `crates/biliup-cli/src/server/common/missing_segment.rs`（`missing_segment_health`）：健康接口按状态实时计数 + 识别尚未被 60 秒自愈周期收敛的 stale uploading 行，不必等后台任务打日志才能观测。
