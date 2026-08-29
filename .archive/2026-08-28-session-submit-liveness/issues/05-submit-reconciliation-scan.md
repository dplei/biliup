# 05 — 启动与周期补提交扫描

Status: resolved
Blocked by: 02

## 背景

即使结束边界和 `persist_segment` 都会主动唤醒，进程仍可能在两次动作之间退出，或者后台任务创建
失败。可靠系统需要数据库驱动的最终一致性扫描，而不能把正确性寄托在一次内存事件上。

## 改动范围

1. 新增会话级补提交扫描，与现有 stale attempt reaper、due segment recovery 同级启动：
   - 服务启动后立即扫描一次；
   - 之后按固定周期扫描。
2. 只选择已经有 `submit_requested_at`、未 finalized、无 submit claim、且 `next_submit_at` 已到期的会话。
3. 每个候选会话异步调用统一协调器；限制并发，避免多个会话同时登录、传封面、调用投稿接口。
4. submit claim 继续承担跨触发源和跨扫描轮次去重；进程内 registry 只能用于减少噪声，不能成为正确性
   唯一来源。
5. 明确失败使用有上限指数退避并带抖动；blocked 会话不需要高频重试，主要由分段状态变化唤醒，
   周期扫描只作漏事件兜底。
6. 以下状态不得自动扫描：
   - 没有投稿意图的活跃会话；
   - finalized；
   - 持有 submit claim；
   - `ok_no_aid` 或远端结果不确定；
   - 尚未到 `next_submit_at` 的明确失败。
7. 记录结构化日志：扫描候选数、claimed/blocked/submitted/retry/manual-inspection 分类和 session id。

## 验收

- 进程在投稿意图落库后退出，重启无需下一场直播即可继续投稿。
- 明确失败按退避重试，不会每分钟轰炸 B 站。
- 活跃但当前全 succeeded 的会话不会被选中。
- 不确定 claim 永不被扫描器偷走。
- 多轮扫描与事件唤醒重叠时仍只提交一次。

## 测试

- 用 fake clock 覆盖启动立即扫描、退避未到/已到、blocked 漏事件兜底。
- 并发扫描与 `persist_segment` 唤醒的单次提交断言。

## Answer

新增数据库驱动的 `submission_scheduler`：服务启动立即扫描，之后每分钟扫描有
`submit_requested_at`、未 finalized、无 claim 且退避已到期的会话；`ok_no_aid` 和异常的
claim-less `submitting` 会被保守排除。每批最多读取 128 个会话、最多并发协调 2 个，并按
blocked/submitted/retry/claim/manual 等结果记录结构化日志。新增独立持久的
`submit_retry_attempts`（不污染只统计真实远端请求的 `submit_attempts`）；明确失败的指数退避增加了有界抖动，
周期扫描与事件唤醒重叠时仍由数据库 submit claim 去重。fake-clock 测试覆盖启动扫描、未到/已到
退避、活跃无意图、finalized 与不确定状态排除。
