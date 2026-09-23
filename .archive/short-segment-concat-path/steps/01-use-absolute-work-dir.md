# 01 使用绝对工作目录运行短片段合并

Status: resolved

文件：`crates/biliup-cli/src/server/common/download.rs`

## 实现

- 新增 `recovery_work_dir`，用当前工作目录补全相对片段的 parent；绝对片段路径保持不变。
- `merge_compatible_segments` 只在入口调用一次，后续两个 concat 阶段共用绝对 `list_path`。
- 添加相对路径含冒号和绝对路径两种回归断言。

## 验证

- `cargo test -p biliup-cli server::common::download::`：24 passed。
- `rustfmt --edition 2024 --check crates/biliup-cli/src/server/common/download.rs`：通过。
- `python3 scripts/check_code_index.py`：通过（119 files，63 relationships）。
- `git diff --check`：通过。

## Comments

- 未处理历史 `Deferred` 批次；本 step 只阻止新合并继续因路径解析失败。
- 全仓 `cargo fmt --all -- --check` 仍会报告多处与本 issue 无关的既有格式差异，本次不扩大改动。
