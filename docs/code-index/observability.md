# 日志与观测

[返回根导航](../../CODE_INDEX.md) · [维护约定](../agents/code-index.md)

业务事件发射、身份传递、脱敏、采集、事件存储、日志 API 和日志页面。

**从这里开始：** 业务事件先看 `observe.rs`；采集入库看 `capture.rs` → `runtime.rs` → `sqlite.rs`；页面取数看 `useLogEventFeed.ts`。

表内路径均相对仓库根目录。职责与关系继承自已有索引；修改前仍需核对当前源码。

## 文件职责

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `crates/biliup-cli/src/observe.rs` | 业务原生事件的唯一发射点：`RecordingIdentity`/`UploadIdentity`/`SubmissionIdentity` 显式携带房间、场次、分段、attempt 与投稿会话身份（不建 span，避免改变旧输出），按契约发录制、预处理、上传、补传、投稿事件；需要附件的直接 emitter 通过 identity 的 owned `Context` 复用同一身份，空字符串表示「调用方没有这个身份」。 | `RecordingIdentity`、`UploadIdentity`、`UploadIdentity::from_missing_row`、`UploadIdentity::with_attempt`、`UploadIdentity::context`、`SubmissionIdentity`、`EVENT_TARGET`、`recording_started`、`segment_enrolled`、`processing_decided`、`upload_started`、`upload_failed`、`recovery_decided`、`submission_decided`、`submission_completed` |
| `crates/biliup-cli/src/observe/standalone.rs` | 独立上传的观测上下文与结果分类：输入序号只在 task 内有效，不补造录制账本身份，不用请求错误推断投稿失败或成功。 | `UploadTask`、`UploadTask::file`、`UploadTask::submit`、`submission_result`、`failure_reason` |
| `crates/biliup-cli/src/observe/external.rs` | 外部进程的有界诊断经**本次调用的采集器**直接写出（附件走不了 tracing 字段），只取当前 dispatch 上的采集器，不搜索全局运行：命令失败发 `processing.command_failed`；exit 0 但时间戳异常措辞无法解析时发 `processing.diagnostic_captured`，两者都把脱敏附件按 event_uid 关联而不复制第三方输出到字段。也为 danmaku/封面/非命令 hook 等没有附件的失败发 auxiliary 事件。 | `command_failed`、`timestamp_anomaly_unparsed`、`auxiliary_failed` |
| `crates/biliup-cli/src/observe/audit.rs` | 把持久 `upload_recovery_audit` 行投影成 `audit.operation_projected`：复用业务行的稳定 event uid，按 durable reason 冻结映射 outcome/reason，显式携带恢复身份且路径只取 basename；业务审计仍是权威。 | `operation_projected`、`classify` |
| `crates/biliup-cli/src/observe/lifecycle.rs` | 入口一次运行的启停：两个 CLI 是一个进程、Python 绑定是一次嵌入调用；运行自带 id，与其中的录制/上传 task 是两个身份不互相代用；正常/出错/被取消分别记 executed、failed、unknown，强杀不执行析构因而没有结束事件、不补造。 | `Invocation`、`Invocation::start`、`Invocation::finish`、`run`、`command_name` |
| `crates/biliup-cli/src/observe/auth.rs` | 凭据健康与认证操作失败：健康跃迁只能由 `cookie_health` 状态机发出，单次操作失败不改变健康状态；错误按既有分类器定型后随即丢弃文本，按 `Debug` 而非 `Display` 渲染以免 `Report` 只暴露顶层 context。 | `health_changed`、`operation_failed`、`observe`、`reason_of`、`reason_for_error` |
| `crates/biliup-observability/src/lib.rs` | 独立结构化日志 crate 门面，不依赖 Web 服务或业务数据库；目前以默认关闭的旁路接入各入口，旧文件与旧页面仍是权威来源。 | `Runtime`、`Emitter`、`CaptureLayer` |
| `crates/biliup-observability/src/model.rs` | 定义 v1 不可变事件、稳定/运行身份、显式上下文与扁平字段允许列表，区分原生与桥接来源；允许键是唯一入口，未列出的键计入 rejected。 | `Event`、`EventData`、`Fields`、`Context`、`Draft`、`field_kind` |
| `crates/biliup-observability/src/capture.rs` | 从父子 span、延迟 record 和事件字段构建快照，以独立层过滤控制原生/桥接采集并排除内部 SQL。 | `CaptureLayer`、`legacy_output` |
| `crates/biliup-observability/src/sanitize.rs` | 对允许字段执行有界格式化、敏感线索与旧链完整 ResponseData 整值脱敏和控制字符处理，不序列化未知 Debug 对象。 | `clean`、`Bounded`、`debug` |
| `crates/biliup-observability/src/diagnostic.rs` | 流式捕获外部诊断：跨 chunk 按有界行脱敏，保留首致命摘要、有限尾部和原始字节/截断信息。 | `DiagnosticCapture`、`Diagnostic` |
| `crates/biliup-observability/src/runtime.rs` | 提供条数/字节双限队列、重要级别预留、可替换后台消费者、提交后高水位、独立健康与限时关闭；身份感知 factory 在 consumer 重建时复用同一运行身份。 | `Runtime`、`Runtime::start_with_identity`、`Emitter`、`Consumer`、`Health`、`Options` |
| `crates/biliup-observability/src/sqlite.rs` | 独立 SQLite migrations 与每 Runtime 私有连接；同机多进程以 WAL 短事务共享库，writer 按运行身份注册、续租、关闭和一次性标记心跳中断，不支持网络盘或多机共享。事件/附件仍走幂等事务，并保留只读游标查询、保留/WAL/低盘保护和一致性备份。Repository 在事件页的同一只读事务快照中聚合当前活跃/状态未知 writer；查询支持级别/分类的精确集合与 `newest_first` 倒序，集合大小在 `push_filters` 里统一设界，`count` 与分页受同一约束。 | `SqliteStore`、`StoreOptions`、`Repository`、`Query`、`Page`、`MIGRATOR`、`touch_writer`、`reap_stale_writers`、`push_filters` |
| `crates/biliup-observability/tests/storage.rs` | SQLite 存储与恢复验收：覆盖运行身份/租约、迁移、预算、保留、备份、只读查询，以及 TempDir 内真实 A/B 子进程共享写入和强杀后的未知窗口；子进程健康回执同时约束额外 dropped/storage failure。 | `separate_process_writer_does_not_disturb_resident_writer`、`force_kill_retains_commit_and_reports_unclean_window` |
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
| `app/ui/logevents/ProgressView.tsx` | 运行进度：复用 `/v1/status`、`/v1/uploads/missing`、`/v1/uploads/sessions/pending` 三个业务快照，无已知总量只显示阶段，过期快照单独标注，`last_error` 只按错误诊断显示，补传入口共用带按钮外观的导航链接。 | `ProgressView`、`workerState`、`RecoveryLink` |
| `app/ui/logevents/ProgressView.module.css` | 进度页补传链接的局部主题样式，统一普通/已访问颜色，提供悬停、按下与键盘焦点反馈。 | `recoveryLink` |
| `app/ui/plugins/developer.tsx` | 开发者日志设置表单，编辑旧 `LOGGING` 配置及后端动态日志过滤使用的 `loggers_level`。 | `Developer` |

