# 01 公共投稿入口关闭准入与排空

Status: ready-for-human

## 范围

- `upload.rs`：统一协调入口读锁、关停写锁、明确关停返回值与本地回归。
- `app.rs`：信号后、下载清理前关闭准入并等待已准入协调。
- `submission_scheduler.rs`：关停结果归类 skipped。
- 更新领域索引；不恢复历史 claim，不修改远端投稿策略。

## 回执

实现已完成，待 PR 合并及真实环境验收。

- `cargo test -p biliup-cli --lib --tests`：501 项通过，12 项原有忽略，零失败（完整测试时包含首项新回归）。
- 补充真实协调器数据库等待测试后，`cargo test -p biliup-cli --lib shutdown`：两项关停回归通过。
- `rustfmt --edition 2024 --check`（三个修改的 Rust 文件）、`git diff --check`：通过。
- `python3 scripts/check_code_index.py`：124 个文件、72 条关系，校验通过。
- 回归使用本地迁移 SQLite 与虚构视频引用，无账号、无远端请求。

## 验收

- 关停时下载收尾仍持久化投稿意图，协调返回 ShuttingDown，不写新 claim。
- 已进入协调的任务收尾后服务才退出；重启扫描接手未取得 claim 的意图。
- 正常下播、分段成功与周期扫描的投稿行为保持原状。

## Comments

历史悬挂 claim 须先人工确认远端结果，本次不自动清除。
