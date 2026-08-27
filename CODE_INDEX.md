# Code Index

这是按实际代码探索路径增量维护的文件级导航，不追求一次性覆盖全仓。查代码时先按领域词、
路径片段或符号名查本页；未命中再使用代码图索引。维护规则见
[`docs/agents/code-index.md`](docs/agents/code-index.md)。

文件职责和跨文件关系刻意分开记录，避免摘要相互交织。

## 进程与语言入口

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `crates/biliup-cli/src/main.rs` | Rust CLI 二进制入口：初始化日志，解析命令并分派登录、上传、下载、Web 服务和封面预览等子命令。 | `main` |
| `crates/biliup-cli/src/lib.rs` | Web 服务启动与主播配置导入的编排层：建立 SQLite 连接、组装服务、恢复主播任务并启动 Axum。 | `run`、`import_config_streamers`、`import_database_streamers` |
| `crates/stream-gears/src/lib.rs` | Rust/Python 的 PyO3 边界，向 Python 暴露下载、上传、登录和 CLI 主循环。 | `stream_gears`、`main_loop`、`download_with_hook`、`upload` |
| `crates/stream-gears/src/server.rs` | Python wheel 的实际 CLI 分派与日志初始化入口，同时向 Python 暴露进程内共享配置。 | `_main`、`config_bindings`、`ConfigState`、`CONFIG` |
| `biliup/__main__.py` | `python -m biliup` 的极薄入口，把执行权交给扩展模块的 `main_loop`。 | `main` |

## Rust 核心库

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `crates/biliup/src/lib.rs` | `biliup` 核心 crate 的公共门面，并提供带指数退避和抖动的通用异步重试。 | `retry`、`retry_with_config` |
| `crates/danmaku/src/lib.rs` | 弹幕录制 crate 的公共门面，组织客户端、协议、消息和 XML 输出模块并重导出稳定 API。 | `DanmakuRecorder`、`RecorderConfig`、`create_platform`、`XmlWriter` |

## Web 前端入口

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `app/layout.tsx` | Next.js 根布局，设置中文页面语言、全局样式和应用级 `body` 容器。 | `RootLayout` |
| `app/(app)/page.tsx` | 应用根页面的路由入口，直接把用户重定向到直播管理页。 | `Home` |
| `app/(app)/missing/page.tsx` | 缺失补传控制页：轮询补传列表，展示 attempt 阶段/进度/线路健康/会话完整性与线路切换历史，并触发补传、换线重投、停止、删除与本场补扫。 | `MissingRecovery`、`AttemptHistoryPanel` |
| `app/lib/api-streamer.ts` | 前端统一的 fetch 封装与错误处理边界：401 跳登录，JSON 错误透传，HTML/空正文按状态码翻译成中文提示。 | `fetcher`、`sendRequest`、`handleResponse`、`describeError` |

## 录制调度

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `crates/biliup-cli/src/server/core/monitor.rs` | 轮询各房间开播状态，命中开播时按平台场次键复用或新建本场 `streamer_info`，并在下载许可下拉起录制流程。 | `live_request` |
| `crates/biliup-cli/src/server/common/download.rs` | 录制主流程与分段事件处理：拉流、断线重试、切片校验，并把有效分段登记后交给上传管道。 | `start_download_workflow`、`DownloadTask`、`SegmentEventProcessor` |

