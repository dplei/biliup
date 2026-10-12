# Code Index

这是 Agent 接到新需求时的入口导航。先按下面的业务词选择一个领域，只读相关子索引和源码，
不要默认加载全部索引。文件职责只在所属领域维护一份；跨领域关系按起点归属。

## 按需求定位

| 需求 / 检索词 | 领域索引 | 首要入口 |
| --- | --- | --- |
| CLI、启动、Python、PyO3、wheel | [进程与语言入口](docs/code-index/entrypoints.md) | `main`、`run`、`main_loop` |
| 开播检测、录制、下载器、FLV、HLS、弹幕、切片、断连、租约 | [录制与直播检测](docs/code-index/recording.md) | `start_monitor`、`start_download_workflow`、`DownloaderRuntime` |
| 上传、投稿、分P、补传、恢复、线路、限流、attempt、会话 | [上传、投稿与补传](docs/code-index/upload.md) | `process_with_upload`、`reconcile_session_submission`、`recover_due_segments` |
| 响度、音量、loudnorm、时间戳修复、ffmpeg、磁盘水位 | [媒体预处理](docs/code-index/processing.md) | `normalize_for_upload`、`normalize_timestamps`、`run_scanning_stderr` |
| 页面、导航、直播卡片、配置表单、房间覆写、上传线路选项 | [Web 前端与配置表单](docs/code-index/frontend.md) | `app/(app)`、`OverrideModal`、`useUploadLines` |
| API、路由、认证、全局配置、SQLite、migration、Cookie 健康 | [Web 服务、配置与业务存储](docs/code-index/server.md) | `ApplicationController::serve`、`router`、`Config`、`ConnectionManager` |
| 日志、事件、身份、脱敏、观测、事件数据库、SSE、日志页面 | [日志与观测](docs/code-index/observability.md) | `observe.rs`、`Emitter::submit`、`Runtime`、`useLogEventFeed` |
| 本地启动、演练、验收、证据导出、巡检、索引校验 | [开发工具与验证](docs/code-index/maintenance.md) | `scripts/dev.sh`、`scripts/structured_logging/`、`check_code_index.py` |

## 检索顺序

1. 用需求中的业务词选择上表领域；子索引提供仓库相对路径、关键符号和高信号关系。
2. 先读入口和相关关系，再用 `codebase-memory-mcp` 确认调用者、下游或影响面：
   `search_graph` 找概念，`search_code` 找已知符号，`query_graph` / `trace_path` 查关系，
   `get_code_snippet` 读已定位实现。项目名为 `Users-leii-Code-record-biliup`。
3. 修改前核对当前源码；索引没有条目不代表代码不存在，图中的边也不能替代对动态分派、
   异步消息和跨语言边界的实际阅读。精确字面量、刚修改的符号或代码图返回空时回退 `rg`。
4. 变更后只同步受影响的领域条目与关系，再运行 `python3 scripts/check_code_index.py`。

## 容易混淆的入口

- 录制时媒体时间轴：读录制领域的 `httpflv.rs` / `util.rs`；上传前修复：读预处理领域的 `timestamp_repair.rs`。
- 拉流线路切换：录制领域的 `route_health.rs`；上传线路健康与限流：上传领域的 `upload_line_health.rs` / `upload_rate_gate.rs`。
- 业务 SQLite 连接池：服务领域；事件 SQLite 及采集生命周期：日志领域。
- 页面入口与表单：前端领域；日志页面及其 API：日志领域；端点背后的上传状态机：上传领域。

索引按实际探索增量维护，不追求全仓覆盖。详细格式和维护边界见
[代码索引维护约定](docs/agents/code-index.md)。
