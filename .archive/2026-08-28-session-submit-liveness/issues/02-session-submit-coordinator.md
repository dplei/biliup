# 02 — 统一会话提交协调器

Status: resolved
Blocked by: 01

## 背景

现有 `submit_session` 是 `upload.rs` 内的私有函数，调用者必须已经持有 `UploadContext`、模板、主播和
运行配置。这使恢复调度器和 API 很难按 session id 复用它，最终形成多条只恢复分段、不负责投稿的
半闭环路径。

## 改动范围

1. 抽出按 session id 工作的统一协调入口，例如
   `reconcile_session_submission(config, pool, session_id, trigger)`；模块位置应让下播、恢复调度器和 API
   共用，不在各处复制 studio 构建与结果写回。
2. 协调器自行加载：
   - `upload_session` 与其 `streamer_info`；
   - `live_streamer`、关联 `upload_streamer`；
   - 主播 override 后的有效 Config；
   - Cookie 登录、线路无关的 Bilibili/封面/投稿上下文。
3. 复用现有 `claim_complete_session`、`build_studio`、`submit_to_bilibili`、`mark_submitted` 和
   submit claim 语义，不另造第二套完整性闸门。
4. 返回结构化结果，至少区分：`NotRequested`、`Blocked`、`ClaimedElsewhere`、`Finalized`、
   `Submitted`、`RetryScheduled`、`ManualInspectionRequired`。
5. 明确失败分类：
   - studio 构建或明确远端失败：释放 claim，写 `failed` 与有上限退避的 `next_submit_at`；
   - `ok_no_aid`、远端成功后本地写回失败、已有无法证明安全的 claim：保留 claim，停止自动重试；
   - 完整性 blocked 不增加远端投稿次数。
6. 把原下播调用点改为调用协调器，保证正常路径与补提交路径使用同一实现。

## 并发与安全

- 远端请求只能在成功获得 submit claim 后发生。
- SQLite 事务内不得执行登录、封面上传或远端投稿。
- 多个触发源可以重复调用协调器；`claim_complete_session` 必须仍是唯一副作用闸门。
- 不自动偷取或按时间回收 submit claim。

## 验收

- 只给 session id 与 Config/Pool 即可完成一次正常投稿。
- 两个并发协调调用最多一个进入远端提交。
- incomplete 会话只写 blocked，不调用远端。
- 明确失败进入退避；不确定结果保持人工检查状态。
- 正常下播投稿的标题、封面、合集、submit API 与改造前一致。

## 测试

- 为远端 submit 与 studio 构建提供可注入测试边界或 spy。
- 覆盖完整、blocked、并发 claim、明确失败、`ok_no_aid`、finalized 六类结果。

## Answer

已增加按 session id 工作的 `reconcile_session_submission` 与结构化结果，统一完成 durable intent
预检、严格 lifecycle claim、历史主播/模板/effective config 加载、登录/封面/投稿、结果写回和有上限
退避。正常下播及废弃会话路径已改用协调器；不确定远端结果继续保留 claim。
