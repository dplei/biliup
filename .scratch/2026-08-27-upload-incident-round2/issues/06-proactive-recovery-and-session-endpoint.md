# 06 — 启动主动扫描与按会话恢复接口

Status: resolved
Blocked by: 02
Model: Opus 5 —— 要在「到期领取」这条新路径上同时守住幂等、`segment_order` 顺序、`aid/bvid`
复用和 finalized 边界；这几条不变量此前是靠单一调用点隐式保证的，换成后台循环后每一条都得
显式重建。

## 背景

对应评估报告 E。服务重启后，已到重试时间的 `failed/pending` 分段不会被主动领取，只能干等
下一场直播；也没有「恢复指定会话」的接口，普通页面上传接口会绕开生命周期会话另建稿件。

## 根因

- [`recover_due_missing_segments`](crates/biliup-cli/src/server/common/upload.rs:1766) 只有两个
  调用点，都在 `process_with_upload` 内部
  （[upload.rs:278](crates/biliup-cli/src/server/common/upload.rs:278)、
  [upload.rs:1028](crates/biliup-cli/src/server/common/upload.rs:1028)），必须有新的直播事件才触发。
- 启动时 [`app.rs:44`](crates/biliup-cli/src/server/app.rs:44) 只拉起
  `start_stale_attempt_recovery`（把卡死租约收敛成 `failed`），没有任何一处领取到期行。
- 路由面（[router.rs:84](crates/biliup-cli/src/server/router.rs:84) 起）只有
  `missing`、`missing/rescan`、`missing/{id}/recover|retry|delete`，没有按会话恢复的入口。

## 改动范围

- 启动后拉起到期扫描循环（与 stale 收割同级，见 `app.rs`）：按 `segment_order` 顺序领取到期行，
  复用会话已有 `aid/bvid`——首段建稿、后续 append。
- 扫描必须走
  [`check_recovery_eligibility`](crates/biliup-cli/src/server/common/recovery_eligibility.rs)，
  不得对 finalized 会话创建新任务（不变量 6）。
- 新增 `POST /v1/uploads/sessions/{id}/recover`：对指定会话重新扫描并恢复，**不新建投稿会话**，
  返回被领取的分段列表。
- 恢复动作全部走 02 的 spawn + 进度落库机制，不在 handler 内 `await` 上传。
- 与既有的补扫接口划清边界：`rescan` 负责「本地有文件但没记录」，本任务负责「有记录但没人跑」。

## 验收

- 服务重启后，已有待补传分段无需新直播事件即可自动恢复。
- 手动按会话恢复立即开始，且保持同一 BV，不产生第二个稿件。
- 恢复严格按 `segment_order` 执行；并发触发（循环 + 手动）不会对同一行起两个 attempt。
- finalized 会话调用该接口返回明确拒绝，不创建任何新行。

## 测试

- 集成：构造「重启后有 3 条到期行」的库，断言无直播事件下全部被领取且顺序正确。
- 回归：finalized 会话 + 迟到分段，断言不产生新的待提交 session。

## Answer

已实现。新模块 `recovery_scheduler`：

- `start_due_recovery_scan` 在 `app.rs` 与 stale 收割同级拉起，每 60 秒扫一次到期
  `pending`/`failed` 行；SQL 直接 `LEFT JOIN upload_session` 排除 finalized，
  并按 `upload_session_id, segment_order, id` 排序。
- 按会话分组，每组一个后台任务顺序领取并执行，因此 `segment_order` 顺序有保证；
  全局 `ACTIVE_GROUPS` 保证同一会话同时只有一个恢复流程（守卫在 Drop 里释放，panic 也不会卡死）。
  无会话的行用 `-missing_id` 作为独立分组键，不会互相阻塞。
- 每一行仍然走 `claim_manual_recovery` → `check_recovery_eligibility`，finalized 会话不会产生新任务；
  行级 `attempt_token` CAS 保证扫描循环与手动点击不会对同一行起两个 attempt。
- 新增 `POST /v1/uploads/sessions/{id}/recover`：会话不存在返回 404，finalized 返回 409，
  其余只领取该会话已有的待补传行并返回 `{started, skipped, busy}`，不新建投稿会话。
- 执行全部走 02 的 spawn + 进度落库机制。
