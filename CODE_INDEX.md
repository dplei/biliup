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

## Web 服务边界

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `crates/biliup-cli/src/server/app.rs` | 组装会话、认证、CORS、业务路由与静态回退，启动 Axum，并在退出信号后清理服务资源。 | `ApplicationController::serve`、`shutdown_signal` |
| `crates/biliup-cli/src/server/common/upload.rs` | 编排直播分段上传、缺失补传、attempt lease/watchdog、线路选择以及远端结果落库。 | `process_with_upload`、`select_recovery_line`、`upload_enrolled_with_watchdog`、`manual_recover_missing_segment` |
| `crates/biliup-cli/src/server/common/segment_enrollment.rs` | 在有效媒体进入内存队列前原子登记 session/分段 identity，并在数据库不可用时写 fsync outbox。 | `enroll_validated_segment`、`find_or_create_session`、`import_outbox_once` |
| `crates/biliup-cli/src/server/common/upload_session.rs` | 维护投稿会话恢复，并以生命周期账本检查完整性、确定性重建分 P 和原子 claim/finalize。 | `SessionCompleteness`、`claim_complete_session`、`mark_submitted` |
| `crates/biliup-cli/src/server/common/upload_line_health.rs` | 分类上传网络错误并持久维护单线路失败次数、冷却和到期单探测租约。 | `UploadFailureKind`、`acquire_line`、`record_failure`、`record_success` |
| `crates/biliup-cli/src/server/api/endpoints.rs` | 实现 Web API 业务端点，包括缺失分段列表/操作与上传健康状态查询。 | `get_missing_uploads`、`get_upload_line_health`、`recover_missing_upload` |

## 上传核心

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `crates/biliup/src/uploader/line.rs` | 定义 B 站上传线路、健康过滤后的自动探测、pre-upload 与服务端确认分块进度。 | `Line`、`Probe::probe_excluding`、`Parcel::upload_with_observer`、`UploadProgress` |

## 仓库维护

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `scripts/check_code_index.py` | 对本索引做结构校验，防止失效路径、重复条目和悬空关系逐渐累积。 | `main` |

## 高信号关系

- `biliup/__main__.py` → `crates/stream-gears/src/lib.rs`（`main_loop`）：Python 模块入口进入 Rust 扩展的 CLI 主循环。
- `crates/stream-gears/src/lib.rs` → `crates/stream-gears/src/server.rs`（`server::_main`）：PyO3 主循环规范化参数后委派实际 CLI 执行。
- `crates/biliup-cli/src/main.rs` → `crates/biliup-cli/src/lib.rs`（`run`）：`server` 子命令进入 Web 服务启动编排。
- `crates/biliup-cli/src/lib.rs` → `crates/biliup-cli/src/server/app.rs`（`ApplicationController::serve`）：服务依赖与主播任务恢复完成后创建并运行 HTTP 应用。
- `crates/biliup-cli/src/server/common/upload.rs` → `crates/biliup-cli/src/server/common/upload_line_health.rs`（`select_recovery_line`）：所有服务端上传入口在 claim 前选择健康线路，并在成功、网络错误或 watchdog 超时后更新持久健康状态。
- `crates/biliup-cli/src/server/common/upload.rs` → `crates/biliup-cli/src/server/common/upload_session.rs`（`claim_complete_session`）：正常下播与重启补提交共用严格完整性闸门，只有 claim 所有者才能构建 studio 和调用远端投稿。
- `crates/biliup-cli/src/server/common/segment_enrollment.rs` → `crates/biliup-cli/src/server/common/upload_session.rs`（`submit_claim_token`）：提交 claim 关闭 enrollment 写入窗口，迟到分段持久转入 outbox 而不污染正在投稿的分 P 快照。
- `crates/biliup-cli/src/server/common/upload.rs` → `crates/biliup/src/uploader/line.rs`（`Probe::probe_excluding`）：恢复与自动模式把冷却线路排除在实际探测请求之外。
- `crates/biliup-cli/src/server/api/endpoints.rs` → `crates/biliup-cli/src/server/common/upload_line_health.rs`（`get_upload_line_health`）：健康接口与缺失列表读取同一份持久冷却状态。
