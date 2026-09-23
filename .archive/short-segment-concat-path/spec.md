# 短片段合并列表路径绝对化

来源：[dplei/biliup#69](https://github.com/dplei/biliup/issues/69)

## 现象

默认录像文件名包含 `HH:MM:SS`。当录像路径是相对路径时，短片段合并生成的 concat 列表文件
也使用相对路径；ffmpeg concat demuxer 会把列表文件名第一个 `:` 前的内容识别成 URL scheme，
导致列表中的绝对媒体路径被错误拼接，直接复制和逐文件 remux 后的 concat 都失败，批次最终
进入 `Deferred`。

## 根因

`merge_compatible_segments` 直接取首个片段的 `parent()` 作为工作目录。只有文件名的相对路径
得到空 parent，于是列表与产物路径保持相对；两个 concat 阶段又复用同一个 `list_path`，因此
都触发 ffmpeg 的 URL 解析歧义。

## 方案

在 `merge_compatible_segments` 的单一入口把片段 parent 解析为绝对工作目录，列表、直接复制产物、
remux 临时文件及最终产物继续沿用现有命名和清理逻辑。已有绝对片段路径保持原目录，相对片段
路径相对当前工作目录解析。

不修改 ffmpeg 参数，不增加新的恢复分支，也不处理已存在的 `Deferred` 批次。

## 验收

- 单元测试覆盖文件名含冒号的相对路径，断言工作目录为绝对路径。
- 单元测试覆盖绝对片段路径，断言工作目录保持为其 parent。
- `cargo test -p biliup-cli server::common::download::` 通过。
- `rustfmt --edition 2024 --check crates/biliup-cli/src/server/common/download.rs` 通过。
- 合并及部署后观察新的短片段合并，生产验证前保留在 `.scratch/`。

## 生产验证结论（2026-09-23）

多个兼容短片段 `short segment merge phase succeeded` → `segment validated and enrolled` → 上传 → 投稿成功；上线后无 `scheme:` 截断错误、无新增 `Deferred` 批次。结论已回贴 #69 并关闭 issue。
