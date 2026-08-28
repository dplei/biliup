# 07 — 事故回归与端到端验证

Status: ready-for-human
Blocked by: 03, 04, 05, 06

## 背景

现有可靠性测试重点验证“不完整会话绝不能投稿”，没有验证相反的活性性质：“已经要求投稿的会话在
账本后来完整时必须最终投稿”。本任务把两次生产事故固化为可重复的自动化回归，并完成上线前验证。

## 自动化场景

1. **正常竞态**：四段会话，首次协调时最后一段 uploading，断言 blocked 且远端调用为 0；最后一段
   `persist_segment` 后，断言最终 exactly-once 投稿、四个 filename 顺序正确、状态 finalized。
2. **重启竞态**：尾段已 durable enrollment，写入投稿意图后模拟进程退出；尾段由恢复任务成功，新的
   启动扫描完成投稿。
3. **历史 NULL 会话**：全部 succeeded、无投稿意图、`submit_state=NULL`；自动扫描不得提交，调用人工
   recover 后必须提交。
4. **活跃直播反例**：当前所有分段 succeeded 但没有投稿意图；多轮扫描后远端调用仍为 0，后续 enrollment
   仍能写入同一 session。
5. **并发唤醒**：下播、最后分段、人工 recover、周期扫描同时触发，断言只有一个 submit claim、一个远端
   请求和一个 BV。
6. **失败退避**：明确远端失败释放 claim 并设置 next time；到期前不重试，到期后仅重试一次。
7. **不确定结果**：`ok_no_aid`、远端成功后写回失败或预置 claim；自动协调器不重复投稿。
8. **阻塞可见性**：source_missing/unknown 等不能完成的会话在 API 与页面显示明确原因。

## 测试设施

- 优先扩充 `crates/biliup-cli/tests/upload_reliability_incident.rs`，复用既有 incident DB fixture。
- 使用可注入 submit spy/fake，禁止自动化测试触碰真实 B 站。
- 使用 fake clock 控制 `next_submit_at`，避免 sleep。
- 对远端副作用以调用次数和 payload 中的有序 filename 双重断言。

## 验证步骤

1. `SQLX_OFFLINE=true cargo check --workspace`。
2. `cargo test -p biliup-cli`，重点报告上述新增 incident cases。
3. `cargo test --workspace`。
4. `tsc --noEmit`、`next build`。
5. 本机 dev 环境用不触碰真实投稿的 fake/测试模式复演 11 秒竞态并查看页面状态流转。
6. 若进入部署阶段，先备份生产 SQLite；migration 后只读检查历史 blocked 回填范围，不批量改变 NULL 会话。
7. 发布后观察至少一场多分段直播：确认正常投稿、无重复稿件、待投稿扫描无异常高频重试。

## 验收

- 所有自动化场景稳定通过，不依赖真实时间和外网。
- migration 的生产只读检查结果与预期一致。
- 一场真实多分段直播正常产生单一稿件；日志能串起
  `submit_requested -> blocked/ready -> claimed -> submitted`。
- 本任务只在验证完成后标记 resolved，并把提交、测试和部署证据写入 `## Answer`。

## Answer

自动化与本地构建部分已完成。`upload_reliability_incident.rs` 新增四组完全无外网的投稿活性回归，
覆盖：尾段晚到时首次 blocked、四路并发唤醒 exactly-once、四个 filename 按 `segment_order` 重建并
finalize；重启扫描接管持久投稿意图；历史 NULL 会话只在人工授权后进入扫描；活跃无意图会话多轮扫描
不投稿且仍可继续 enrollment；明确失败到期前/后选择；`ok_no_aid` 保留 claim 并永不自动重试。
API 单测另覆盖 waiting/ready/submitting/retry/manual 五态和 source_missing/unknown 等阻塞文案。

已通过 `SQLX_OFFLINE=true cargo check --workspace`、`cargo test -p biliup-cli`、
`cargo test --workspace`、`npx tsc --noEmit`、`npm run build`、格式检查与 Code Index 校验；测试模式不读取
Cookie、不触碰真实 B 站。

仍需人工/部署授权的验收：备份生产 SQLite 后部署 migration 20，执行只读历史数据核对，并观察至少一场
真实多分段直播只产生一个稿件。未执行这三项生产操作，因此按约定保持 `ready-for-human`，不虚报
`resolved`。

### 2026-08-28 本机 dev 环境验证（步骤 5、6，以及步骤 7 的 pre-deploy 部分）

