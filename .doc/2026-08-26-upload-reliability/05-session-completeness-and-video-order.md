# 子任务 05：session 完整性闸门与分 P 排序

Status: completed

Blocked by: 01, 02

## 目标

投稿前以数据库生命周期账本为唯一事实来源。存在任何未成功或不可证明成功的预期分段时，严格禁止 finalize；分 P 列表按稳定顺序重建并去重。

## 完整性定义

一个 v2 session 只有同时满足以下条件才可投稿：

1. 没有 pending、uploading、failed、source_missing 或 deleting 行；
2. 每条生命周期记录均为 succeeded；
3. 每条 succeeded 均有可反序列化的 `video_json`；
4. `segment_order` 在 session 内唯一且连续；
5. 按源分段 identity 和远端 Video identity 检查后不存在冲突重复；
6. session 尚未 finalized，且本次 submit 尚未被其他任务 claim。

## 详细步骤

### 1. 完整性查询

- [x] 新建 `SessionCompleteness` 结构，包含各状态计数、总预期数、有效 Video 数和异常原因。
- [x] 使用一组事务内查询或单条聚合 SQL 读取完整状态，避免多次查询之间发生变化。
- [x] 对 v1/legacy session 使用子任务 07 生成的基线记录，不直接假定 `videos_json.len()` 等于预期数。
- [x] 将未知 status、损坏 video_json、重复 order 视为阻塞异常，而不是忽略。

### 2. 投稿 claim

- [x] 为 session 投稿增加原子 claim，避免下播流程、后台补提交和重启恢复并发 submit。
- [x] claim 前先执行完整性查询。
- [x] 不完整时不调用 `build_studio` 或任何 B 站 submit API。
- [x] 写回 `submit_state=blocked_missing_segments`、状态计数和可读摘要。
- [x] blocked 不增加真正的 submit attempt 计数；可另记 blocked 次数或结构化日志。
- [x] 全部分段恢复后，后台补提交器或下一次会话恢复能够再次检查并获得 claim。

### 3. 重建 videos_json

- [x] 查询 session 全部 succeeded 生命周期行，按 `segment_order ASC, id ASC` 排序。
- [x] 从 `video_json` 反序列化 Video，拒绝静默丢弃损坏数据。
- [x] 本地逻辑 identity 使用 enrollment/missing id 和 normalized path，不使用标题作为唯一键。
- [x] 远端 Video identity 优先使用稳定 filename；标题只用于诊断重复，不作为唯一判定。
- [x] 完整性通过后，在 submit 前事务更新 `videos_json` 为重建结果。
- [x] 不再依赖 `insert_video_at_order` 对当前 Vec 的 best-effort 插入作为最终顺序来源。

### 4. 提交与 finalize

- [x] submit 返回 aid 后按现有原则写 aid/bvid/finalized。
- [x] aid 写回失败仍保持现有“避免盲目重复投稿”的异常策略，但要保留 claim/响应诊断。
- [x] submit 成功前再次确认 session claim 未失效。
- [x] finalized 后禁止任何路径改变 `videos_json` 或加入新预期分段。
- [x] 加合集失败不回滚投稿完成状态。

### 5. 重复 `04:54:30` 诊断

- [x] 在事故审计中分别比对源路径、segment_order、Video filename 和 title。
- [x] 同标题不同源路径可能是合法内容，不自动删除。
- [x] 同源 identity 对应两个 Video 才判定为幂等重复并阻塞自动投稿。
- [x] 已投稿稿件的重复项只生成报告，未经确认不自动 edit。

### 6. 页面和告警

- [x] missing 页面显示“会话因 N 个未完成分段暂停投稿”。
- [x] 提供各状态数量和最早阻塞分段链接。
- [x] session finalized 前不得展示为投稿完成。
- [x] webhook 只在首次进入 blocked 或阻塞集合发生实质变化时通知。

## 测试

- [x] 每一种 active/异常状态都单独阻止 submit。
- [x] uploading 被 watchdog 转 failed 后仍阻止 submit。
- [x] 最后一条 missing succeeded 后自动满足完整性并只 submit 一次。
- [x] 乱序完成最终按 enrollment order 重建。
- [x] 相同标题不同路径不误删；相同 identity 重复 Video 阻塞。
- [x] 两个并发 finalize 调用只有一个获得 claim。

## 验收标准

- 不完整 session 的 submit HTTP 请求数严格为 0。
- 完整 session 的 `videos_json` 与 succeeded 生命周期行一一对应、顺序一致。
- 分 P 重复或顺序冲突会阻塞并显式报错，不会静默投稿。

## 完成记录

- 日期：2026-08-26
- 提交：`17ff807`
- 实现：migration 13 增加持久 submit claim 与 blocked fingerprint；正常下播、废弃会话补提交统一在 `build_studio` 前执行生命周期完整性检查；通过后按账本顺序重建 `videos_json`，失败则持久化计数/摘要且 submit attempt 保持不变。
- 兼容：legacy `videos_json` 不再被当作完整性证据；子任务 07 写入 synthetic lifecycle 基线后即可通过同一闸门。
- 验证：`cargo test -p biliup-cli --lib` 200 passed / 1 ignored；事故测试 12 passed / 2 ignored；workspace check 通过。Next 编译成功，类型检查停在既有 `app/ui/TemplateFields.tsx:117` 回调类型错误。
