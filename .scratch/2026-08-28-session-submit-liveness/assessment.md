# 2026-08-28 投稿会话活性修复：总览与任务拆分

Status: ready-for-agent

来源：[`dplei/biliup#1`](https://github.com/dplei/biliup/issues/1) 及
[`2026-08-28 再次复现`](https://github.com/dplei/biliup/issues/1#issuecomment-5447934912)。

本文只拆分实现步骤，不包含代码改动。目标是同时修复两类已经在生产发生的状态：

- 下播提交检查先于最后一个分段成功，先写下 `blocked_missing_segments`；最后一段随后成功，
  但没有消费者再次投稿。
- 下播事件已经过去或进程随后退出，会话从未进入提交检查，因而 `submit_state = NULL`；补传完成后
  同样没有消费者投稿。

原 issue 附带的 `.part.flv` 中间文件清理是独立问题，不纳入本轮投稿活性计划。

---

## 1. 已确认根因

当前系统满足“安全性”，但不满足“活性”：

1. `claim_complete_session` 会在生命周期账本不完整时正确阻止投稿，并使用 submit claim 防止并发
   重复提交。
2. `persist_segment` 把分段改为 `succeeded` 并重建 `videos_json` 后即返回，不发布“账本可能已完整”
   的会话级事件。
3. 下播路径只调用一次 `submit_session`。这次检查一旦早于最后一个恢复任务完成，就没有第二次机会。
4. 周期恢复与 `POST /v1/uploads/sessions/{id}/recover` 都只领取 `pending/failed` 分段；全部分段已经
   `succeeded` 时，它们稳定返回空结果。
5. 补传页的会话横幅依赖当前 missing 行集合。默认“待补传”过滤不返回 `succeeded` 行，因此账本完整后
   会话从页面消失。

缺失的系统不变量是：

> 一旦某个会话被持久标记为“本场已结束、要求投稿”，系统必须在账本最终完整后至少尝试一次投稿；
> 进程重启、重复唤醒和并发唤醒不得导致重复稿件。

---

## 2. 核心设计决策

### 2.1 投稿意图必须持久化

不能用“当前所有 lifecycle row 都是 `succeeded`”直接判断应当投稿。直播期间两个分段之间也可能
短暂满足这个条件，直接扫描会提前建稿并关闭 enrollment 窗口。

为 `upload_session` 增加独立的持久投稿意图，例如 `submit_requested_at`：

- `NULL`：本场仍可能产生新分段，自动协调器不得投稿；
- 非 `NULL`：录制生产端已经关闭，待账本完整后必须最终投稿。

该字段与 `submit_state` 分工明确：前者表达“是否应该最终投稿”，后者表达“最近一次投稿/闸门结果”。

### 2.2 投稿意图要在生产端关闭边界落库

投稿意图应在所有尾部分段已经完成 durable enrollment、上传 channel 即将关闭时写入，而不是等上传
消费者全部跑完才写。这样即使最后一段仍在预处理/上传，或进程在其后退出，重启扫描仍知道本场已经
结束。

### 2.3 所有入口统一唤醒一个提交协调器

下播、`persist_segment` 成功、人工 recover、启动/周期扫描都不得各自复制投稿流程；它们只调用同一个
按 session id 工作的协调器。协调器统一负责：

- 读取会话、主播、投稿模板和有效 override 配置；
- 调用现有 `claim_complete_session`；
- 账本不完整时记录 blocked 并安全返回；
- 账本完整时构建 studio、调用投稿接口并 `mark_submitted`；
- 远端失败时释放 claim、记录退避；
- `ok_no_aid` 或“远端成功、本地写回不确定”时保留 claim，禁止自动重复投稿。

远端网络请求不得发生在 SQLite 事务内。

### 2.4 自动扫描只处理有投稿意图的会话

周期扫描只选择：

- `status != 'finalized'`；
- `submit_requested_at IS NOT NULL`；
- 没有 submit claim；
- 到达下一次投稿时间；
- 状态允许自动重试。

严禁把所有 `status='uploading' AND lifecycle rows all succeeded` 的会话一概提交。

### 2.5 人工 recover 同时是显式投稿授权

操作员调用会话 recover 时：

- 有待恢复分段：照旧领取分段，并持久写入投稿意图；最后一段成功会自动唤醒投稿。
- 没有待恢复分段且账本完整：持久写入投稿意图并立即异步唤醒提交协调器。
- 账本仍不完整但没有可领取行：返回明确的阻塞原因，不再用 `started=[]` 冒充成功恢复。

这条路径负责救回历史上 `submit_state=NULL`、无法安全自动判定是否已经下播的会话。

### 2.6 历史数据迁移必须保守

- `blocked_missing_segments` 已证明系统曾经尝试过下播提交，可以安全回填投稿意图。
- `submit_state=NULL` 既可能是事故残留，也可能仍在直播，migration 不得批量猜测；由人工 recover 或
  其它可靠的结束证据显式设置。

---

## 3. 状态收敛

```text
录制中
  submit_requested_at = NULL
          │ 生产端关闭；尾段均已 durable enrollment
          ▼
等待投稿
  submit_requested_at != NULL
          │
          ├─ 账本不完整 ──> blocked_missing_segments
          │                      │ 最后一个分段 succeeded
          │                      └──────────────┐
          │                                     ▼
          ├─ 账本完整 ──> submitting（持有 claim）──> finalized / ok_with_aid
          │                                     │
          │                                     ├─ 明确失败：释放 claim + 退避重试
          │                                     └─ 结果不确定：保留 claim，人工检查
          │
          └─ 重启/漏事件 ──> 周期协调器重新检查
```

---

## 4. 子任务与依赖

| # | 子任务 | 目标 | Blocked by |
|---|---|---|---|
| [01](issues/01-submit-intent-state.md) | 持久投稿意图与状态迁移 | 建立不会误投直播中会话的 durable 判据 | — |
| [02](issues/02-session-submit-coordinator.md) | 统一会话提交协调器 | 把按 session id 补提交收口为一个幂等入口 | 01 |
| [03](issues/03-close-and-segment-wakeups.md) | 结束边界与分段完成唤醒 | 下播意图不丢，最后一段成功立即重新检查 | 02 |
| [04](issues/04-recover-endpoint-submit-fallback.md) | recover 接口投稿兜底 | 让历史完整会话有真正的人工恢复入口 | 02 |
| [05](issues/05-submit-reconciliation-scan.md) | 启动与周期补提交扫描 | 覆盖进程重启、任务丢失和明确失败退避 | 02 |
| [06](issues/06-pending-submit-observability.md) | 待投稿会话 API、页面与通知 | 即使 missing 行全成功也可见、可操作 | 04, 05 |
| [07](issues/07-incident-regression-and-verification.md) | 事故回归与端到端验证 | 锁住竞态、重启、人工恢复和防重复不变量 | 03, 04, 05, 06 |

完成 02 后，03、04、05 可以并行；06 可在 04 的接口语义稳定后开始，07 最后统一收口。

---

## 5. 整体验收

1. 构造评论中的 11 秒竞态：下播检查先 blocked，最后一段随后成功；无需下一次直播、无需人工操作，
   会话最终只产生一个 BV。
2. 构造原 issue 的重启路径：生产端已关闭、最后一段重启后补传；启动扫描最终完成投稿。
3. 对历史 `submit_state=NULL`、全分段成功会话调用 recover，接口不再空转，最终完成投稿。
4. 直播仍在进行且当前分段恰好全成功时，不得因为周期扫描提前投稿。
5. 下播、最后分段、人工 recover、周期扫描同时唤醒时，最多一个调用者获得 submit claim，远端只收到
   一次投稿请求。
6. `ok_no_aid`、远端可能已接受但本地无法确认、已有 submit claim 等不确定状态不得自动偷 claim 或
   重复投稿。
7. 补传页能独立显示“分段未齐”和“分段已齐、等待/重试投稿”，文案不再把后者引向空列表。

## 6. 非目标

- 不在本轮处理音量标准化遗留 `.part.flv` 的清理。
- 不自动修复已经重复创建的远端稿件。
- 不自动偷取无过期策略的 submit claim；不确定远端结果继续要求人工核对。
- 不改变正常“整场只投稿一次”的产品语义。
