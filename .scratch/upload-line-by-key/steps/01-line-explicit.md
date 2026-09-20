# 01 · `Line::explicit`

- `crates/biliup/src/uploader/line.rs`：加 `Line::explicit(upcdn) -> Line`，`probe_url` 空串；删 `bldsa()`…`alia()` 15 个函数及 `Default for Line`（若 `..bldsa()` 只被它用）。
- `crates/biliup-cli/src/server/common/upload_line_selection.rs`：`explicit_upload_line(key)` → key 匹配 `^[a-z0-9]+$` 且不等于 `auto`（不分大小写）时 `Some(Line::explicit(key))`。
- 单测：三个旧 key 的 query 参数集合与旧值相等；`estx` 通过、`AUTO` / 空 / 带 `&` 的 key 拒绝。
- `probe_filter_tests` 里用 `bldsa()` 等构造的测试改用 `Line::explicit`。
