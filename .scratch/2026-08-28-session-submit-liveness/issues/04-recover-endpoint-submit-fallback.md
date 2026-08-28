# 04 — recover 接口投稿兜底

Status: resolved
Blocked by: 02

## 背景

`POST /v1/uploads/sessions/{id}/recover` 当前只调用 `recover_due_segments`。当所有 lifecycle row 都是
`succeeded` 时固定返回 `started=[]`，既不检查完整性，也不投稿，无法救回原 issue 中
`submit_state=NULL` 的历史会话。

## 改动范围

1. 调用有效未 finalized 会话的 recover 时，先幂等写入投稿意图；操作员点击本身就是明确的“本场应
   收尾”授权。
2. 保留现有分段恢复能力：
   - 有 due rows：领取并异步执行；完成事件由 03 唤醒投稿；
   - 已有恢复任务在跑：返回 busy，不抢占 attempt；
   - 无 due rows：异步调用统一提交协调器。
3. 接口不得等待远端上传或投稿完成，避免重新引入反代超时；返回 202 和结构化状态，例如：
   `segments_started`、`segments_busy`、`submission_queued`、`blocking_summary`。
4. 如果账本不完整且没有可领取行（如 `source_missing`、`deleting`、未知状态），返回明确阻塞信息，
   不能继续用空数组表示“恢复成功”。
5. finalized 继续返回 409；不存在返回 404；已有不确定 submit claim 返回人工核对提示，不发起第二稿。
6. 为前端提供轮询所需的会话状态，不在 handler 内猜测最终投稿结果。

## 验收

- 全部分段 succeeded、`submit_state=NULL` 的会话调用 recover 后真正进入投稿。
- 全部分段 succeeded、`blocked_missing_segments` 的会话同样可恢复。
- 有 pending/failed 行时仍按原顺序补传，补传完成后自动投稿。
- source_missing 等不可自动完成状态返回可操作的阻塞原因。
- 重复点击只产生一次远端投稿。

## 测试

- API 集成覆盖：完整空转旧案例、仍需补传、busy、source_missing、finalized、已有 submit claim。
- 断言接口快速返回，远端动作在后台执行。

## Answer

`POST /v1/uploads/sessions/{id}/recover` 现在先幂等持久化人工投稿意图，再领取到期分段；没有可领取的
分段且不存在运行中的 attempt/submit claim 时，会异步唤醒统一投稿协调器并立即返回 202。响应包含
分段启动/busy、投稿排队、当前会话投稿状态、完整性及可操作的阻塞摘要；`source_missing`、`deleting`、
未知状态和不确定 submit claim 均不会再以空数组冒充恢复成功。端点回归覆盖历史完整会话、待补传、
busy、`source_missing`、finalized/不存在及保留 claim 的防重复语义。
