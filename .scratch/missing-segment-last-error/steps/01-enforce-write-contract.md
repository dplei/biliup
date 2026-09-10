# 01 · 收口 `last_error` 写入契约

Status: ready-for-agent

## 目标

修复仍会产生污染数据的运行时路径，使新写入满足 spec 的字段契约。

## 修改范围

### `crates/biliup-cli/src/server/common/missing_segment.rs`

- `mark_retry_success` 在设置 `status = "succeeded"` 时同时设置 `last_error = None`。
- 把现有成功转换测试改成先注入一个旧错误，再断言成功后错误已清空。
- 删除没有生产调用者的 `reset_for_manual_retry` 及其孤立测试，不为死代码维护错误语义。

### `crates/biliup-cli/src/server/common/upload.rs`

- `claim_manual_recovery` 检测到 source 文件重新出现时，把 `last_error` 设为 `NULL`，不要写
  `source file reappeared; manual recovery requested`。
- 在现有 claim 测试附近补一条回归：source-missing 行重新可用后能被领取，且正常迁移不会制造
  新错误文本。
- 保留以下真实错误写入：上传初始化失败、segment processor 失败、attempt 失败、源文件仍缺失、
  删除失败。

## 验收

- v1 自动补传与人工补传都通过共享 helper 清空旧错误。
- v2 `persist_segment` 的既有 `last_error = NULL` 不被重复封装或改写。
- `rg` 检查不到正常说明文案继续写入 `upload_missing_segment.last_error`。
- `cargo test -p biliup-cli missing_segment` 通过。

## Comments

