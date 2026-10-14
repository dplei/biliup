# Web 服务、配置与业务存储

[返回根导航](../../CODE_INDEX.md) · [维护约定](../agents/code-index.md)

HTTP 路由、认证边界、业务端点、配置模型、业务 SQLite 连接池与 Cookie 健康。

**从这里开始：** 路由从 `app.rs`、`router.rs` 进入，业务端点看 `endpoints.rs`；配置以 `Config` / `ConfigPatch` 为入口。

表内路径均相对仓库根目录。职责与关系继承自已有索引；修改前仍需核对当前源码。

## 文件职责

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `crates/biliup-cli/src/server/infrastructure/connection_pool.rs` | 创建固定 SQLite 类型的业务连接池（上限 2）并执行业务 migrations，另提供测试用迁移后临时库。 | `ConnectionPool`、`ConnectionManager::new_pool`、`test_support::migrated_pool` |
| `crates/biliup-cli/src/server/infrastructure/models/live_streamer.rs` | 定义持久化直播间配置及其新增载荷；该模型的全字段更新语义要求独立运行状态不要混入主播配置。 | `LiveStreamer`、`InsertLiveStreamer` |
| `crates/biliup-cli/src/server/config.rs` | 全局配置结构体与其 `struct_patch` 派生的 `ConfigPatch`（房间级覆写的载体）：录制、分段、上传节流、响度标准化等所有可配项都在这里，各带 serde 默认值与取值区间校验。新增字段自动获得覆写能力，无需 migration。 | `Config`、`ConfigPatch`、`Config::apply`、`normalization_settings`、`effective_audio_target_lufs`、`validate_segment_limits`、`normalize_segment_limits` |
| `crates/biliup-cli/src/server/app.rs` | 组装会话、认证、CORS、业务路由与静态回退，启动 Axum，并在退出信号后清理服务资源。 | `ApplicationController::serve`、`shutdown_signal` |
| `crates/biliup-cli/src/server/common/cookie_health.rs` | 维护平台 Cookie 健康快照，并为钉钉、企微和通用 URL 提供共享 Webhook 告警分发；只有本文件的状态机能宣布异常/恢复，去抖窗口内的重复失败不计数因而也不发原生事件，时钟由参数传入以便验证窗口与阈值。 | `record_success`、`record_error`、`record_error_at`、`classify_error`、`notify_alert`、`snapshot` |
| `crates/biliup-cli/src/server/router.rs` | 组装主要 v1 业务 HTTP 路由和静态文件服务；认证与日志 WebSocket 另由应用启动层挂载。 | `router`、`static_file_router` |
| `crates/biliup-cli/src/server/api/endpoints.rs` | 实现 Web API 业务端点：缺失补传/待投稿五态与人工恢复、上传健康、B 站线路索引转发（`/v1/upload-lines`，拉不到时回 `IMPLICIT_FALLBACKS` 并标 `degraded`）和页面整场上传；页面响应返回观测 task，跨后台上传/投稿传递，首文件主播反查只用于模板。分段补传只 claim，按会话恢复持久化授权后异步唤醒投稿协调器。 | `post_uploads`、`get_missing_uploads`、`get_pending_submit_sessions`、`recover_session_uploads`、`get_missing_upload_attempts`、`get_upload_lines`、`UploadLines` |

## 高信号关系

- `crates/biliup-cli/src/server/app.rs` → `crates/biliup-cli/src/server/common/upload.rs`（`shutdown_session_submissions`）：信号后先关闭投稿准入并等待已准入协调收尾，再清理下载任务；关停产生的持久投稿意图留给重启扫描。

以下箭头包含控制流、数据流和跨语言边界，具体含义以边后的说明为准；不等同于完整函数调用图。

- `crates/biliup-cli/src/server/common/cookie_health.rs`、`crates/biliup-cli/src/uploader.rs`、`crates/stream-gears/src/lib.rs` → `crates/biliup-cli/src/observe/auth.rs`（`health_changed`、`operation_failed`、`observe`）：健康跃迁只从状态机的两次转变发出，登录/续期/Python 登录辅助函数只报告单次失败，不改变健康状态。
- `crates/biliup-cli/src/server/app.rs` → `crates/biliup-cli/src/server/api/ws.rs`（`ws_logs`）：应用启动层单独挂载日志 WebSocket；检查认证边界时不能只读业务 router。
- `crates/biliup-cli/src/server/router.rs` → `crates/biliup-cli/src/server/api/log_events.rs`、`crates/biliup-cli/src/server/api/ws.rs`（`list_log_events`、`ws_logs`）：新老两个日志入口都挂在 router 内，`app.rs` 的 `login_required` 因此对两者同时生效；开启 `--auth` 时未登录一律 401，显式关闭认证的部署行为不变。
- `crates/biliup-cli/src/server/api/endpoints.rs` → `crates/biliup-cli/src/server/common/upload_line_health.rs`（`get_upload_line_health`）：健康接口与缺失列表读取同一份持久冷却状态。
- `crates/biliup-cli/src/server/app.rs` → `crates/biliup-cli/src/server/common/recording_lease.rs`（`start_recording_lease_tasks`）：Web 服务同级运行五秒到期扫描和可靠通知扫描，并在 shutdown 时中止二者；通知投递复用全局配置的 `cookie_health_webhook`，没有独立的租约 webhook 字段，未配置时到期租约停在 `not_configured`。
- `crates/biliup-cli/src/server/api/endpoints.rs` → `crates/biliup-cli/src/server/common/missing_segment.rs`（`missing_segment_health`）：健康接口按 `status` 实时计数 + 识别尚未被 60 秒自愈周期收敛的 stale uploading 行，不以 `last_error` 是否非空判健康。
- `crates/biliup-cli/src/server/api/endpoints.rs` → `crates/biliup-cli/src/server/common/upload.rs`、`crates/biliup-cli/src/observe/standalone.rs`（`post_uploads`、`upload_with_task`、`UploadTask::submit`）：页面请求同一 task 进入后台传输与投稿，显式继承请求 subscriber；关联回归入口是 `page_upload_events` 集成测试。

## 跨领域继续阅读

- [进程与语言入口](entrypoints.md)
- [录制与直播检测](recording.md)
- [上传、投稿与补传](upload.md)
- [日志与观测](observability.md)
