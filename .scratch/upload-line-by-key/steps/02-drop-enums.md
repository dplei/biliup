# 02 · 删枚举

- `crates/biliup-cli/src/lib.rs`：删 `UploadLine` 枚举，`--line` 参数改 `Option<String>`；`upload_by_command` / `upload_by_config` / `append` 的 `line` 参数同步。
- `crates/biliup-cli/src/uploader.rs:569` match → `line.as_deref().and_then(explicit_upload_line)`，`None` 走 `Probe::probe`。
- `crates/stream-gears/src/uploader.rs`：删 `UploadLine` pyclass 与 `From`，Python 参数改 `Option<String>`；同步 `stream-gears` 的 `.pyi`（若有）。
- `crates/biliup-cli/src/server/api/endpoints.rs:681`、`upload.rs:3529`：改用字串 key。
