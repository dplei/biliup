# 02 — 关闭态零基线会话终结原语

Status: resolved

## 背景

零 lifecycle row 会被 `inspect_completeness` 标记为 `session has no lifecycle baseline`；
`claim_complete_session` 随后写 `blocked_missing_segments` 并递增 `blocked_count`。对已关闭且永远没有
源文件的空会话，周期重试没有任何可能使状态改善。

## 改动范围

1. 在 `upload_session.rs` 增加事务化的空会话判定与终结函数，供投稿协调器和人工接口共用。
2. 判定至少包含：
   - 未 finalized；
   - `submit_requested_at IS NOT NULL`；
   - 零 lifecycle row；
   - `videos_json=[]`，无 aid/bvid；
   - 无 submit claim。
3. 满足条件时在同一个 `BEGIN IMMEDIATE` 事务中：
   - `status='finalized'`；
   - `submit_state='discarded_empty'`；
   - 清空 `last_submit_error`、`blocked_signature`、`next_submit_at`；
   - 保留 `submit_requested_at`、`streamer_info_id` 与会话行作为审计和防复活身份。
4. 扩展 `SubmitClaim` 或协调器结果，显式表达 `EmptyFinalized`/`DiscardedEmpty`；调用方不得把它当作
   blocked、failed 或远端投稿成功。
5. 自动终结只发生在有持久投稿意图的关闭态会话；普通活动空会话保持不变。

## 不变量

- 终结空会话不调用 B 站登录、上传或投稿接口。
- 有任何 lifecycle row 时不得走此分支，无论该行处于什么状态。
- 有远端标识或 submit claim 时不得自动清理。
- 状态转换幂等；并发协调器最多一个执行更新，其余读取 finalized。
- finalized 空会话继续能被 `finalized_session_for_streamer_info` 找到，补扫不会复活它。

## 验收与测试

1. 构造已请求投稿的零基线会话：一次协调后变成 `finalized/discarded_empty`，`blocked_count` 不增加。
2. 再运行启动/周期扫描：该 id 不再入选。
3. 没有 `submit_requested_at` 的活动空会话：保持 uploading。
4. 分别构造 pending、failed、succeeded lifecycle row：均不得被当作空会话终结。
5. 构造 aid、bvid、claim 任一存在的零行会话：拒绝自动终结。
6. 两个并发调用只产生一次终结状态，无数据库锁泄漏。

## Comments

- 不新增“所有 0/0 自动 finalized”的 migration；历史行通过协调器或 03 的显式入口收敛。

## Answer

- 新增 `discard_empty_session` 事务原语，严格检查关闭意图、零 lifecycle、空 `videos_json`、无
  aid/bvid、无 claim、非远端不确定状态，再写入 `finalized/discarded_empty`。
- `claim_complete_session` 返回显式 `DiscardedEmpty`，投稿协调器不加载账号、不调用远端接口、不发送
  缺失分段告警；周期扫描随后不再选中该 id。
- 新增关闭态收敛、安全拒绝、并发幂等及历史启动扫描测试；累计 `blocked_count` 保留作审计，不再增长。
