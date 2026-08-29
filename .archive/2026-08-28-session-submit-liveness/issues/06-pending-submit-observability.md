# 06 — 待投稿会话 API、页面与通知

Status: resolved
Blocked by: 04, 05

## 背景

补传页当前从 missing 行反推 blocked 会话。默认过滤只返回活动异常行；全部分段成功后页面为空，
即使 `upload_session.submit_state='blocked_missing_segments'` 也不会显示任何入口。通知文案仍让操作员
前往这个空页面。

## 改动范围

### API

1. 增加独立的待投稿会话查询，或在现有接口中返回独立 `sessions` 集合；不得再要求必须存在一条当前
   可见的 missing row 才能展示 session。
2. 会话视图至少包含：session id、主播/场次、投稿意图时间、submit state、attempts、最近错误、
   next submit time、claim 是否存在、账本完整性摘要、aid/bvid/status。
3. 后端给出稳定的可操作状态：`waiting_segments`、`ready_to_submit`、`submitting`、`retry_scheduled`、
   `manual_inspection`，前端不重复推导状态机。

### 页面

1. 补传页新增“待投稿会话”区域，独立于 missing 分段筛选。
2. 区分展示：
   - 仍有异常分段：跳到具体 missing row；
   - 分段已齐、等待协调/退避：显示状态与下次时间；
   - 自动流程无法继续：显示“重新投稿/恢复会话”按钮；
   - 有不确定 claim：只提示核对，不提供会制造重复稿件的普通重试按钮。
3. recover 调用后根据 04 的结构化响应提示“已开始补传”“已排队投稿”或具体阻塞原因。

### 通知

1. “存在未完成分段”通知只在账本确实不完整时引导缺失补传。
2. 账本完整但等待重投/退避时使用单独文案，给出 session id 与页面入口。
3. 状态签名去重，避免周期扫描重复推送同一内容。

## 验收

- 四段全 succeeded 的 blocked 会话仍在页面可见，并能触发 recover。
- active missing 列表为空不影响待投稿会话区域。
- incomplete 与 complete-pending-submit 的文案和按钮不同。
- `ok_no_aid`/不确定 claim 不提供危险的一键重复投稿。
- 通知不再把“分段已齐”用户引向空页面。

## 测试

- API 状态映射单测。
- 前端至少覆盖 waiting/ready/submitting/retry/manual-inspection 五种渲染状态。
- `tsc --noEmit` 与 `next build`。

## Answer

新增 `GET /v1/uploads/sessions/pending`，直接从有持久投稿意图的非 finalized 会话构建视图，返回主播/
场次、投稿次数与时间、最近错误、claim、完整性、aid/bvid/status，以及后端稳定映射的
`waiting_segments`、`ready_to_submit`、`submitting`、`retry_scheduled`、`manual_inspection` 五态。
补传页新增独立的“待投稿会话”区域，不受 missing 状态筛选影响；可安全恢复的状态调用 04 的会话
recover 并按结构化响应提示，`ok_no_aid` 或陈旧/不确定 claim 只显示人工核对且不提供普通重试按钮。
分段不完整通知继续按 blocking signature 去重，账本完整但明确失败则使用独立的退避重试通知文案。
后端测试覆盖五态映射、完整会话在活动 missing 列表为空时仍可见，以及不确定 claim 不被释放。