## 高信号关系

以下箭头包含控制流、数据流和跨语言边界，具体含义以边后的说明为准；不等同于完整函数调用图。

- `crates/biliup-observability/src/capture.rs` → `crates/biliup-observability/src/runtime.rs`（`Emitter::submit`）：tracing 回调只交付已脱敏快照，SQL 不在采集线程执行。
- `crates/biliup-observability/src/runtime.rs` → `crates/biliup-observability/src/sqlite.rs`（`Runtime::start_with_identity`、`Consumer::write`）：宿主以 factory 把稳定运行身份带入每次 SQLite 重连；同机多个 Runtime 通过各自的运行行和 WAL 短事务共享库，只有事件事务提交后才更新健康高水位。
- `crates/biliup-observability/examples/acceptance.rs` → `crates/biliup-observability/src/sqlite.rs`（`Repository::query`）：隔离负载完成后按游标检查持久化完整性与查询预算。
- `app/ui/logviewer/LegacyLogViewer.tsx` → `crates/biliup-cli/src/server/api/ws.rs`（`/v1/ws/logs`）：文件 tab 以 `file` 参数订阅文本流，前端逐条追加显示。
- `app/ui/logevents/useLogEventFeed.ts` → `crates/biliup-cli/src/server/api/log_events.rs`（`/v1/log-events`、`/stream`）：历史用 `order=desc` + `until_id` 往回翻，实时用同一套筛选从已见最大 id 接续，两边共用入库序号游标。
- `app/ui/logevents/ProgressView.tsx` → `crates/biliup-cli/src/server/api/endpoints.rs`（`get_status`、`get_missing_uploads`、`get_pending_submit_sessions`）：进度视图只读已有业务快照，不在日志页另建一套上传状态机。
- `app/ui/plugins/developer.tsx` → `crates/biliup-cli/src/server/api/endpoints.rs`（`loggers_level`、配置保存）：表单值保存后通过 reload handle 修改 tracing 过滤器。

## 跨领域继续阅读

- [Web 服务、配置与业务存储](server.md)