## Web 服务边界

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `crates/biliup-cli/src/server/app.rs` | 组装会话、认证、CORS、业务路由与静态回退，启动 Axum，并在退出信号后清理服务资源。 | `ApplicationController::serve`、`shutdown_signal` |
| `crates/biliup-cli/src/server/common/upload.rs` | 编排直播分段上传、缺失补传、attempt lease/watchdog（分阶段计时）、线路决策入口以及远端结果落库；补传拆成「同步 claim + 后台执行」两段。 | `process_with_upload`、`decide_upload_line`、`upload_enrolled_with_watchdog`、`claim_manual_recovery`、`run_claimed_recovery`、`stop_missing_segment_attempt` |
| `crates/biliup-cli/src/server/common/attempt_lease.rs` | 定义 attempt 的三个阶段与各自的收割判据，提供心跳/阶段落库和 `upload_attempt` 历史表的读写。 | `AttemptPhase`、`classify_stale_lease`、`preprocess_deadline`、`record_heartbeat`、`close_attempt_history` |
| `crates/biliup-cli/src/server/common/upload_line_selection.rs` | 全仓唯一的上传线路决策：纯函数规划（配置/手动优先、冷却回退、auto 兜底）加一步 probe 解析。 | `plan_upload_line`、`resolve_planned_line`、`LinePlan`、`LineSource`、`cooling_lines` |
| `crates/biliup-cli/src/server/common/recovery_scheduler.rs` | 到期补传的主动扫描循环与后台执行：按会话串行、按 `segment_order` 顺序领取，接口只负责 claim。 | `start_due_recovery_scan`、`recover_due_segments`、`spawn_claimed_recovery` |
| `crates/biliup-cli/src/server/common/segment_enrollment.rs` | 在有效媒体进入内存队列前原子登记 session/分段 identity，按场次键续接会话（时钟窗口只作缺键兜底），并在数据库不可用时写 fsync outbox。 | `enroll_validated_segment`、`find_or_create_session`、`import_outbox_once` |
| `crates/biliup-cli/src/server/common/recovery_eligibility.rs` | 统一补扫、静默恢复和人工恢复的只读资格判定，并负责把消失源文件收敛为终态、记录 finalized 审计。 | `check_recovery_eligibility`、`mark_source_missing`、`record_recovery_audit` |
| `crates/biliup-cli/src/server/common/upload_session.rs` | 维护投稿会话恢复（场次键优先、时钟窗口兜底），并以生命周期账本检查完整性、确定性重建分 P 和原子 claim/finalize。 | `SessionCompleteness`、`select_recovery_candidate`、`reusable_streamer_info`、`touch_session_activity`、`claim_complete_session` |
| `crates/biliup-cli/src/server/common/lifecycle_backfill.rs` | 把历史会话的 videos_json 与遗留 missing 行回填成 v2 生命周期账本，合并重复源、生成 legacy:// 基线并按会话断点续跑。 | `run_lifecycle_backfill`、`plan_session`、`BackfillPlan` |
| `crates/biliup-cli/src/server/common/upload_line_health.rs` | 分类上传网络错误并持久维护单线路失败次数、冷却和到期单探测租约。 | `UploadFailureKind`、`acquire_line`、`record_failure`、`record_success` |
| `crates/biliup-cli/src/server/router.rs` | 声明全部 v1 HTTP 路由与静态回退，是查「某个接口存不存在、挂在哪个 handler」的唯一入口。 | `router` |
| `crates/biliup-cli/src/server/api/endpoints.rs` | 实现 Web API 业务端点：缺失分段列表（含后端算好的「下次线路」）、补传/重投/停止/按会话恢复、attempt 历史与上传健康查询。补传类端点只 claim，不等上传。 | `get_missing_uploads`、`recover_missing_upload`、`stop_missing_upload`、`recover_session_uploads`、`get_missing_upload_attempts` |
| `crates/biliup-cli/src/server/common/missing_segment.rs` | 缺失分段的补传状态机：入队、按状态计数与 stale-uploading 健康快照、后台自愈租约（先取消进程内 attempt 再落库）与重试延迟。 | `missing_segment_health`、`recover_stale_upload_attempts`、`start_stale_attempt_recovery`、`enqueue_pending_segment` |

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

## 高信号关系

