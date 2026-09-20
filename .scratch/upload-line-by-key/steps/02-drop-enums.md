# 02 · 删枚举

- `crates/biliup-cli/src/lib.rs`：删 `UploadLine` 枚举，`--line` 参数改 `Option<String>`；`upload_by_command` / `upload_by_config` / `append` 的 `line` 参数同步。
- `crates/biliup-cli/src/uploader.rs:569` match → `line.as_deref().and_then(explicit_upload_line)`，`None` 走 `Probe::probe`。
- `crates/stream-gears/src/uploader.rs`：删 `UploadLine` pyclass 与 `From`，Python 参数改 `Option<String>`；同步 `stream-gears` 的 `.pyi`（若有）。
- `crates/biliup-cli/src/server/api/endpoints.rs:681`、`upload.rs:3529`：改用字串 key。

## 完成（2026-09-20）

- 两个 `UploadLine` 枚举、`key()`、`From` 与 `m.add_class` 全删；`--line` / Python `line` / `config.line`
  统一为 `Option<String>`，`.pyi` 与 `test_upload.py` 注释同步。
- 独立 CLI 路径：`line.as_deref().and_then(explicit_upload_line)`，畸形值打一条 warn 后走自动探测，
  与旧的 `from_str(..).ok()` 静默降级语义一致但有日志。
- `post_uploads` 不再把 `config.lines` 重复当 `forced` 传给 `decide_upload_line`——它本来就是
  `configured` 输入；重复传只会把来源误标成 `Manual`。行为不变，`LineSource` 标签更真实。
- `cargo test --workspace` 全绿（含 `standalone_upload_events` 集成测试改为 `Some("bda2")`）。
