# 09 — 会话续接改用场次键 + session 心跳

Status: resolved
Blocked by: 07
Model: Opus 5 —— 决定「什么算同一场直播」的合并语义，判据过宽会把两场直播并进一个稿件，
比现在的分裂更难挽回；且要在 monitor / enrollment / rescan 三处身份判定上保持一致。

## 背景

对应评估报告 D。同一场帝骑哥直播先创建会话 228，服务重启后又创建空会话 229，最后靠人工合并
数据库才收拾干净。

## 根因（两个条件同时成立）

1. **重启必然产生新的 `streamer_info` 行**：`monitor` 每次检测到 `LiveStatus::Live` 都无条件
   `StreamerInfo::builder().insert()`
   （[monitor.rs:121](crates/biliup-cli/src/server/core/monitor.rs:121)），没有「这场直播已经有
   streamer_info」的复用判断。重启后 `ctx.id()` 就变了。
2. **会话复用的两条路因此同时失效**：
   [`find_or_create_session`](crates/biliup-cli/src/server/common/segment_enrollment.rs:340) 先按
   `(live_streamer_id, streamer_info_id)` 精确匹配（重启后必 miss），再按
   `live_streamer_id + updated_at >= now-30min` 兜底；而 `upload_session.updated_at` 只在
   `persist_segment`（[upload.rs:977](crates/biliup-cli/src/server/common/upload.rs:977)）、
   enroll 走窗口分支时（[segment_enrollment.rs:379](crates/biliup-cli/src/server/common/segment_enrollment.rs:379)）
   和投稿相关操作里刷新——**录制中和上传中都没有心跳**。一段 3.32 GB 分段传一个多小时期间
   `updated_at` 完全不动，30 分钟窗口早已过期。

`select_recovery_candidate`（[upload_session.rs:19](crates/biliup-cli/src/server/common/upload_session.rs:19)）
用同一个窗口判据，所以 `prepare_archive` 也救不回来。
`rescan_local_valid_segments`（[upload.rs:2320](crates/biliup-cli/src/server/common/upload.rs:2320)）
在按新 `streamer_info_id` 找不到会话时同样会**再建一个**，人工补扫反而加剧分裂。

## 改动范围

1. **续接改用场次键**（07 提供）：`monitor` 检测到开播时，先按 `live_streamer_id + 场次键` 查找
   未 finalize 的同场 `streamer_info` 并复用，而不是无条件插入新行；`upload_session` 记录它归属
   的场次键。
2. **场次键为 `None` 时的 fallback**：退回今天的 `live_streamer_id + 时钟窗口` 判据。不得因为
   拿不到键就放宽合并（那会跨场合并），也不得因此拒绝录制。
3. **session 心跳**：分段 enroll、attempt claim、进度落库都刷新 `upload_session.updated_at`，
   让 30 分钟窗口回到「真的静默了 30 分钟」的语义。这一条对 fallback 路径尤其关键。
4. **rescan 不再新建**：[`rescan_local_valid_segments`](crates/biliup-cli/src/server/common/upload.rs:2320)
   找不到会话时改为按 `live_streamer_id` + 场次键找同场未 finalize 会话；确实没有时才新建，
   并在返回值里说明是新建的。
5. `recovery_window_minutes`（[config.rs:105](crates/biliup-cli/src/server/config.rs:105)）保持
   现值即可，不作为主修复手段。

## 验收

- 直播中重启服务并继续录制多个切片：所有分段落在同一 `upload_session`，`segment_order` 连续
  且唯一，最终只创建一个 BV 并按顺序追加分 P。
- 同一主播先后两场直播不得被合并进同一会话（含「上一场刚下播、几分钟后又开播」的情形）。
- 场次键缺失时行为与今天一致，不 panic、不跨场合并。
- 一场持续录制的直播，其 `upload_session.updated_at` 不会老化超过窗口。
- 补扫不再产生第二个会话。

## 测试

- 单元：场次键相同 / 不同 / 缺失三种输入下的续接判定。
- 集成：扩充 `crates/biliup-cli/tests/upload_reliability_incident.rs`，加一条「录制中重启后分段
  归入同一 session」和一条「相邻两场不合并」的回归。

## 风险

- 历史数据里已经分裂的会话不在本任务范围内，沿用 `lifecycle_backfill` 的人工路径处理。
- 这条改动会改变生产库的会话归属语义，建议先在测试主播上灰度一场再上生产。

## Answer

已实现。

1. **monitor 复用同场 streamer_info**：新增 `upload_session::reusable_streamer_info`，
   按 `url + live_session_key` 找仍挂着未 finalize 会话的行并复用，找不到才插入新行。
   查询失败时退回「新建」而不是拒绝录制。
2. **续接改用场次键**：`find_or_create_session` 的顺序变成
   精确 `streamer_info_id` → 相同 `live_session_key`（不看时间窗口）→ 时钟窗口兜底；
   窗口分支会拒绝场次键**明确不同**的会话（一方缺键不算不同），避免把两场并进一个稿件。
   新建会话时写入 `live_session_key`。
   `select_recovery_candidate` / `select_stale_session_indices` 同样加了场次键参数，
   同场会话永远不会被当成「废弃会话」补提交掉。
3. **session 心跳**：新增 `touch_session_activity`；attempt claim 时刷新，分段 enroll 命中已有会话时
   刷新（带 60 秒下限，避免给录制期最热的事务加写锁）。30 分钟窗口回到「真的静默了 30 分钟」的语义。
4. **rescan 不再轻易新建**：按 `streamer_info_id` 找不到时先按 `live_session_key` 找同场未 finalize 会话，
   确实没有才新建，并在返回值里带 `created_session: true`，页面据此弹一条警告。
5. `recovery_window_minutes` 保持现值。

测试：`upload_session` 三条单元测试（同场跨窗口续接、异场不合并、缺键退回窗口）；
集成 `target_08_a_restart_mid_broadcast_keeps_one_session`（重启后同一 session、`segment_order` 连续、
全库只有一个 session）与 `target_08_two_broadcasts_are_never_merged`。

**上线提醒**：这条改动会改变生产库的会话归属语义，建议先在测试主播上灰度一场再上生产。