**步骤 6（生产数据只读核对）已完成，结论是 migration 19 的回填为空操作。** 对生产
`data/data.sqlite3` 只读查询：`submit_state='blocked_missing_segments'` 且未 finalized 的会话
**0 行**，因此迁移不会回填任何 `submit_requested_at`，首次启动扫描不存在批量投稿风险。生产库现状
230 finalized / 2 uploading，199 `ok_with_aid` / 33 `NULL`，`upload_missing_segment` 全为
`succeeded`。两条 uploading 会话（id 232、233，`submit_state=NULL`，创建于 2026-08-28T02:51，相差 1 秒）
属于 2.6 节「不得批量猜测、只能人工 recover」那一类，迁移刻意不动它们。

**步骤 7 的真实多分段直播已在本机 dev 环境完成，一场直播只产生一个稿件**
（`is_only_self=1`）。抖音真实直播，origin 档 flv 15Mbps，`segment_time` 临时改为
3 分钟以复现竞态，5 个分段全部经 `bda2` 上传成功。为稳定命中竞态窗口，在尾段仍在 transferring 时
人工暂停房间触发生产端关闭，而非等待自然下播；关闭边界与自然下播共用同一条路径。

日志串起了完整的 `submit_requested -> blocked -> claimed -> submitted`：

```
15:49:41  segment validated and enrolled  missing_id=11 session=2 segment_order=4
15:49:41  Download workflow completed => Pause
15:49:41  submit_requested_at = 2026-08-28T07:49:41.323313+00:00
15:49:41  session submit blocked by incomplete lifecycle ledger  incomplete=2 pending=1 uploading=1
15:49:41  会话投稿协调唤醒完成  trigger=DownloadClosed  outcome=Blocked{total_expected:5, succeeded:3}
15:50:27  trigger=SegmentPersisted  outcome=Blocked{succeeded:4, uploading:1}
15:51:01  submit_attempt：开始下播一次性投稿  n_videos=5  trigger="segment_persisted"
15:51:02  code:0  投稿成功（aid/bvid 略）
15:51:02  会话投稿协调唤醒完成  trigger=SegmentPersisted  outcome=Submitted
```

blocked 到自动投出间隔 80 秒，无人干预、不依赖下一场直播，`submit_attempts=1`、
`submit_retry_attempts=0`，全库无重复稿件。分 P 标题按 `segment_order` 严格递增
（15_36_40 / 15_39_37 / 15_42_37 / 15_45_37 / 15_48_37），且取原始录像名而非
`audio-normalized-<hash>.part` 中间件名——尾段走的正是补传调度器这条最易泄漏的路径。

**整体验收 4（活跃直播不得提前投稿）在真实直播上直接观测到**：15:44:55 与 15:45:27 两个采样点
账本「暂时完整」（已录分段全 succeeded、无分段在传），周期扫描每 60 秒运行，全程
`submit_requested_at IS NULL`，零投稿。

**步骤 5（页面状态流转）已完成。** 因真实会话 80 秒即投完、中间态无法驻留，改为在本地库造 5 条
一次性会话覆盖五态；为防止 `ready_to_submit` 被扫描器真的投出去，验证期间临时移走 B 站 cookie，
结束后已还原。补传页「待投稿会话」区域标注「独立于下方缺失分段筛选」，五态渲染互不相同：

| 会话 | action | 页面标签 | 操作按钮 |
| --- | --- | --- | --- |
| 901 | `waiting_segments` | 等待分段 | 查看阻塞分段 #13 / 恢复会话 |
| 902 | `ready_to_submit` | 待投稿 | 恢复会话 |
| 903 | `submitting` | 投稿中 | 无 |
| 904 | `manual_inspection` | 需人工核对 | 无，并显式提示「为避免重复稿件，此状态不提供普通重试按钮」 |
| 905 | `retry_scheduled` | 退避重试 | 恢复会话，附最近错误与下次重试时间 |

同一轮扫描日志 `candidates=[901, 902]` 证明 903（claim + submitting）、904（`ok_no_aid`）、
905（退避未到期）**未进入候选**，对应整体验收 6 与退避语义。

验证副产物：`completeness.valid_videos` 来自分段行的 `video_json` 列而非会话的 `videos_json`，
造数据时两者都要写才会判为完整。另发现补传页在 `next dev` 下有既有的 React hydration mismatch
（`page.tsx:211` 的 `toLocaleString` 与 `:256` 的 `useState(Date.now())`，分别引入于 fdf41302 与
558ac791），非本轮回归，`next build` 通过，已另开任务跟踪。

**仍未完成、需要部署授权的部分**：备份生产 SQLite 后部署 migration 19+20，并在生产观察至少一场
多分段直播。因此本任务继续保持 `ready-for-human`。