- `biliup/__main__.py` → `crates/stream-gears/src/lib.rs`（`main_loop`）：Python 模块入口进入 Rust 扩展的 CLI 主循环。
- `crates/stream-gears/src/lib.rs` → `crates/stream-gears/src/server.rs`（`server::_main`）：PyO3 主循环规范化参数后委派实际 CLI 执行。
- `crates/biliup-cli/src/main.rs` → `crates/biliup-cli/src/lib.rs`（`run`）：`server` 子命令进入 Web 服务启动编排。
- `crates/biliup-cli/src/lib.rs` → `crates/biliup-cli/src/server/app.rs`（`ApplicationController::serve`）：服务依赖与主播任务恢复完成后创建并运行 HTTP 应用。
- `crates/biliup-cli/src/server/common/upload.rs` → `crates/biliup-cli/src/server/common/upload_line_selection.rs`（`decide_upload_line`）：录制期上传、页面整场上传、静默补传和手动补传共用同一个线路决策，回退原因随决策一起写日志并返回给页面。
- `crates/biliup-cli/src/server/common/upload_line_selection.rs` → `crates/biliup-cli/src/server/common/upload_line_health.rs`（`cooling_lines`）：决策把持久冷却状态当作唯一的「这条线不能用」依据；成功、网络错误或传输阶段的 watchdog 超时反过来更新它。
- `crates/biliup-cli/src/server/common/missing_segment.rs` → `crates/biliup-cli/src/server/common/attempt_lease.rs`（`classify_stale_lease`）：收割循环与健康接口共用同一份分阶段判据，避免页面显示与后台行为不一致。
- `crates/biliup-cli/src/server/common/missing_segment.rs` → `crates/biliup-cli/src/server/common/upload.rs`（`cancel_registered_attempt`）：收割一条仍被本进程持有的租约时，先真正取消并等它退出，再 CAS 落库，杜绝幽灵上传。
- `crates/biliup-cli/src/server/common/recovery_scheduler.rs` → `crates/biliup-cli/src/server/common/upload.rs`（`claim_manual_recovery`）：主动扫描与按会话恢复复用手动补传的资格判定和 claim，只是把执行搬到后台任务。
- `crates/biliup-cli/src/server/core/monitor.rs` → `crates/biliup-cli/src/server/common/upload_session.rs`（`reusable_streamer_info`）：开播检测先用平台场次键找同场未 finalize 的 `streamer_info`，重启不再为同一场直播造出第二个身份。
- `crates/biliup-cli/src/server/common/upload.rs` → `crates/biliup-cli/src/server/common/upload_session.rs`（`claim_complete_session`）：正常下播与重启补提交共用严格完整性闸门，只有 claim 所有者才能构建 studio 和调用远端投稿。
- `crates/biliup-cli/src/server/common/segment_enrollment.rs` → `crates/biliup-cli/src/server/common/upload_session.rs`（`submit_claim_token`）：提交 claim 关闭 enrollment 写入窗口；迟到分段在 finalized 边界写入审计而不污染正在投稿的分 P 快照。
- `crates/biliup-cli/src/server/common/upload.rs` → `crates/biliup-cli/src/server/common/recovery_eligibility.rs`（`check_recovery_eligibility`）：补扫、静默补传和人工操作共用 finalized/source-missing/succeeded 的准入结果，防止 closed session 产生新任务。
- `crates/biliup-cli/src/server/common/lifecycle_backfill.rs` → `crates/biliup-cli/src/server/common/upload_session.rs`（`session_completeness`）：回填的目标就是让历史会话的账本能被严格完整性闸门判为完整，冲突行则以未知状态持续阻塞投稿。
- `crates/biliup-cli/src/server/common/upload.rs` → `crates/biliup/src/uploader/line.rs`（`Probe::probe_excluding`）：恢复与自动模式把冷却线路排除在实际探测请求之外。
- `crates/biliup-cli/src/server/api/endpoints.rs` → `crates/biliup-cli/src/server/common/upload_line_health.rs`（`get_upload_line_health`）：健康接口与缺失列表读取同一份持久冷却状态。
- `crates/biliup-cli/src/server/core/monitor.rs` → `crates/biliup-cli/src/server/common/download.rs`（`start_download_workflow`）：开播检测插入 `streamer_info` 后，以该行 id 作为本场 Context 身份进入录制与上传流水线。
- `crates/biliup/src/uploader/line.rs` → `crates/biliup/src/uploader/line/upos.rs`（`Upos::upload_stream`）：线路对象把实际分块传输委派给 upos 协议实现，观察者回调据此产生已确认字节进度。
- `app/(app)/missing/page.tsx` → `crates/biliup-cli/src/server/api/endpoints.rs`（`get_missing_uploads`）：补传页的列表、进度与「下次线路」全部来自该接口的聚合视图。
- `crates/biliup-cli/src/server/api/endpoints.rs` → `crates/biliup-cli/src/server/common/missing_segment.rs`（`missing_segment_health`）：健康接口按状态实时计数 + 识别尚未被 60 秒自愈周期收敛的 stale uploading 行，不必等后台任务打日志才能观测。
