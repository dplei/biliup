# Issue #65：残缺上传会话可安全作废

Status: ready-for-human

来源：https://github.com/dplei/biliup/issues/65

## 根因

现有分段删除只接受 `pending` / `failed`，与页面对 `source_missing` 展示删除按钮不一致；
现有会话删除接口只处理零账本空会话，导致“部分分段已上传、其余源文件丢失”的已关闭会话
既不能投稿，也不能终结。先删缺失分段再终结会话还会暴露一个被调度器自动投稿的窗口。

## 方案

- `source_missing` 复用现有幂等文件清理与删除流程。
- 保留投稿 claim 使用的“仅自动终结空会话”路径。
- 人工丢弃在一个 `BEGIN IMMEDIATE` 事务内校验会话已关闭、无投稿 claim、无远端稿件身份、
  无不确定投稿结果、无正在上传/删除的分段，然后直接写入
  `finalized/discarded` 并清除投稿意图；分段账本与 UPOS 描述保留作审计。
- 待投稿会话卡片为可安全丢弃的非空会话展示二次确认按钮。

## 验证

- Rust：`source_missing` 删除 claim；非空会话原子丢弃、账本保留、幂等、危险状态拒绝；
  自动空会话路径仍不吞掉正常会话。
- 前端：lint 与 TypeScript 检查。

## 结果

- `source_missing` 已复用现有幂等文件清理与删除 claim。
- 人工丢弃会话写入 `finalized/discarded`，清除投稿意图并保留会话和全部分段账本；
  自动空会话路径仍只接受零账本、零视频会话。
- 投稿 claim、远端身份、远端结果不确定、正在上传或删除中的会话继续拒绝丢弃。
- 上传页展示二次确认，并明确提示单删缺失分段可能触发自动投稿。
- `cargo test -p biliup-cli --lib`：408 passed、0 failed、10 ignored（首次并发运行有 1 个连接池用例超时，单独复跑及全量复跑均通过）。
- `next lint` 通过（仅保留既有 `<img>` 警告），`tsc --noEmit` 与代码索引检查通过。
