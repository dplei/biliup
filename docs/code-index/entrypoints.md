# 进程与语言入口

[返回根导航](../../CODE_INDEX.md) · [维护约定](../agents/code-index.md)

CLI 分派、服务启动、Python / PyO3 桥接与核心库公共入口。

**从这里开始：** 先定位 `main`、`run`、`main_loop`；具体下载、上传和日志行为转到相应领域。

表内路径均相对仓库根目录。职责与关系继承自已有索引；修改前仍需核对当前源码。

## 文件职责

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `crates/biliup-cli/src/main.rs` | Rust CLI 二进制入口：初始化日志，解析命令并分派登录、上传、下载、Web 服务和封面预览等子命令。 | `main` |
| `crates/biliup-cli/src/lib.rs` | Web 服务启动与主播配置导入的编排层：建立 SQLite 连接、组装服务、恢复主播任务并启动 Axum。 | `run`、`import_config_streamers`、`import_database_streamers` |
| `crates/stream-gears/src/lib.rs` | Rust/Python 的 PyO3 边界，向 Python 暴露下载、上传、登录和 CLI 主循环；下载回调与上传函数各自安装局部控制台/文件日志订阅器。 | `stream_gears`、`main_loop`、`download_with_callback`、`download_with_hook`、`upload` |
| `crates/stream-gears/src/server.rs` | Python wheel 的实际 CLI 分派与日志初始化入口，同时向 Python 暴露进程内共享配置。 | `_main`、`config_bindings`、`ConfigState`、`CONFIG` |
| `biliup/__main__.py` | `python -m biliup` 的极薄入口，把执行权交给扩展模块的 `main_loop`。 | `main` |
| `crates/biliup/src/lib.rs` | `biliup` 核心 crate 的公共门面，并提供带指数退避和抖动的通用异步重试。 | `retry`、`retry_with_config` |

## 高信号关系

以下箭头包含控制流、数据流和跨语言边界，具体含义以边后的说明为准；不等同于完整函数调用图。

- `crates/stream-gears/src/server.rs` → `crates/biliup-observability/src/shadow.rs`（`Shadow::from_env`、`shadow::block_on`）：wheel server 入口只组合一个 subscriber，旧 sink 走 per-layer 过滤，新采集默认关闭。
- `crates/biliup-cli/src/main.rs` → `crates/biliup-observability/src/shadow.rs`（`Shadow::layer`）：Rust CLI 同样以旁路层接入，宿主已装 subscriber 时不 panic 也不替换。
- `crates/stream-gears/src/lib.rs` → `crates/biliup-observability/src/shadow.rs`（`inherited_dispatch`、`block_on_inherited`）：Python 局部宿主路径在保留原有 subscriber 的前提下补一条采集链。
- `biliup/__main__.py` → `crates/stream-gears/src/lib.rs`（`main_loop`）：Python 模块入口进入 Rust 扩展的 CLI 主循环。
- `crates/stream-gears/src/lib.rs` → `crates/stream-gears/src/server.rs`（`server::_main`）：PyO3 主循环规范化参数后委派实际 CLI 执行。
- `crates/biliup-cli/src/main.rs` → `crates/biliup-cli/src/lib.rs`（`run`）：`server` 子命令进入 Web 服务启动编排。
- `crates/biliup-cli/src/main.rs`、`crates/stream-gears/src/server.rs`、`crates/stream-gears/src/lib.rs` → `crates/biliup-cli/src/observe/lifecycle.rs`（`run`、`Invocation`）：四个入口共用同一包装报告一次运行的启停；Rust CLI 有真实进程实跑证据，wheel 与 Python 入口目前只有编译核对。
- `crates/biliup-cli/src/lib.rs` → `crates/biliup-cli/src/server/infrastructure/connection_pool.rs`（`ConnectionManager::new_pool`）：服务启动时创建业务 SQLite 连接池并运行迁移。
- `crates/biliup-cli/src/lib.rs` → `crates/biliup-cli/src/server/app.rs`（`ApplicationController::serve`）：服务依赖与主播任务恢复完成后创建并运行 HTTP 应用。
- `crates/biliup-cli/src/main.rs`、`crates/stream-gears/src/server.rs` → `crates/biliup-cli/src/uploader.rs`（`upload_by_command`、`upload_by_config`、`append`）：Rust/wheel CLI 复用同一独立上传链，task 在调用层分配并传至每次尝试。
- `crates/stream-gears/src/lib.rs` → `crates/stream-gears/src/uploader.rs`（`upload`）：Python 函数在局部 subscriber/runtime 内执行上传编排，每次调用持有独立 task。

## 跨领域继续阅读

- [上传、投稿与补传](upload.md)
- [Web 服务、配置与业务存储](server.md)
- [日志与观测](observability.md)
