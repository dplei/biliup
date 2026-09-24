# 01 文件名撞车时追加序号

Status: 实现完成，待合并

## 改哪里

[`util.rs`](../../../crates/biliup/src/downloader/util.rs) 的 `LifecycleFile::create`：
`<base>.<ext>` 或 `<base>.<ext>.part` 已存在时改用 `<base>-<n>.<ext>`，n 从 1 递增。

## 验证

- `recording_events::each_created_file_gets_its_own_identity_and_reports_it_on_close`：
  原断言 `first.original_file == second.original_file`（等于把覆盖当预期）改为不相等，
  并断言两个文件内容都在。改前红，改后绿。
- `cargo test -p biliup --test recording_events`、`-p biliup --lib downloader`、
  `-p biliup-cli --lib download` 全绿；clippy 警告数改前改后一致。

## 生产验收（合并发版后）

- 日志里不再出现 `merged compatible recoverable short segments` 的 `originals` 含重复路径。
- 出现 `Different h264 sequence header` 同秒切段时，能看到 `Save to <name>-1.flv.part`。
