# 01 — 持久投稿意图与状态迁移

Status: resolved

## 背景

当前 `upload_session.status='uploading'` 同时表示“直播仍在产生分段”和“直播已结束但尚未投稿”，
无法作为自动补提交条件。`submit_state` 又只是最近一次投稿结果；原事故可能保持 `NULL`，不能表达
本场是否已经关闭。

## 改动范围

1. 新增 migration `19_*`，为 `upload_session` 增加：
   - `submit_requested_at DATETIME`：持久投稿意图；
   - `next_submit_at DATETIME`：明确失败后的下一次自动尝试时间；
   - 为“待协调会话”增加部分索引，查询条件至少包含未 finalized、有投稿意图、无 claim。
2. 更新 `UploadSession` / `InsertUploadSession` 模型与所有构造器；新会话默认两个字段均为 `NULL`。
3. 在 `upload_session.rs` 增加原子状态函数，例如：
   - `request_session_submit(session_id, now)`：幂等设置投稿意图；
   - `schedule_submit_retry(session_id, claim_token, next_at, error)`；
   - 查询“当前是否要求投稿/是否到期”的纯函数或视图结构。
4. migration 只为 `submit_state='blocked_missing_segments'` 的历史未 finalized 会话回填
   `submit_requested_at`。不得批量回填所有 `submit_state IS NULL` 会话。
5. 明确状态字段职责并写入代码注释：`submit_requested_at` 是目标，`submit_state` 是最近结果，
   `submit_claim_token` 是远端副作用所有权，`next_submit_at` 是自动重试节流。

## 不变量

- 设置投稿意图是单调且幂等的；不得因一次 blocked 或失败清空。
- finalized 会话不重新打开。
- migration 不得把仍在直播的旧会话误标为待投稿。
- 不修改任何已应用 migration 的字节。

## 验收

- 新会话不会自动带投稿意图。
- 同一会话重复 request 只保留最早或已有的有效意图，不产生状态回退。
- 历史 blocked 会话迁移后可被协调器发现；历史 `submit_state=NULL` 会话保持不变。
- 待协调查询有索引，不做全表 lifecycle 聚合扫描。

## 测试

- migration fixture：blocked / NULL / finalized 三类历史会话。
- 状态函数：重复 request、finalized 拒绝、retry 时间推进。

## Answer

已完成 migration 19、`UploadSession`/插入模型字段、单调投稿意图、廉价 readiness 查询和带
claim 校验的退避调度。迁移测试锁定仅回填历史 `blocked_missing_segments`，会话状态测试锁定重复
request、finalized 拒绝和 retry 时间推进。
