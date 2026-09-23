# 投稿与分段上传的自动重试上限

来源：[dplei/biliup#76](https://github.com/dplei/biliup/issues/76)。发起时版本 1.3.38。

Status: ready-for-human（[#78](https://github.com/dplei/biliup/pull/78) 已合入，1.3.39 发版，dev 验证通过，待生产验证）

关联：#75（`.scratch/timestamp-start-skew/`，触发本题的那场 21588 拒稿）。

> 单点修复，一轮做完，不拆 `steps/`。

## 一句话

两处自动重试都没有上限，也不分失败是否会自己好：投稿被内容类错误拒掉后每 ≤30 分钟原样重投一次，
分段上传失败后固定每 10 分钟重试一次；人工处理完想立即重投，「恢复会话」又越不过退避，只能改库。

## 方案

### 投稿（`upload.rs` `retry_submission` / `upload_session.rs`）

| 失败 | 处理 |
|---|---|
| 远端拒绝，码在 `FINAL_REJECTION_CODES`（21588） | 立即停止自动重投 |
| 其余远端拒绝（含 21566） | 普通退避；连续第 `MAX_SUBMIT_REJECTIONS`（8）次停止，约首次失败后 1.5 小时 |
| 本地前置失败（登录、模板、封面） | 不计入上限，照旧无限退避：不发远端请求，没有浪费 |
| 远端结果不确定 | 不变：保留 claim，禁止自动重投 |

停止 = `submit_state = 'held'`、`next_submit_at = NULL`、释放 claim、告警。调度扫描排除它，就绪预检
返回 `Held`，分段成功的唤醒也不会再投。只收实测确认过的拒绝码，不猜。

「恢复会话」先 `rearm_session_submit`：`next_submit_at = now`、`submit_retry_attempts = 0`、`held → failed`。
`ok_no_aid` / `submitting` / `unknown_remote_result` 与持有 claim 的会话不动。

### 分段上传（`fail_enrolled_attempt_with_outcome` / `recovery_scheduler.rs`）

- v2 行改用 `retry_delay_for_attempt`（10m → 30m → 1h → 2h → 6h），不再固定 10 分钟。
- 自动路径（周期扫描 `due_rows(None)`、开播静默恢复 `due_missing_segments_for_session`）只取
  `attempts < MAX_AUTO_UPLOAD_ATTEMPTS`（6，约 10 小时）的行。
- 人工路径越过退避与上限：单段重试本来就会把 `next_retry_at` 拉到现在；「恢复会话」的
  `due_rows(Some(id))` 去掉了到期条件。
- 时间戳 `Unfixable`：错误带 `UNFIXABLE_UPLOAD_ERROR` 前缀，落库时 outcome 记 `unfixable`、
  `attempts` 直接拉到上限。每次尝试的真实记录仍在 `upload_attempt` 历史表里。

用次数表达「停止」而不是加列：不需要 migration，页面由接口给出 `auto_retry_stopped`。

### 页面

- 待投稿会话新增动作 `held`（「已停止自动重投」，红色）。
- 分段行 `auto_retry_stopped` 时显示「已停止自动重试，需手动重试」，不再显示一个不会到来的下次时间。
- 尝试历史新增 outcome `unfixable`。

## 验证

- `only_remote_rejections_can_stop_auto_submit`：判定表。
- `a_content_rejection_holds_the_session_until_manual_recovery`：21588 → `Held`、扫描不领取；
  `rearm` 后 `Ready`、`failed`、计数清零。
- `upload_failures_back_off_and_stop_auto_retry_at_the_cap`：连续 6 次失败的间隔为
  `[10, 30, 60, 120, 360, 360]` 分钟，之后自动恢复不再领取。
- `an_unfixable_segment_stops_auto_retry_at_once`：用上传路径同样的 `{e:?}` 落库，一次即达上限、
  历史 outcome 为 `unfixable`。
- `manual_recovery_overrides_backoff_and_the_auto_cap`：自动只取 `[1]`，人工取 `[1, 2, 3]`。
- 顺带修了 `explicit_app_21566_uses_regular_submit_backoff` 的偶发失败：上界改用调用之后的时刻。
- `cargo test -p biliup-cli -p biliup` 全绿。

## dev 环境验证（2026-09-23，库的临时副本）

- 把一个因分段缺失而阻塞的会话置 `held`（带 21588 错误），把它缺失的那一段置 `failed`、
  `attempts=6`：待投稿会话接口返回 `action=held`，分段接口返回 `auto_retry_stopped=true`；
  页面分别显示「已停止自动重投」「已停止自动重试，需手动重试」。期间周期扫描没有领取这两者。
- 点「恢复会话」：`held` 解除、`submit_retry_attempts=0`、`next_submit_at` 置为当下，协调器立即
  运行并因分段不完整停在 `blocked_missing_segments`（未发远端请求）；已用完次数的那一段被人工
  恢复领取，源文件不在，转为 `source_missing`。
- 同一环境随后真实录制 4 段并上传，下播后一次投稿成功，正常路径未受影响。

## 待验

- 生产：下一次 21588 拒稿后会话停在 `held`、收到告警、不再每 30 分钟重投。
