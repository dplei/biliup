# 下播边界的投稿门禁误报降噪

来源：[dplei/biliup#48](https://github.com/dplei/biliup/issues/48)

## 结论

issue 提出的根因链（拉流 404 → 账本行停在 `uploading` → 门禁永不放行）**不成立**。两条 WARN
是设计内的下播边界竞态，在下一次复查之前就已自行放行；真正的问题是**告警噪音**，不是投稿卡死。
issue 评论区已有一份逐条核对，本 spec 在代码上复核后结论一致，这里只记实现所需的事实。

## 已核实的事实

- 404 分支 `stream_gears.rs:158-173` 打完 WARN 直接 `return Ok(status)`，不碰账本。
  行被置为 `uploading` 的唯一入口是 `claim_enrolled_attempt`，前置条件 `status IN ('pending','failed')`。
- 分段在发给 uploader **之前**就已由 `SegmentEventProcessor::enqueue_validated` 入账为 `pending`
  （`download.rs:370-395`），所以门禁不可能漏掉尾段，只会「看到尾段还没完」。
- 下播时有三路提交触发，前两路都**预期**撞上仍在处理的尾段：
  1. `SegmentEventProcessor::finish`（`download.rs:283-302`）写入 durable intent 后**立刻**
     spawn `DownloadClosed` 提交，此时 uploader 还在处理尾段——注释原话即是如此。
  2. 调度器 `PeriodicScan` 每 60s 扫一次，intent 已写入且尚未 blocked 时也会被选中。
  3. 尾段上传完成 → `SegmentPersisted`（`upload.rs:1353`）；pipeline 排空 → `upload.rs:398`
     的 `DownloadClosed`。这两路才是正常放行路径。
- 门禁 Blocked 分支（`upload.rs:795-828`）的 `warn!` **无条件**打印，`notify_alert` 受
  `changed` 门控；`changed` 由 `claim_complete_session` 比较 `blocked_signature` 得出，
  首次阻塞时前值为 NULL，所以**首次阻塞必发告警**「投稿已暂停：存在未完成分段」。
- 被阻塞的会话 10 分钟后才重新参与扫描（`BLOCKED_RECHECK_INTERVAL`）。如果 291 真卡住，
  24h 该有 ~140 条同指纹 WARN；实际每个 session 各 1 条 → 都在下一轮复查前收敛。
- 三层兜底都在：`next_submit_at IS NULL` 视为到期；blocked 每 10 分钟复查；
  `recover_stale_upload_attempts` 每 60s 把失主 `uploading` 行打回 `failed`。

## 目标

下播边界上「所有未完成行都还在飞（`pending`/`uploading`）」的首轮阻塞不再产生 WARN 和
webhook 告警；真正值得人看的阻塞（有 `failed`/`source_missing`/`deleting`/`unknown` 行，
或在飞行状态停留超过一轮复查间隔）保持现有 WARN + 告警行为不变。

## 最小方案

判定放在 `claim_complete_session`（`upload_session.rs:509`）内，因为它在同一事务里已经
读 `upload_session` 并写 `blocked_signature`，外层不需要再查库：

1. 定义 **in-flight-only**：`failed + source_missing + deleting + unknown == 0`。
2. 定义 **quiet**：in-flight-only 且 `now - submit_requested_at < BLOCKED_RECHECK_INTERVAL`
   （10 分钟）。用下播时刻而不是 `blocked_count`/触发源做「是不是刚下播」的判据——
   `blocked_count` 会被多段在飞时的多次 `SegmentPersisted` 抬高，触发源则区分不了
   60s 的 `PeriodicScan` 和 10 分钟后的复查。
3. quiet 时**不写 `blocked_signature`**（仍写 `submit_state`/`blocked_count`/`updated_at`，
   让 10 分钟延后复查照常生效）。这样 10 分钟后若仍在飞，复查那次 `changed` 才为 true，
   告警不会因为首轮被静默就永久丢失。
4. `SubmitClaim::Blocked` 加一个 `quiet: bool`；`upload.rs` Blocked 分支 quiet 时用
   `info!`、跳过 `notify_alert`，其余逻辑不动。`submission_decided` 仍记 `waiting`。

## 不做

- 不删 `finish()` 里的 `DownloadClosed` spawn：删了 60s 的 `PeriodicScan` 照样会撞上尾段，
  噪音只是换了触发源；durable intent + 立即 kick 是 uploader 死掉时的快路径。
- 不在 `SegmentPersisted`/`finish()` 里预判「还有兄弟行在飞就不 kick」：同上，调度器那一路
  绕不开，判定必须落在门禁本身。
- 不改 stale lease 口径、不改 10 分钟复查间隔。
- 不做评论里的 B（v1 行无自动收敛）。评估见下。

## 评论里的 B、C 改动量评估

**C（无 token 的 `uploading` 行没有人工出口）——小，并入 step 01。**
`stop_missing_segment_attempt`（`upload.rs:4401`）遇到 `attempt_token IS NULL` 直接返回
`NotRunning`；补一条 CAS 即可：`UPDATE ... SET status='failed', last_error=?, next_retry_at=?
WHERE id=? AND status='uploading' AND attempt_token IS NULL` → `Stopped`。无注册 attempt 可取消，
所以不走 `cancel_registered_attempt`。约 12 行 + 1 个单测，HTTP 入口 `endpoints.rs:1242` 不用动。
若遇到的是进程内仍在跑的 legacy 上传，完成时 `mark_retry_success` + `update_all_fields` 会整行
覆写回 `succeeded`，强停不会造成状态损坏。

**B（v1 `uploading` 行无自动收敛）——不小，不并入。**
根因是 legacy 手动恢复路径把 `uploading` 当 claim 用但没有 lease：不写 token、不注册 attempt、
上传中不心跳，进程死掉就留下无人认领的行；`ensure_missing_segment_session` 还会把它绑进一个
session，门禁于是能看到它。三种修法都不是几行：
- 回收器按 `updated_at` 年龄回收无心跳行 = 恢复被刻意废掉的「五分钟一刀切」，会把合法的长
  传切成重复上传；
- legacy 路径改走 `claim_enrolled_attempt` = 手动恢复时临场做 v1→v2 升格，而这正是
  `lifecycle_backfill` 用一整套去重/冲突规则才敢做的事；
- 给 legacy 路径补心跳 = 在已标记淘汰的代码上再加一套 lease。

C 落地后 B 的卡死有了人工出口（停 → `failed` → 重试或删除），紧迫性下降。是否值得做自动收敛，
先在生产只读核一下还有没有非终态 v1 行：
`SELECT status, COUNT(*) FROM upload_missing_segment WHERE lifecycle_version = 1
AND status NOT IN ('succeeded') GROUP BY status`。为 0 则 B 不做；非 0 再另开 issue。

## 拆解

| step | 内容 | 依赖 | 状态 |
| --- | --- | --- | --- |
| [01](steps/01-quiet-boundary-block.md) | 门禁 quiet 判定 + 日志/告警降级 + 无 token `uploading` 行可强停（C） + 测试 | — | complete |

## 完成标准

- 单测：下播 10 分钟内、仅 `uploading`/`pending` 行阻塞 → `Blocked{quiet:true}`，
  `blocked_signature` 仍为 NULL；同一会话 10 分钟后再阻塞 → `quiet:false`、`changed:true`。
- 单测：任一 `failed`/`source_missing` 行 → 无论时间都 `quiet:false`，`changed` 行为与现状一致。
- dev 环境实录一场短直播，下播日志里门禁那条为 `INFO`，无「投稿已暂停」webhook，
  会话最终 `finalized` 且有 aid。
- 单测：`lifecycle_version=1`、`status=uploading`、`attempt_token IS NULL` 的行调 `stop_missing_segment_attempt`
  → `Stopped`，行变 `failed` 且 `last_error` 含 reason；`attempt_token` 非空的行行为不变。
- issue 严重度按「预期行为 + 告警噪音」下调。
