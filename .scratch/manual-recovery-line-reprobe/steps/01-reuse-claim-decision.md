# 01 手动补传复用 claim 时的线路决策

Status: ready-for-human（代码已合并 `5d14bc27`，待生产验收）

- 改动：`crates/biliup-cli/src/server/common/upload.rs` 的 `initialize_upload_context`、
  `process_with_upload`、`run_claimed_recovery`。
- 验证：`cargo test -p biliup-cli -- upload` 全绿。`initialize_upload_context` 依赖 cookie 登录，
  未加离线单测；分支只是 `Option` 选择。
- 生产验收：AUTO 全线冷却时手选显式线路补传，日志 `upload attempt started line_source=manual`
  之后不再出现 `no upload line candidates remain after filtering`，而是进入分块上传。
