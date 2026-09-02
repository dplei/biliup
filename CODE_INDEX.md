# Code Index

这是按实际代码探索路径增量维护的文件级导航，不追求一次性覆盖全仓。查代码时先按领域词、
路径片段或符号名查本页；未命中再使用代码图索引。维护规则见
[`docs/agents/code-index.md`](docs/agents/code-index.md)。

文件职责和跨文件关系刻意分开记录，避免摘要相互交织。

## 进程与语言入口

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `crates/biliup-cli/src/main.rs` | Rust CLI 二进制入口：初始化日志，解析命令并分派登录、上传、下载、Web 服务和封面预览等子命令。 | `main` |
| `crates/biliup-cli/src/observe.rs` | 业务原生事件的唯一发射点：`RecordingIdentity`/`UploadIdentity`/`SubmissionIdentity` 显式携带房间、场次、分段、attempt 与投稿会话身份（不建 span，避免改变旧输出），按契约发录制、预处理、上传、补传、投稿事件；需要附件的直接 emitter 通过 identity 的 owned `Context` 复用同一身份，空字符串表示「调用方没有这个身份」。 | `RecordingIdentity`、`UploadIdentity`、`UploadIdentity::from_missing_row`、`UploadIdentity::with_attempt`、`UploadIdentity::context`、`SubmissionIdentity`、`EVENT_TARGET`、`recording_started`、`segment_enrolled`、`processing_decided`、`upload_started`、`upload_failed`、`recovery_decided`、`submission_decided`、`submission_completed` |
| `crates/biliup-cli/examples/upload_pilot.rs` | P3/13 的受控后处理演练：本地 sqlite 跑真实登记、投稿判定与补传资格判定，两个 sink 同时输出，并生成证据包请求与预期事实清单。无账号、无网络。 | `drill`、`write_evidence_request` |
| `crates/biliup-cli/src/uploader.rs` | 独立 CLI 上传、配置逐稿投稿、追加与登录工具；保留 checkpoint 和限流锁，上传调用显式传 task，预上传重试分别分配 attempt。 | `upload_by_command`、`upload_by_config`、`append`、`upload_with_task`、`UploadCheckpoint` |
| `crates/biliup-cli/src/observe/standalone.rs` | 独立上传的观测上下文与结果分类：输入序号只在 task 内有效，不补造录制账本身份，不用请求错误推断投稿失败或成功。 | `UploadTask`、`UploadTask::file`、`UploadTask::submit`、`submission_result`、`failure_reason` |
| `crates/biliup-cli/src/observe/external.rs` | 外部命令失败的有界诊断：`processing.command_failed` 经**本次调用的采集器**直接写出（附件走不了 tracing 字段），只取当前 dispatch 上的采集器，不搜索全局运行；stderr 尾部作附件按 event_uid 关联，事件字段不复制第三方输出。也为 danmaku/封面/非命令 hook 等没有附件的失败发 `recording.auxiliary_failed` / `processing.auxiliary_failed`，只接受稳定 reason。 | `command_failed`、`auxiliary_failed` |
| `crates/biliup-cli/src/observe/audit.rs` | 把持久 `upload_recovery_audit` 行投影成 `audit.operation_projected`：复用业务行的稳定 event uid，按 durable reason 冻结映射 outcome/reason，显式携带恢复身份且路径只取 basename；业务审计仍是权威。 | `operation_projected`、`classify` |
| `crates/biliup-cli/src/observe/lifecycle.rs` | 入口一次运行的启停：两个 CLI 是一个进程、Python 绑定是一次嵌入调用；运行自带 id，与其中的录制/上传 task 是两个身份不互相代用；正常/出错/被取消分别记 executed、failed、unknown，强杀不执行析构因而没有结束事件、不补造。 | `Invocation`、`Invocation::start`、`Invocation::finish`、`run`、`command_name` |
| `crates/biliup-cli/src/observe/auth.rs` | 凭据健康与认证操作失败：健康跃迁只能由 `cookie_health` 状态机发出，单次操作失败不改变健康状态；错误按既有分类器定型后随即丢弃文本，按 `Debug` 而非 `Display` 渲染以免 `Report` 只暴露顶层 context。 | `health_changed`、`operation_failed`、`observe`、`reason_of`、`reason_for_error` |
| `crates/biliup-cli/src/server/core/live.rs` | 把平台提取出的 `LiveStream` 转成服务端 Worker/下载运行时/弹幕客户端；downloader hint 或显式配置可实际选择 StreamGears、FFmpeg、Streamlink、YtDlp/YtArchive，不可再把后两类当作死分支移出覆盖分母。 | `live_streamer`、`streamer_info`、`downloader_runtime`、`streamlink_runtime`、`ytdlp_runtime`、`danmaku_client`（需传入本场录制身份） |
| `crates/biliup-cli/src/server/core/downloader/ffmpeg_downloader.rs` | 服务端 FFmpeg 下载器（内部/外部分段）：外部分段的目标文件本进程选定，创建与关闭都是真实观测；内部分段只能在收到分段列表行时分配身份，故只发关闭事件。关闭原因跟随进程结束方式，取消标记先于杀进程写入。`spawn_log` 在保留旧 `[ffmpeg]` 逐行输出的同时并行做有界 stderr 采集。分段列表行只有 basename，按 `output_dir` 还原；单段收尾失败或重名不结束整场录制。 | `FfmpegDownloader`、`download_external`、`download_internal`、`external_close_reason`、`internal_close_reason`、`report_command_failure`、`spawn_log`、`FfmpegDownloader::stop` |
| `crates/biliup-cli/src/server/core/downloader/streamlink.rs` | 服务端 Streamlink 子进程下载器：目标 `.part` 由本进程 `--output` 选定，创建与关闭都是真实观测并带稳定 S/DA；关闭原因跟随进程结束方式，取消标记先于杀进程写入。退出后没有 `.part` 或改名失败如实记一次失败的关闭。`spawn_log` 在保留旧 `[streamlink]` 逐行输出的同时并行做有界 stderr 采集。 | `Streamlink`、`StreamlinkDownloader`、`build_file_args`、`close_reason`、`should_report_failure`、`report_command_failure`、`spawn_log` |
| `crates/biliup-cli/src/server/core/downloader/ytdlp.rs` | 服务端 YtDlp/YtArchive 运行时：组装外部命令、可并发抓封面、搬运最终产物。文件由外部工具创建/命名/搬运，创建时刻不可观测，故只发 `segment_closed`；录制身份只在 `download()` 由运行时配置填入。失败正文不再整段回灌自由错误，改为 `failure_summary` 的有界脱敏摘要，另发 spawn/退出诊断。`stop()` 只置标记不杀进程是既有语义。 | `YouTubeDownloader`、`DownloadConfig`、`run_ytdlp`、`run_ytarchive`、`output_path`、`bounded_output`、`failure_summary`、`report_command_failure` |
| `crates/stream-gears/src/uploader.rs` | Python 上传参数到上传/封面/投稿的编排，显式传递同一 task，复用服务端线路及持久限流准入。 | `StudioPre`、`upload`、`UploadLine` |
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
| `crates/biliup/src/downloader/hls.rs` | HLS 主/媒体列表下载与 TS 文件切分：保留序号 0、过滤重复片段、按小数时长切片、ENDLIST 终止及空直播列表等待；以文件身份记录序列缺口/不连续和 typed 错误，只在收到非空媒体后通知 ready。 | `download`、`download_with_ready`、`download_inner`、`boundary`、`TsFile` |
| `crates/biliup/tests/recording_events.rs` | 原生录制事件回归：文件创建/关闭身份与旧 target 隔离；回环 HLS 验证序号 0、重复/缺口、不连续、小数时长分段、空列表及错误返回。 | `HlsFixture`、`hls_zero_repeat_gap_and_discontinuity_preserve_file_identity` |
| `crates/biliup/src/downloader/flv_writer.rs` | 录制落盘的 FLV 写入器：写 FLV 头、按 tag 原样写回时间戳（**不做任何偏移重基**，CDN 给什么就写什么），分段时刷盘换文件并交给 `LifecycleFile` 收尾。 | `FlvFile`、`FlvFile::write_tag`、`FlvFile::write_tag_header`、`FlvFile::create_new`、`FLV_HEADER` |
| `crates/biliup/src/downloader/util.rs` | 分段判据 `Segmentable`（时间/大小任一超限即切）与录制文件生命周期 `LifecycleFile`；时间判据用媒体时间戳做 `current - start` 的**饱和减法**，时间戳倒退时 elapsed 归零。**文件创建时分配稳定 `SegmentIdentity`**（关闭回调与登记账本都用它）。`recording.segment_created/closed` 的发射抽成公开函数，外部进程产出文件的下载器共用同一套字段。 | `Segmentable`、`Segmentable::needed`、`elapsed_time`、`set_time_position`、`set_start_time`、`LifecycleFile`、`LifecycleFile::create`、`SegmentIdentity`、`RecordingOwner`、`allocate_id`、`segment_created`、`segment_closed`、`segment_close_failed`、`close_reason_code`、`SegmentCloseReason` |
| `crates/biliup/src/downloader/live/bilibili.rs` | B 站直播提取与选流：解析真实房间信息、API 或 master m3u8 候选，按配置选 CDN，并可在录制前探测失败后回退同协议候选；选流返回最终 URL 与候选实际 `qn`。 | `Bilibili`、`BilibiliLive::check_stream`、`get_stream_candidates`、`select_stream_url`、`BiliStreamCandidate`、`parse_codec_urls`、`parse_master_m3u8` |
| `crates/biliup/src/downloader/live/douyin.rs` | 抖音选流：解析房间信息后枚举各档位候选（含每档 `bitrate` 元数据），按请求档位就近选档并下发 flv/hls 地址与鉴权头；档位仅按名称在 `QUALITY_CODES` 内上下查找，**不参考码率**。 | `Douyin`、`DouyinLive::check_stream`、`select_quality_code`、`QUALITY_CODES`、`build_stream_candidates` |
| `crates/danmaku/src/lib.rs` | 弹幕录制 crate 的公共门面，组织客户端、协议、消息和 XML 输出模块并重导出稳定 API，含后台任务的终止观察类型。 | `DanmakuRecorder`、`RecorderConfig`、`RecorderExit`、`RecorderFailure`、`ExitObserver`、`create_platform`、`XmlWriter` |
| `crates/danmaku/src/client.rs` | 弹幕录制生命周期：连接/轮询循环、重连、命令处理与 XML 滚动保存。`start()` 只 spawn 后台任务并立刻返回句柄，因此终止由宿主注入的观察回调上报——上报守卫默认 `Aborted`，panic 与被取消也算数；失败按错误类型映射成稳定原因码，不解析错误文本。未注入观察者时行为与从前一致。已知缺陷：YouTube 轮询分支单次出错即终止（WS/TCP 分支有 30s 重连），`RecorderHandle::stop()` 吞掉发送错误。 | `DanmakuRecorder::start`、`with_exit_observer`、`RecorderExit`、`RecorderFailure::of`、`RecorderHandle::rolling`、`roll_writer` |

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
| `app/ui/OverrideModal.tsx` | 主播级「配置覆写」弹窗：顶部 JSON 文本框与各分区控件合成同一份 `override`，提交时控件值覆盖文本框的同名键。`entityFields` 里的键是 livestreamers 表上的真实列，不进 override。音量一组由「为这个房间单独设置音量」独占，与全局同值且原先未覆写的项不写入，override 保持最小。Form 带 `key`，每次打开重建，否则 Semi 保留的折叠面板不会重新应用 initValues。 | `OverrideModal`、`handleOk`、`AudioOverrideSection`、`CoverSection`、`AUDIO_OVERRIDE_FIELDS`、`AUDIO_OVERRIDE_TOGGLE` |
| `app/ui/AudioNormalizationControl.tsx` | 响度标准化的表单控件，空间配置页与主播覆写弹窗共用同一套界面：开关、磁盘保留线、保留原片、竖向音量推子，以及基于 WebAudio 增益的样片试听。样片全局唯一，覆写弹窗传 `showSample={false}` 隐藏其更新/删除按钮。 | `AudioNormalizationControl`、`prepareAudio`、`STATUS_URL`、`SAMPLE_URL` |
| `app/lib/api-streamer.ts` | 前端统一的 fetch 封装与错误处理边界：401 跳登录，JSON 错误透传，HTML/空正文按状态码翻译成中文提示。 | `fetcher`、`sendRequest`、`handleResponse`、`describeError` |

