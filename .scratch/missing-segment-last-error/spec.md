# 缺失分段 `last_error` 语义修复

来源：[dplei/biliup#7](https://github.com/dplei/biliup/issues/7)

## 目标

让 `upload_missing_segment.last_error` 只表达真实失败诊断，不再同时承担状态说明；任何
`succeeded` 行都必须满足 `last_error IS NULL`。健康判断继续以 `status` 和 attempt lease 为准，
不能以 `last_error` 是否非空为准。

## 当前事实

- lifecycle v2 的统一成功提交 `persist_segment` 已执行 `last_error = NULL`，这部分不重做。
- lifecycle v1 的两个恢复入口都经过 `mark_retry_success`，该 helper 只改 `status`，会保留旧错误。
- `source_missing` 的源文件重新出现后，当前代码把正常状态说明写入 `last_error`。
- `reset_for_manual_retry` 也写入正常说明，但生产代码没有调用者，只有自身单元测试。
- 上传初始化失败、分段处理失败、传输失败、源文件缺失和删除失败仍是真错误，应继续写
  `last_error`。
- 缺失分段健康接口已经按 `status` 和 stale lease 判定，不依赖 `last_error`。
- 历史 `succeeded` 行可能仍带旧错误或旧状态说明，需要按状态做一次确定性清理。

## 字段契约

1. 只有实际失败路径可以写非空 `last_error`。
2. 成功提交必须清空 `last_error`，v1/v2 一致。
3. 正常状态迁移不得用 `last_error` 记录说明；需要追踪原因时使用现有 tracing、恢复审计或
   `upload_attempt` 历史。
4. `status` 是当前健康状态的权威；`last_error` 只是诊断文本。

## 最小方案

- 修正共享的 v1 `mark_retry_success`，让两个调用入口一次收敛。
- 源文件重新出现时清空已解决的 source-missing 错误，不写新的说明文案。
- 删除无生产调用者的 `reset_for_manual_retry` 及其孤立测试。
- 新增一条 migration，仅清空 `status = 'succeeded'` 的历史 `last_error`；不按文案猜类型。
- 更新读取侧注释/文案，使其不再宣称 `last_error` 可以保存正常进度说明。

## 不做

- 不增加 `last_error_kind`：字段恢复为单一错误语义后，`status` 已能表达状态，新增列只会形成
  第二套状态分类。
- 不增加 `last_note`：现有 tracing、恢复审计和 `upload_attempt` 已保存过程信息。
- 不迁移或解析历史错误文本：没有可靠结构，字符串分类会把展示文案固化成数据协议。
- 不做真实 B 站上传验证：本问题是本地状态迁移和 SQLite 数据契约，可用确定性测试覆盖。

## 拆解

| step | 内容 | 依赖 | 状态 |
| --- | --- | --- | --- |
| [01](steps/01-enforce-write-contract.md) | 收口成功与正常迁移的写入语义 | — | complete |
| [02](steps/02-clean-history-and-read-semantics.md) | 清理历史成功行并统一读取侧语义 | 01 | complete |
| [03](steps/03-verify-and-submit.md) | 全量验证、回写并提交 PR | 01、02 | complete |

## 完成标准

- 所有成功路径都保证 `last_error IS NULL`。
- 当前代码中没有正常状态说明写入 `upload_missing_segment.last_error`。
- migration 只影响 `succeeded` 行，失败行诊断原样保留。
- Rust 测试和前端检查通过，PR 以 `dev` 为 base 并关联 issue #7。
