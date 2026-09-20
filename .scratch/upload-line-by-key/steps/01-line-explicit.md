# 01 · `Line::explicit`

- `crates/biliup/src/uploader/line.rs`：加 `Line::explicit(upcdn) -> Line`，`probe_url` 空串；删 `bldsa()`…`alia()` 15 个函数及 `Default for Line`（若 `..bldsa()` 只被它用）。
- `crates/biliup-cli/src/server/common/upload_line_selection.rs`：`explicit_upload_line(key)` → key 匹配 `^[a-z0-9]+$` 且不等于 `auto`（不分大小写）时 `Some(Line::explicit(key))`。
- 单测：三个旧 key 的 query 参数集合与旧值相等；`estx` 通过、`AUTO` / 空 / 带 `&` 的 key 拒绝。
- `probe_filter_tests` 里用 `bldsa()` 等构造的测试改用 `Line::explicit`。

## 完成（2026-09-20）

- `Line::explicit(upcdn)` 落在 `line.rs`；15 个构造函数与 `Default for Line`（唯一用户是 `..bldsa()`）已删。
- `explicit_upload_line` 改为形状校验（非空、仅 `[a-z0-9]`、不等于 `auto`）+ `Line::explicit`。
  `no-such-line` 因含 `-` 仍降级 auto，原测试语义保留（改名为 `a_malformed_…`）；新增
  `an_unregistered_but_well_formed_key_is_an_explicit_line`（`estx`/`akbd` 通过，六种畸形值拒绝）。
- `uploader.rs` 的 15 行 match 先桥接为 `Line::explicit(line.key())`，枚举本身留给 step 02。
- `upos_recovery_round_trip_by_line`（ignored）改用 `Line::explicit`，名单不变。
- 测试：`line` 过滤 16 + 39 + 1 + 2 通过；`upload_line_selection` 8 通过。