## 日志与存储基础设施

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `crates/biliup-observability/src/lib.rs` | 独立结构化日志 crate 门面，不依赖 Web 服务或业务数据库；目前以默认关闭的旁路接入各入口，旧文件与旧页面仍是权威来源。 | `Runtime`、`Emitter`、`CaptureLayer` |
| `crates/biliup-observability/src/model.rs` | 定义 v1 不可变事件、稳定/运行身份、显式上下文与扁平字段允许列表，区分原生与桥接来源；允许键是唯一入口，未列出的键计入 rejected。 | `Event`、`EventData`、`Fields`、`Context`、`Draft`、`field_kind` |
| `crates/biliup-observability/src/capture.rs` | 从父子 span、延迟 record 和事件字段构建快照，以独立层过滤控制原生/桥接采集并排除内部 SQL。 | `CaptureLayer`、`legacy_output` |
| `crates/biliup-observability/src/sanitize.rs` | 对允许字段执行有界格式化、敏感线索与旧链完整 ResponseData 整值脱敏和控制字符处理，不序列化未知 Debug 对象。 | `clean`、`Bounded`、`debug` |
| `crates/biliup-observability/src/diagnostic.rs` | 流式捕获外部诊断：跨 chunk 按有界行脱敏，保留首致命摘要、有限尾部和原始字节/截断信息。 | `DiagnosticCapture`、`Diagnostic` |
| `crates/biliup-observability/src/runtime.rs` | 提供条数/字节双限队列、重要级别预留、可替换后台消费者、提交后高水位、独立健康与限时关闭；身份感知 factory 在 consumer 重建时复用同一运行身份。 | `Runtime`、`Runtime::start_with_identity`、`Emitter`、`Consumer`、`Health`、`Options` |
| `crates/biliup-observability/src/sqlite.rs` | 独立 SQLite migrations 与每 Runtime 私有连接；共享库中的 writer 按运行身份注册、续租、关闭和一次性标记心跳中断，事件/附件仍走幂等事务，并保留只读游标查询、保留/WAL/低盘保护和一致性备份。Repository 在事件页的同一只读事务快照中聚合当前活跃/状态未知 writer；查询支持级别/分类的精确集合与 `newest_first` 倒序，集合大小在 `push_filters` 里统一设界，`count` 与分页受同一约束。 | `SqliteStore`、`StoreOptions`、`Repository`、`Query`、`Page`、`MIGRATOR`、`touch_writer`、`reap_stale_writers`、`push_filters` |
| `crates/biliup-observability/src/shadow.rs` | 把独立采集以可关闭旁路接进各入口：启动时读环境开关、同库重叠调用共享 run（全部 guard 退出后的顺序调用新建 run）、把 runtime worker 绑定到同一 dispatch，并保留嵌入宿主已有 subscriber。 | `Shadow`、`Config::from_env`、`block_on_inherited`、`health_snapshot`、`Inherited` |
| `crates/biliup-observability/examples/shadow_acceptance.rs` | 新旧双路同时开启的合成负载入口：同时测量发射延迟、排空、两侧丢弃与新旧合计磁盘占用，并产出可导出的证据请求。 | `main`、`ticks` |
| `crates/biliup-observability/examples/acceptance.rs` | 隔离的日志预算验收入口：以合成双路事件测量发射、调度延迟、排空、分页和磁盘占用，不启动业务服务。 | `main`、`workload` |
| `crates/biliup-cli/src/server/api/log_events.rs` | 独立事件库的只读入口：按级别/分类/时间/关键词/关联 ID/采集种类分页查询（**默认只回原生事件**，桥接须显式请求），附件详情、SSE 实时接续（只发已提交事件，与列表共用入库序号游标）与 JSONL/CSV 流式导出；列表同时返回历史未知窗口累计与当前活跃/状态未知 writer 数，采集关闭或库不可用时这些数值归零并以 `availability` 区分。`levels`/`categories` 是精确集合（`min_level` 之外另给一条路），`order=desc` 让页面从最新一条开始读、用 `next_until_id` 往回翻，实时与导出恒为正序。 | `list_log_events`、`stream_log_events`、`export_log_events`、`get_log_event_diagnostic`、`Availability`、`ListResponse`、`ListParams`、`set` |
| `crates/biliup-cli/src/server/api/ws.rs` | 基于文件的实时日志 WebSocket：白名单逻辑文件名映射到最新滚动文件，初连取末尾记录，再轮询新增内容并发送保活。 | `ws_logs`、`websocket_logs`、`resolve_latest_log`、`send_last_lines` |
| `app/(app)/logviewer/page.tsx` | 日志入口的默认页：按 `LOG_EVENTS_IS_DEFAULT` 决定渲染旧文件日志页还是新事件页，本身不含页面逻辑。P4/17 只改开关即可切换与回退。 | `LogViewerPage` |
| `app/(app)/log-events/page.tsx` | 新事件页的固定地址，开关怎么改都能从这里打开。 | `LogEventsPage` |
| `app/(app)/logviewer/legacy/page.tsx` | 旧文件日志页的固定地址，新页成为默认后仍可看原始文件与静态下载。 | `LegacyLogViewerPage` |
| `app/ui/logviewer/LegacyLogViewer.tsx` | 按日志文件切换 tab 的实时日志页（原 `logviewer/page.tsx` 逐字移出）：WebSocket 追加文本、滚动/连接状态、静态文件下载，另标注「数据来源：旧日志文件」。 | `LegacyLogViewer`、`LogContent` |
| `app/lib/log-view-config.ts` | 默认日志入口开关与两个导航条目（谁是默认、旧页去哪），被布局与三个路由共用。 | `LOG_EVENTS_IS_DEFAULT`、`LOG_NAV_ENTRIES`、`LOG_NAV_ROUTES`、`LEGACY_LOG_HREF` |
| `app/lib/log-events.ts` | 事件库前端契约：`StoredEvent`/`ListResponse`（含 writer 健康计数）类型、级别与分类中文表、字段名表、原生覆盖范围说明、筛选到查询参数的转换与列表/实时/导出 URL。**页面不解析日志字符串，级别与结果一律取结构化字段。** | `EventFilters`、`ListResponse`、`filterParams`、`listUrl`、`streamUrl`、`exportUrl`、`storageTrouble`、`NATIVE_CATEGORIES` |
| `app/ui/logevents/LogEventsView.tsx` | 新事件页主体：视图切换、命中数与级别计数、可用性/保留期缺口/写入异常分别提示，按“未确认正常关闭或心跳中断”准确呈现 writer 健康，支持场次范围进出与阅读位置恢复、导出和暂停刷新。 | `LogEventsView`、`Body`、`describeConnection` |
| `app/ui/logevents/useLogEventFeed.ts` | 列表取数与实时接续：把 API writer 健康计数纳入 feed meta，倒序取最新一页、`until_id` 往回翻、SSE 从已见最大 id 续、冻结时新事件只进缓冲、有界缓存与请求取消。 | `useLogEventFeed`、`Feed`、`FeedMeta` |
| `app/ui/logevents/useUrlFilters.ts` | 页面状态与地址栏同步（History API，不用 `useSearchParams`，静态导出下不引入 Suspense 边界）。 | `useUrlState`、`decodeState`、`encodeState` |
| `app/ui/logevents/FilterBar.tsx` | 级别快速筛选（带其余条件下的命中数）、业务类型/主播/时间/关键词，以及折叠的来源、事件名、运行实例与关联字段。 | `FilterBar`、`StreamerOption` |
| `app/ui/logevents/EventRow.tsx` | 一条事件的行与行内详情：级别颜色/图标/文字、摘要、身份与技术字段分层、按需取原始诊断、场次/会话/分段跳转。 | `EventRow`、`Detail`、`RawDiagnostic` |
| `app/ui/logevents/ProgressView.tsx` | 运行进度：复用 `/v1/status`、`/v1/uploads/missing`、`/v1/uploads/sessions/pending` 三个业务快照，无已知总量只显示阶段，过期快照单独标注，补传入口共用带按钮外观的导航链接。 | `ProgressView`、`workerState`、`RecoveryLink` |
| `app/ui/logevents/ProgressView.module.css` | 进度页补传链接的局部主题样式，统一普通/已访问颜色，提供悬停、按下与键盘焦点反馈。 | `recoveryLink` |
| `app/ui/plugins/developer.tsx` | 开发者日志设置表单，编辑旧 `LOGGING` 配置及后端动态日志过滤使用的 `loggers_level`。 | `Developer` |
| `crates/biliup-cli/src/server/infrastructure/connection_pool.rs` | 创建固定 SQLite 类型的业务连接池（上限 2）并执行业务 migrations，另提供测试用迁移后临时库。 | `ConnectionPool`、`ConnectionManager::new_pool`、`test_support::migrated_pool` |

## 录制调度

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `crates/biliup-cli/src/server/core/monitor.rs` | 轮询各房间开播状态，命中开播时按平台场次键复用或新建本场 `streamer_info`，并在下载许可下拉起录制流程；单次检查抽成共用实现，供轮询与「主动检查」按钮各调一次。 | `start_monitor`、`check_room_once`、`check_now`、`CheckOutcome` |
| `crates/biliup-cli/src/server/common/download.rs` | 录制主流程与分段事件处理：拉流、断线重试、切片校验，把有效分段登记后交给上传管道，并在尾段 durable enrollment 后持久关闭会话投稿意图。 | `start_download_workflow`、`DownloadTask`、`SegmentEventProcessor`、`persist_closed_session_intents` |
| `crates/biliup-cli/src/server/core/downloader.rs` | 下载器类型、下载配置、统一运行时枚举、分段回调载体和服务端弹幕客户端。`from_type` 只直接构造 FFmpeg/StreamGears；Streamlink 与 YtDlp/YtArchive 由 `core/live.rs` 读取平台 runtime options 后显式构造，四者现在都填完整的 `SegmentInfo`（裸 `SegmentInfo::new` 只剩没有分段身份的场合）。`RustDanmakuClient` 持有本场录制身份并给 recorder 注入终止观察回调，把 `download()` 成功之后的死亡转成 `recording.auxiliary_failed`/`danmaku_runtime`。 | `DownloaderType`、`DownloaderRuntime`、`DownloadConfig`、`SegmentInfo`、`RustDanmakuClient::with_identity`、`danmaku_exit_reason_code`、`parse_duration` |
| `crates/biliup-cli/src/server/core/downloader/stream_gears.rs` | 服务端拉流执行器：已知 HLS 直接解析，其余探测 FLV 并回落 HLS；传递 R/DA 与关闭回调的 S。HLS 收到非空媒体后才记重连，取消先设置关闭原因再释放下载 future。 | `StreamGears`、`start_download`、`classify_download_error`、`classify_reqwest_error`、`hls_server_reconnect_requires_media_and_cancel_preserves_identity` |
| `crates/biliup-cli/src/server/common/util.rs` | 录像分段落盘后的有效性判据：容器探测、`HeaderOnly`（FLV ≤13 字节）与小于阈值的可恢复短分段分类，决定丢弃、入队合并还是登记上传。 | `FileValidator`、`MediaValidation`、`InvalidMediaReason`、`probe_flv` |
| `crates/biliup-cli/src/server/core/download_manager.rs` | 单平台下载编排的持有者：建 `Monitor` 与上传 Actor 池，并把房间增删、暂停入队/出队、主动检查转发给监控 Actor。 | `DownloadManager`、`add_room`、`make_waker`、`check_room_now` |
| `crates/biliup-cli/src/server/infrastructure/context.rs` | 持有单房间 Worker 的运行状态、画质与活动录制快照，并以 `Context` 传递本场 `streamer_info` 身份和业务依赖。 | `Context`、`Worker`、`ActiveRecordingSnapshot`、`WorkerStatus`、`Stage` |
| `crates/biliup-cli/src/server/infrastructure/models/live_streamer.rs` | 定义持久化直播间配置及其新增载荷；该模型的全字段更新语义要求独立运行状态不要混入主播配置。 | `LiveStreamer`、`InsertLiveStreamer` |
| `crates/biliup-cli/src/server/config.rs` | 全局配置结构体与其 `struct_patch` 派生的 `ConfigPatch`（房间级覆写的载体）：录制、分段、上传节流、响度标准化等所有可配项都在这里，各带 serde 默认值与取值区间校验。新增字段自动获得覆写能力，无需 migration。 | `Config`、`ConfigPatch`、`Config::apply`、`normalization_settings`、`effective_audio_target_lufs`、`validate_segment_limits`、`normalize_segment_limits` |

## Web 服务边界

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `crates/biliup-cli/src/server/app.rs` | 组装会话、认证、CORS、业务路由与静态回退，启动 Axum，并在退出信号后清理服务资源。 | `ApplicationController::serve`、`shutdown_signal` |
| `crates/biliup-cli/src/server/api/stream_check.rs` | 主动检查直播流的端点：把监控层的检查结论翻译成中文提示，正常状态回 200 + `outcome`，检查失败与建会话失败如实报错。 | `check_stream_now`、`CheckStreamResponse` |
| `crates/biliup-cli/src/server/common/cookie_health.rs` | 维护平台 Cookie 健康快照，并为钉钉、企微和通用 URL 提供共享 Webhook 告警分发；只有本文件的状态机能宣布异常/恢复，去抖窗口内的重复失败不计数因而也不发原生事件，时钟由参数传入以便验证窗口与阈值。 | `record_success`、`record_error`、`record_error_at`、`classify_error`、`notify_alert`、`snapshot` |
| `crates/biliup-cli/src/server/common/recording_lease.rs` | 实现录制租约的持久状态机、纯到期/准入决策、CAS 扫描、下播收敛和带 claim/退避的通知投递。 | `RecordingLease`、`due_action`、`admit_detected_session`、`complete_grace_session`、`scan_due_recording_leases` |
| `crates/biliup-cli/src/server/api/recording_lease.rs` | 提供租约创建/替换/清除与幂等录制状态接口，并将输入校验、乐观并发和到期恢复守卫映射为 400/404/409。 | `put_recording_lease`、`delete_recording_lease`、`put_recording_state` |
| `crates/biliup-cli/src/server/common/upload.rs` | 编排直播分段上传、会话级幂等投稿协调、缺失补传、attempt lease/watchdog（分阶段计时）、线路决策入口以及远端结果落库；分段成功会在事务外唤醒有 durable 投稿意图的父会话。 | `process_with_upload`、`reconcile_session_submission`、`spawn_session_submission`、`persist_segment`、`decide_upload_line`、`upload_enrolled_with_watchdog`、`claim_manual_recovery`、`run_claimed_recovery`、`stop_missing_segment_attempt`、`segment_part_title` |
| `crates/biliup-cli/src/server/common/attempt_lease.rs` | 定义 attempt 的三个阶段与各自的收割判据，提供心跳/阶段落库和 `upload_attempt` 历史表的读写。 | `AttemptPhase`、`classify_stale_lease`、`preprocess_deadline`、`record_heartbeat`、`close_attempt_history` |
| `crates/biliup-cli/src/server/common/upload_line_selection.rs` | 全仓唯一的上传线路决策：纯函数规划（配置/手动优先、冷却回退、auto 兜底）加一步 probe 解析。probe **优先只在 `RECOVERABLE_LINES` 里挑**——那是实测能凭原始 `X-Upos-Auth` 把源对象 GET 回来的线路，而「事后能取回原片」是删本地原片的前提；这些线路全不可用时放开限制重探并 warn，因为传不上去比失去取回通道更严重。显式配置的线路不受此影响。 | `plan_upload_line`、`resolve_planned_line`、`RECOVERABLE_LINES`、`LinePlan`、`LineSource`、`cooling_lines` |
| `crates/biliup-cli/src/server/common/recovery_scheduler.rs` | 到期补传的主动扫描循环与后台执行：按会话串行、按 `segment_order` 顺序领取，接口只负责 claim。 | `start_due_recovery_scan`、`recover_due_segments`、`spawn_claimed_recovery` |
| `crates/biliup-cli/src/server/common/submission_scheduler.rs` | 数据库驱动的待投稿会话启动/周期扫描：只选有持久投稿意图、无 claim 且已到期的会话，以有界并发唤醒统一协调器并分类记录结果。 | `start_submission_reconciliation_scan`、`scan_due_submissions`、`due_submission_session_ids` |
| `crates/biliup-cli/src/server/common/segment_enrollment.rs` | 在有效媒体进入内存队列前原子登记 session/分段 identity，按场次键续接会话（时钟窗口只作缺键兜底），并在数据库不可用时写 fsync outbox。后台 importer 启动时全量分页重放持久恢复审计，运行中重试最近窗口；事件库按稳定 uid 去重。 | `enroll_validated_segment`、`find_or_create_session`、`import_outbox_once`、`spawn_outbox_importer` |
| `crates/biliup-cli/src/server/common/recovery_eligibility.rs` | 统一补扫、静默恢复和人工恢复的只读资格判定，并负责把消失源文件收敛为终态、记录 finalized 审计。审计业务行先持久化稳定 event uid，提交后 best-effort 投影；有界重放会给遗留 NULL uid 回填后始终复用同一 uid，不承诺跨业务库/事件库原子性。 | `check_recovery_eligibility`、`mark_source_missing`、`record_recovery_audit`、`replay_recovery_audits` |
| `crates/biliup-cli/src/server/common/upload_session.rs` | 维护投稿会话恢复（场次键优先、时钟窗口兜底）、单调持久投稿意图与退避时间，并以生命周期账本检查完整性、确定性重建分 P 和原子 claim/finalize；关闭态零基线会话在同一事务中逻辑终结为 `discarded_empty`。 | `SessionCompleteness`、`request_session_submit`、`session_submit_readiness`、`discard_empty_session`、`schedule_submit_retry`、`select_recovery_candidate`、`reusable_streamer_info`、`touch_session_activity`、`claim_complete_session` |
| `crates/biliup-cli/src/server/common/lifecycle_backfill.rs` | 把历史会话的 videos_json 与遗留 missing 行回填成 v2 生命周期账本，合并重复源、生成 legacy:// 基线并按会话断点续跑。 | `run_lifecycle_backfill`、`plan_session`、`BackfillPlan` |
| `crates/biliup-cli/src/server/common/upload_line_health.rs` | 分类上传网络错误并持久维护单线路失败次数、冷却和到期单探测租约。 | `UploadFailureKind`、`acquire_line`、`record_failure`、`record_success` |
| `crates/biliup-cli/src/server/common/audio_normalization.rs` | 上传前的双遍 `loudnorm` 响度标准化：探测、测量（顺带做时间戳诊断）、`-c copy` 视频 + 重编音频的转码，产物过严格校验后**原子替换原片**（`keep_original` 可退回旧的临时件形态），前后各有一道磁盘水位，全局单并发，任一步失败一律降级直传原片；服务端上传构造的系统 runner 显式携带 `UploadIdentity`，命令失败附件可按分段/attempt 关联。时长一律用 `content_span` 算出的**内容跨度**（直录 FLV 的 `format.duration` 是末尾时间戳，不是时长）；校验连续以同一理由失败会熔断，本进程停止发起标准化。 | `normalize_for_upload`、`NormalizationOutcome`、`NormalizedForm`、`NormalizationSettings`、`DiskBudget`、`output_is_faithful`、`OutputRejection`、`RejectionStreak`、`content_span`、`parse_probe_output`、`AudioFfmpegRunner`、`SystemAudioFfmpeg::with_context`、`parse_loudnorm_measurement`、`NORMALIZE_SLOTS` |
| `crates/biliup-cli/src/server/common/timestamp_repair.rs` | 上传前的时间戳异常检测与单级修复：`-c copy` + `setts` bsf 把 packet 时间戳夹成单调递增，只动容器元数据、payload 不解码，成本是一次顺序读写。**动手前先过回退量闸门**——`max()` 的损害上限恰好等于回退量，超过 `MAX_REPAIRABLE_BACKWARD_MS` 或根本解析不出回退量就直接 `Unfixable`，因为那种输入被 clamp 之后是单调的、复检发现不了，但内容已被压成帧风暴。整段 x264 重编码已删除；修复以复检为准，检测/remux/复检的进程失败分别返回带稳定原因的 `Fallback` 并直传，只有确认无异常才返回 `Clean`；`Unfixable` 走原片直传 + 告警，本地不留档。 | `normalize_timestamps`、`RepairOutcome`、`RepairFallbackReason`、`Detection`、`MAX_REPAIRABLE_BACKWARD_MS`、`FfmpegRunner`、`SystemFfmpeg::with_context`、`SETTS_MONOTONIC` |
| `crates/biliup-cli/src/server/common/ffmpeg_scan.rs` | 全片扫描类 ffmpeg 调用的 stderr 消费：边读边匹配时间戳异常并保留 64KiB 业务解析尾部，同时独立维护有界脱敏诊断附件；spawn/read/wait/非零退出都按调用方固定 stage 发原生失败，只有系统给出时才带退出码。`ScanObserver` 可接调用方显式 owned context，直接 emitter 因而能复用上传身份，无身份时仍只带扫描文件名。命中的异常行顺带解出**最大单次回退量**（认 muxer 的两种措辞，解不出返回 `None`——调用方必须当作「未知」保守处理，不能当作 0），时间戳修复的闸门靠它决策。可选 tee 保留 remux/hook 原先继承的 stderr；read_until 的单行业务临时缓冲尚无硬限。 | `run_scanning_stderr`、`ScanObserver`、`ScanObserver::with_context`、`StderrScan`、`stderr_indicates_anomaly`、`parse_backward_ms` |
| `crates/biliup-cli/src/server/common/disk_space.rs` | 文件系统可用空间探测（unix `statvfs`，取非特权可用的 `f_bavail`）：响度标准化的准入与硬水位共用，探测不出一律返回 `None` 让调用方放行。 | `available_bytes` |
| `crates/biliup-cli/src/server/common/process_priority.rs` | 给上传前的 ffmpeg 预处理子进程设置后台 nice 与 IO 优先级，使其让路给网页请求；录制与用户 hook 不降级。 | `background`、`background_std` |
| `crates/biliup-cli/src/server/router.rs` | 组装主要 v1 业务 HTTP 路由和静态文件服务；认证与日志 WebSocket 另由应用启动层挂载。 | `router`、`static_file_router` |
| `crates/biliup-cli/src/server/api/endpoints.rs` | 实现 Web API 业务端点：缺失补传/待投稿五态与人工恢复、上传健康和页面整场上传；页面响应返回观测 task，跨后台上传/投稿传递，首文件主播反查只用于模板。分段补传只 claim，按会话恢复持久化授权后异步唤醒投稿协调器。 | `post_uploads`、`get_missing_uploads`、`get_pending_submit_sessions`、`recover_session_uploads`、`get_missing_upload_attempts` |
| `crates/biliup-cli/src/server/common/missing_segment.rs` | 缺失分段的补传状态机：入队、按状态计数与 stale-uploading 健康快照、后台自愈租约（先取消进程内 attempt 再落库）与重试延迟。⚠️ `upload_missing_segment` 是**补救账本**而非全量分段账本——走正常上传路径的会话在表中没有任何行，「无行」是健康状态；查询前先读 [`scripts/README.md`](./scripts/README.md#consistency-auditsh)。 | `missing_segment_health`、`recover_stale_upload_attempts`、`start_stale_attempt_recovery`、`enqueue_pending_segment` |

## 上传核心

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `crates/biliup/src/uploader/line.rs` | 定义 B 站上传线路、健康过滤后的自动探测、pre-upload 与服务端确认分块进度。`Parcel::recovery()` 取走本次 preupload 的 UPOS 取回描述符，必须在 `upload()` 消耗 parcel 之前调用。探测的候选过滤是纯函数 `retained_lines`：先剔冷却，再在 `allowed` 非空时限定子集。 | `Line`、`Probe::probe_excluding`、`Probe::probe_filtered_with_failures`、`retained_lines`、`Parcel::upload_with_observer`、`Parcel::recovery`、`UploadProgress` |
| `crates/biliup/src/uploader/line/upos.rs` | upos 协议的分块 PUT 实现：并发窗口、单请求超时与有限重试（每次失败带线路/分块号/耗时的结构化日志），最后汇总分片完成上传。另定义 `UposRecovery`（endpoint + upos_uri + auth）——上传完还能把源对象取回来所需的一切，**只能在 preupload 时拿到**，事后重新申请是 403。**含凭证，不得进日志/事件/告警。** 取回是否可行按线路而定：实测 tx/bda2/alia 的 GET 逐字节一致，bldsa 只给 HEAD 200、GET 403。 | `Upos::upload_stream`、`CHUNK_REQUEST_TIMEOUT`、`Upos::get_ret_video_info`、`UposRecovery`、`UposRecovery::object_url`、`Bucket::recovery` |

## 仓库维护

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `scripts/check_code_index.py` | 对本索引做结构校验，防止失效路径、重复条目和悬空关系逐渐累积。 | `main` |
| `scripts/structured_logging/check_diagnostic_classification.py` | 校验 P3/14 的机器分类目录：四个 Rust 运行时源码根里每个含 tracing 级别宏的文件必须恰好有一个默认处置，新增/删除文件会阻断；只做文件级漂移检测，不替代调用点语义复核。 | `main`、`scanned_files` |
| `.scratch/structured-logging/diagnostic-classification-v1.json` | P3/14 未迁移诊断的机器目录：按文件冻结 native/bridge/no-persistence/coverage-gap 默认处置，并另列有理由的明确不支持能力边界。人类可读语义与阶段结论见同目录 `diagnostic-classification.md`。 | `version`、`groups`、`explicitly_unsupported_boundaries` |
| `scripts/dev.sh` | 本机开发环境启动脚本：按需构建前端产物与后端二进制，可选带起 Next.js 热重载，绑 127.0.0.1 起服务。 | — |
| `scripts/timestamp_shift.py` | 算出把回退的时间戳「平移」接回去所需的 setts 参数并打印现成的 ffmpeg 命令：逐流找回退点、按典型帧间隔补偏移、多次回退累加。与服务端的 `max()` 夹取互补——夹取会把回退点之后的内容压成帧风暴（所以那边有 10 秒闸门），平移一帧不丢、只是文件变长，代价是本机才跑得起的分析。`segment-recover` skill 用它。 | `find_drops`、`expression`、`typical_delta` |
| `scripts/normalization-disk-sample.py` | 采样响度标准化中间件的数量与字节峰值并判定是否超过上限；只读，直播中可跑。 | `scan`、`Peaks` |
| `scripts/structured_logging/evidence.py` | 有界只读双源证据导出与确定性校验：固定高水位分批、原生/桥接分列、旧文件代次与字节边界、批内一致匿名映射，manifest 记录不完整原因。 | `export`、`validate`、`Bundle`、`Budget`、`readonly` |
| `scripts/structured_logging/recording_pilot.py` | P3/12 的受控录制演练：本地回环发合成 FLV（含人工注入的 DTS 倒退与中途截断），跑真实 Python 下载入口，核对身份链与分段/断连事件，并生成证据包请求与预期事实清单。 | `inject_dts_backward`、`serve`、`check`、`expectations` |
| `scripts/structured_logging/reconcile.py` | 按证据包生成互不可见的双源分析视图、校验报告引用合法性，并单列桥接文本传输结论（永不为原生覆盖计分）。 | `prepare`、`cross`、`check_report`、`bridge_transport` |
| `scripts/structured_logging/upload_entries.py` | 独立上传入口的无账号演练：缺凭据的命令/配置/追加与 Python 调用、三态、重复调用、关闭回退及双源证据导出，不冒充成功远端运行。 | `main`、`run`、`embedded` |
| `scripts/structured_logging/page_upload_entries.py` | 页面上传的隔离 Rust/wheel HTTP 演练：缺失/损坏凭据、并发请求 task 查询、采集三态与关闭回退，导出双源和分离视图，不执行远端上传。 | `main`、`run`、`call` |
| `scripts/structured_logging/hls_entries.py` | HLS 三下载入口受控演练：CLI 提取输出 fixture、真实回环 TS、主列表/序列缺口/不连续/坏列表/HTTP 错误，核对输出字节、task/DA/S、三态与回退并导出双源。不代表真实平台提取或服务整链验收。 | `main`、`run`、`embedded`、`verify_native` |
| `scripts/structured_logging/smoke_entries.py` | 手工触发的入口冒烟：5 个支持入口 × 采集关闭/开启/新库不可用三态加一次关闭回退，全程合成媒体与空业务库，不做账号或网络动作。 | `main`、`child` |
| `scripts/consistency-audit.sh` | 只读巡检：比对每个已投稿会话的 `videos_json` 与本地分段账本，按投稿意图是否为空切成 legacy/current 两组，找出重复进稿、上传未进稿和序号不连续三类错位。 | — |

## 高信号关系

- `crates/biliup-observability/src/capture.rs` → `crates/biliup-observability/src/runtime.rs`（`Emitter::submit`）：tracing 回调只交付已脱敏快照，SQL 不在采集线程执行。
- `crates/biliup-observability/src/runtime.rs` → `crates/biliup-observability/src/sqlite.rs`（`Runtime::start_with_identity`、`Consumer::write`）：宿主以 factory 选择 SQLite 消费者并把稳定运行身份带入每次重连；只有事务提交后才更新健康高水位。
- `crates/biliup-observability/examples/acceptance.rs` → `crates/biliup-observability/src/sqlite.rs`（`Repository::query`）：隔离负载完成后按游标检查持久化完整性与查询预算。
- `crates/stream-gears/src/server.rs` → `crates/biliup-observability/src/shadow.rs`（`Shadow::from_env`、`shadow::block_on`）：wheel server 入口只组合一个 subscriber，旧 sink 走 per-layer 过滤，新采集默认关闭。
- `crates/biliup-cli/src/main.rs` → `crates/biliup-observability/src/shadow.rs`（`Shadow::layer`）：Rust CLI 同样以旁路层接入，宿主已装 subscriber 时不 panic 也不替换。
- `crates/stream-gears/src/lib.rs` → `crates/biliup-observability/src/shadow.rs`（`inherited_dispatch`、`block_on_inherited`）：Python 局部宿主路径在保留原有 subscriber 的前提下补一条采集链。
- `scripts/structured_logging/reconcile.py` → `scripts/structured_logging/evidence.py`（`validate`）：组视图与交叉包前先做确定性校验，校验失败的包不进入比较。
- `scripts/structured_logging/check_diagnostic_classification.py` → `.scratch/structured-logging/diagnostic-classification-v1.json`：扫描实际 tracing 文件并与机器目录做集合等价校验；人类可读理由与阶段门槛见同目录 `diagnostic-classification.md`。
- `biliup/__main__.py` → `crates/stream-gears/src/lib.rs`（`main_loop`）：Python 模块入口进入 Rust 扩展的 CLI 主循环。
- `crates/stream-gears/src/lib.rs` → `crates/stream-gears/src/server.rs`（`server::_main`）：PyO3 主循环规范化参数后委派实际 CLI 执行。
- `crates/biliup-cli/src/main.rs` → `crates/biliup-cli/src/lib.rs`（`run`）：`server` 子命令进入 Web 服务启动编排。
- `crates/biliup-cli/src/main.rs`、`crates/stream-gears/src/server.rs`、`crates/stream-gears/src/lib.rs` → `crates/biliup-cli/src/observe/lifecycle.rs`（`run`、`Invocation`）：四个入口共用同一包装报告一次运行的启停；Rust CLI 有真实进程实跑证据，wheel 与 Python 入口目前只有编译核对。
- `crates/biliup-cli/src/server/common/cookie_health.rs`、`crates/biliup-cli/src/uploader.rs`、`crates/stream-gears/src/lib.rs` → `crates/biliup-cli/src/observe/auth.rs`（`health_changed`、`operation_failed`、`observe`）：健康跃迁只从状态机的两次转变发出，登录/续期/Python 登录辅助函数只报告单次失败，不改变健康状态。
- `crates/biliup-cli/src/lib.rs` → `crates/biliup-cli/src/server/infrastructure/connection_pool.rs`（`ConnectionManager::new_pool`）：服务启动时创建业务 SQLite 连接池并运行迁移。
- `crates/biliup-cli/src/lib.rs` → `crates/biliup-cli/src/server/app.rs`（`ApplicationController::serve`）：服务依赖与主播任务恢复完成后创建并运行 HTTP 应用。
- `crates/biliup-cli/src/server/app.rs` → `crates/biliup-cli/src/server/api/ws.rs`（`ws_logs`）：应用启动层单独挂载日志 WebSocket；检查认证边界时不能只读业务 router。
- `crates/biliup-cli/src/server/router.rs` → `crates/biliup-cli/src/server/api/log_events.rs`、`api/ws.rs`（`list_log_events`、`ws_logs`）：新老两个日志入口都挂在 router 内，`app.rs` 的 `login_required` 因此对两者同时生效；开启 `--auth` 时未登录一律 401，显式关闭认证的部署行为不变。
- `app/ui/logviewer/LegacyLogViewer.tsx` → `crates/biliup-cli/src/server/api/ws.rs`（`/v1/ws/logs`）：文件 tab 以 `file` 参数订阅文本流，前端逐条追加显示。
- `app/(app)/layout.tsx`、`app/(app)/logviewer/page.tsx` → `app/lib/log-view-config.ts`（`LOG_NAV_ENTRIES`、`LOG_EVENTS_IS_DEFAULT`）：导航条目与默认页由同一个开关决定，切换默认页不需要改路由或页面代码。
- `app/ui/logevents/useLogEventFeed.ts` → `crates/biliup-cli/src/server/api/log_events.rs`（`/v1/log-events`、`/stream`）：历史用 `order=desc` + `until_id` 往回翻，实时用同一套筛选从已见最大 id 接续，两边共用入库序号游标。
- `app/ui/logevents/ProgressView.tsx` → `crates/biliup-cli/src/server/api/endpoints.rs`（`get_status`、`get_missing_uploads`、`get_pending_submit_sessions`）：进度视图只读已有业务快照，不在日志页另建一套上传状态机。
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
- `crates/biliup-cli/src/server/common/recovery_eligibility.rs` → `crates/biliup-cli/src/observe/audit.rs` → 事件 SQLite（`record_recovery_audit`、`replay_recovery_audits`、`operation_projected`）：业务审计事务先保存稳定 uid，提交后立即投影；importer 启动全量、周期最近窗口重放同一 uid，事件库唯一约束收敛重复，业务审计不依赖事件留存。
- `crates/biliup-cli/src/server/common/lifecycle_backfill.rs` → `crates/biliup-cli/src/server/common/upload_session.rs`（`session_completeness`）：回填的目标就是让历史会话的账本能被严格完整性闸门判为完整，冲突行则以未知状态持续阻塞投稿。
- `crates/biliup-cli/src/server/common/upload.rs` → `crates/biliup/src/uploader/line.rs`（`Probe::probe_excluding`、`Parcel::upload_with_observer`、`Parcel::recovery`）：恢复与自动模式把冷却线路排除在实际探测请求之外；上传返回的 `Video` 标题由上传文件名兜底，而喂进去的是响度标准化/时间戳修复的中间件，因此 `upload_single_file_with_repair` 必须用原始录像名覆盖分P标题。另外 preupload 一成功就把 UPOS 取回描述符经 `UploadActivity::UposRecovery` 送到 watchdog 循环落库——拿到它的 `upload_single_file` 手上没有 `missing_id`，而 watchdog 两样都有；落库列是 `upload_missing_segment.upos_recovery_json`，明文 + 写入时顺带清理超 TTL 的行（migration 24）。
- `crates/biliup-cli/src/server/api/endpoints.rs` → `crates/biliup-cli/src/server/common/upload_line_health.rs`（`get_upload_line_health`）：健康接口与缺失列表读取同一份持久冷却状态。
- `crates/biliup-cli/src/server/core/downloader/ffmpeg_downloader.rs` → `crates/biliup/src/downloader/util.rs`、`crates/biliup-cli/src/observe/external.rs`（`segment_created`、`segment_closed`、`segment_close_failed`、`command_failed`）：外部进程产出的分段复用自研写入路径的同一套事件字段；非 0/255 退出且未取消时才把退出码与有界 stderr 作为外部命令失败上报。
- `crates/biliup-cli/src/server/core/downloader/stream_gears.rs` → `crates/biliup/src/downloader/util.rs`（`LifecycleFile::with_owner`、`SegmentIdentity`）：服务端把房间/场次/attempt 身份交给文件层，关闭回调再把同一 `segment_id` 放进 `SegmentInfo` 交给上传管道；FLV 有效帧头或 HLS 实收媒体才触发 reconnect，退避阶段只存缺口。
- `crates/biliup-cli/src/server/core/downloader/stream_gears.rs` → `crates/biliup/src/downloader/hls.rs`（`download_with_ready`）：已知 m3u8/ts 不读 FLV 头，非空媒体收到后通知已有重连上下文；没有 FLV gap 测量，序列缺口另以片段数量记录。
- `crates/biliup-cli/src/server/common/download.rs` → `crates/biliup-cli/src/observe.rs`（`RecordingIdentity`、`segment_enrolled`）：录制循环与分段处理器共用同一身份对象，登记结果（成功/重复/outbox/finalized/源丢失）逐一映射为原生事件的 outcome/reason。
- `crates/biliup/src/downloader/live/bilibili.rs` → `crates/biliup-cli/src/server/common/download.rs` → `crates/biliup-cli/src/server/api/endpoints.rs` → `app/(app)/streamers/page.tsx`（`BiliStreamCandidate::qn`、`LiveStream.recording_quality`、`Worker::set_recording_quality`、`LiveStreamerResponse.recording_quality`）：B 站候选的实际画质沿通用 Worker/API 状态链到主播卡片；通用链只透传，平台选流必须保留最终命中候选的 `qn`，页面负责中文档位映射。
- `crates/biliup-cli/src/server/core/downloader/stream_gears.rs` → `crates/biliup/src/downloader/httpflv.rs`（`Connection::with_stall_timeout`、`read_frame`、`diagnostics`）：服务端 FLV 录制走这条拉流路径；`read_frame` 的单次 chunk 读超时是「上游停发数据」的唯一检测手段，超时长度（`stream_stall_timeout_secs`，缺省 30s）直接决定断连后空等多久。调用方保留 `Connection` 所有权，下载返回后读 `diagnostics().silent_for` 存进 `StreamGapReport`。
- `crates/biliup-cli/src/server/core/downloader/stream_gears.rs` → `crates/biliup-cli/src/server/common/download.rs`（`DownloaderRuntime::take_last_gap`）：把「上游最后一个字节 → 判死」的静默时长传回重连循环，与 `check_elapsed + backoff` 合成三段式缺口（`event="stream_gap"`）并累加进 `estimated_missing`；只有 FLV 自研解析路径测得到，其余下载器退回旧口径。
- `crates/biliup-cli/src/server/common/download.rs` → `crates/biliup-cli/src/server/common/util.rs`（`FileValidator::validate`）：分段关闭后按同一份判据分流——`Invalid` 直接删除，`RecoverableShort` 仅在`preserve_recoverable_short_segments` 开启时进入合并管线，否则同样删除；进入管线后还要满足同组 `group.len() > 1` 才会 `merge_compatible_segments`，**单个短片段一律走 `defer_recovery_batch` 落库等待处理，不进成片**。
- `crates/biliup-cli/src/server/core/monitor.rs` → `crates/biliup-cli/src/server/common/download.rs`（`start_download_workflow`）：开播检测插入 `streamer_info` 后，以该行 id 作为本场 Context 身份进入录制与上传流水线。
- `crates/biliup-cli/src/server/core/live.rs` → `crates/biliup-cli/src/server/core/downloader.rs`、`downloader/streamlink.rs`、`downloader/ytdlp.rs`（`downloader_runtime`）：平台 hint/运行参数和显式配置共同决定真实运行时，不能沿用旧索引的“全部回落 StreamGears”判断；三种外部运行时现在都走 `biliup/src/downloader/util.rs` 与 `observe/external.rs` 的同一套分段与命令失败事件。
- `crates/biliup-cli/src/server/core/downloader.rs` → `crates/danmaku/src/client.rs`（`RustDanmakuClient::download` 注入 `with_exit_observer`）：后台弹幕任务的终止经回调回到 `observe/external.rs` 的辅助失败边界，与 `danmaku_start`/`stop`/`roll` 三个同步 stage 区分；`danmaku` crate 本身不依赖采集组件。
- `crates/biliup-cli/src/server/core/monitor.rs` → `crates/biliup-cli/src/server/common/recording_lease.rs`（`admit_detected_session`）：直播检测在写入新场次和启动下载前读取活动租约；到期后只允许可证明匹配的持久 grace 场次。
- `crates/biliup-cli/src/server/common/download.rs` → `crates/biliup-cli/src/server/common/recording_lease.rs`（`complete_grace_session`）：确认下播并处理尾段后，先 CAS 到期暂停再决定是否把 Worker 放回轮询队列。
- `crates/biliup-cli/src/server/app.rs` → `crates/biliup-cli/src/server/common/recording_lease.rs`（`start_recording_lease_tasks`）：Web 服务同级运行五秒到期扫描和可靠通知扫描，并在 shutdown 时中止二者；通知投递复用全局配置的 `cookie_health_webhook`，没有独立的租约 webhook 字段，未配置时到期租约停在 `not_configured`。
- `app/ui/OverrideModal.tsx` → `crates/biliup-cli/src/server/config.rs`（`ConfigPatch`、`Config::apply`）：弹窗写的 `override` JSON 落在 livestreamers 表的 `override` 列，反序列化成 `struct_patch` 生成的 `ConfigPatch`。`Config` 的裸 `bool`/数字在 patch 侧都是 `Option`，因此**「键不存在」才表示跟随全局**，写 `false` 是显式覆写成关闭——界面上要区分这两态，靠的是额外的「是否单独设置」开关而不是字段本身。整份 override 每次提交整体替换，没有增量合并。
- `crates/biliup-cli/src/server/infrastructure/context.rs`、`common/upload.rs` → `crates/biliup-cli/src/server/config.rs`（`Config::apply`）：房间级覆写在三处各自合并进生效配置——`ctx.config()`（录制与首传，`global_config()` 才是未合并的全局值）、投稿前的 `build_studio` 链路、以及补传/手动恢复。新增一个需要按房间覆写的配置项时这三处都已覆盖，不必再改后端。
- `app/ui/StreamerActions/RecordingLeaseModal.tsx` → `crates/biliup-cli/src/server/api/recording_lease.rs`（租约 mutation）：弹窗提交明确 UTC 时间、当前租约 id 和客户备注，后端返回权威状态与服务器时间。
- `crates/biliup/src/uploader/line.rs` → `crates/biliup/src/uploader/line/upos.rs`（`Upos::upload_stream`）：线路对象把实际分块传输委派给 upos 协议实现，观察者回调据此产生已确认字节进度。
- `crates/biliup-cli/src/server/common/upload.rs` → `crates/biliup-cli/src/server/common/audio_normalization.rs`、`timestamp_repair.rs`（`normalize_for_upload`、`normalize_timestamps`）：预处理顺序是先标准化、后时间戳检测，检测的对象是标准化产物而不是原片。标准化的测量遍已经完整 demux 过原片并顺带扫了时间戳，原片干净时 `upload_single_file_with_repair` 跳过对产物的整片扫描；诊断缺失或原片异常时照常走完整的检测/修复链路。两个系统 runner 都从本次 `UploadIdentity` 构造显式 context，`processing.command_failed` 与完成事件按同一分段/attempt 关联。
- `crates/biliup-cli/src/server/common/audio_normalization.rs`、`timestamp_repair.rs`、`download.rs` → `crates/biliup-cli/src/server/common/process_priority.rs`（`background`）：预处理与分段恢复合并的 ffmpeg 一律降到后台优先级；`core/downloader` 的录制进程和用户自定义 hook 刻意不走这里。
- `crates/biliup-cli/src/server/api/endpoints.rs` → `crates/biliup-cli/src/server/common/missing_segment.rs`（`missing_segment_health`）：健康接口按状态实时计数 + 识别尚未被 60 秒自愈周期收敛的 stale uploading 行，不必等后台任务打日志才能观测。

- `crates/biliup-cli/src/main.rs`、`crates/stream-gears/src/server.rs` → `crates/biliup-cli/src/uploader.rs`（`upload_by_command`、`upload_by_config`、`append`）：Rust/wheel CLI 复用同一独立上传链，task 在调用层分配并传至每次尝试。
- `crates/stream-gears/src/lib.rs` → `crates/stream-gears/src/uploader.rs`（`upload`）：Python 函数在局部 subscriber/runtime 内执行上传编排，每次调用持有独立 task。
- `crates/stream-gears/src/uploader.rs` → `crates/biliup-cli/src/server/common/upload.rs`（`upload_with_task`）：Python 传入 task 关联底层传输与后续投稿。
- `crates/biliup-cli/src/server/api/endpoints.rs` → `crates/biliup-cli/src/server/common/upload.rs`、`crates/biliup-cli/src/observe/standalone.rs`（`post_uploads`、`upload_with_task`、`UploadTask::submit`）：页面请求同一 task 进入后台传输与投稿，显式继承请求 subscriber；关联回归入口是 `page_upload_events` 集成测试。
- `crates/biliup-cli/src/uploader.rs`、`crates/stream-gears/src/uploader.rs` → `crates/biliup-cli/src/observe/standalone.rs`（`UploadTask`）：早退记录投稿未开始的原因，远端请求返回后单独区分成功、失败与不确定结果。
