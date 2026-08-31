# Spec：补扫不得复活零分段空会话

Status: resolved（随 `2c1b871` 合入 `dev`）

来源：[`dplei/biliup#3`](https://github.com/dplei/biliup/issues/3)

分支：`dev`（原 `codex/fix-rescan-empty-sessions`）

本文记录已经确认的根因、修复边界与任务拆分。实现 ticket 位于 [`issues/`](./issues/)；本轮不依赖
现场遗留录像，空目录、临时 SQLite 和测试生成的媒体夹具足以覆盖主路径。

---

## 1. 问题一句话

`POST /v1/uploads/missing/rescan` 在确认存在可恢复文件之前就创建 `upload_session`。历史场次的源文件
已经被后处理删除时，补扫仍留下一个没有任何 lifecycle row 的空壳；它随后被投稿协调链路视为上一场
遗留会话，永久停在 `blocked_missing_segments`。

## 2. 已确认的完整因果链

1. `rescan_local_valid_segments` 找不到活动会话或 finalized 会话时，先调用
   `insert_uploading_session(..., &[])`。
2. 会话创建完成后，函数才从 `filelist` 和工作目录收集候选，并用 `FileValidator` 验证。
3. 当候选文件已消失、已登记或无效时，`queued=0`，但新建会话不回滚。
4. 空会话超过恢复窗口后，下一次上传管道会把它作为 stale session 请求投稿；人工“恢复会话”也会
   写入同样的持久投稿意图。
5. `SessionCompleteness::is_complete` 要求 `total_expected > 0`，零行会附加
   `session has no lifecycle baseline`，所以投稿闸门写入 `blocked_missing_segments`。
6. 周期投稿扫描每次重新检查都会执行 `blocked_count + 1`，但数据库状态永远不可能自行改善。
7. 页面只有“恢复会话”，没有安全终结入口；硬删除会话又会重新制造孤儿 `streamerinfo`，下一次补扫
   仍可能复活它。

本次事故跨过 migration 19：迁移把历史 `blocked_missing_segments` 回填成持久投稿意图，随后周期
协调器开始稳定重试。迁移本身按当时的不变量是合理的，真正破坏不变量的是补扫创建了零基线会话。

## 3. 目标

1. 补扫在“没有有效且未登记的本地分段”时对 `upload_session` 和
   `upload_missing_segment` 保持零副作用。
2. 第一条有效分段登记时才创建或复用会话，并保持现有 enrollment 的幂等与并发安全。
3. 已有的关闭态零基线会话能够单次收敛为可审计终态，不进入周期重试。
4. 页面提供受保护的“丢弃空会话”操作；丢弃只做逻辑终结，不硬删除身份。
5. 补扫结果和结构化日志明确区分：无有效候选、复用会话、新建会话、跳过 finalized 会话。
6. 用临时目录和临时数据库锁定回归，不要求生产录像或生产数据库。

## 4. 非目标

- 不恢复已经被 `postprocessor: ["rm"]` 删除的录像。
- 不自动合并历史 `streamerinfo` 或远端稿件。
- 不批量终结所有 `0/0` 会话；刚开始录制、首段尚未 enrollment 的会话也可能暂时为零。
- 不修改已应用 migration 19 的字节，也不以新 migration 猜测所有历史行的业务含义。
- 不把“丢弃会话”实现成数据库 `DELETE`；必须保留 finalized 身份，防止补扫再次复活。

## 5. 设计决策

### 5.1 补扫改成“先发现，后变更”

补扫分两阶段：

```text
只读阶段：加载场次/主播 → 查现有会话 → 收集候选 → 排除已知项 → 验证媒体
                                           │
                                           ├─ 0 个有效未知候选：返回，不创建会话
                                           └─ ≥1 个：逐条走 durable enrollment
```

- 不采用“先建、扫完为零再删”的补偿方案。创建与删除之间存在崩溃窗口，并可能和录制 enrollment
  并发；失败后仍会留下相同空壳。
- 首条有效候选直接交给 `enroll_validated_segment`，由 enrollment 事务创建或复用会话。补扫不得再
  维护一套独立的会话创建规则。
- 已存在活动会话时，即使本轮没有新候选，也只返回该会话，不改变其状态。
- 已有 finalized 会话继续短路并写 `rescan_skipped_finalized_session` 审计。
- 当既无已有会话又无有效候选时，结果中的 `upload_session_id` 应允许为 `null`；前端提示“未发现可
  登记分段”，不能伪造会话号。

### 5.2 零基线只在“本场已关闭”后终结

不能把所有 `total_expected == 0` 直接 finalized。安全的自动终结至少要求：

- `status != 'finalized'`；
- `submit_requested_at IS NOT NULL`，证明生产端已关闭或操作员明确请求协调；
- 没有 `upload_missing_segment` 行；
- `videos_json` 为空，`aid`/`bvid` 为空；
- 没有 `submit_claim_token`。

检查与状态更新必须在同一个 `BEGIN IMMEDIATE` 事务内。终结结果使用明确的
`submit_state='discarded_empty'`，清空重试/阻塞字段，保留 `upload_session` 与
`streamer_info` 的关联。这样 finalized 查询会阻止同一历史场次被补扫再次创建。

投稿协调器收到该结果后正常返回，不调用 B 站接口，不发送“存在未完成分段”告警，也不再被周期扫描
选中。

### 5.3 人工丢弃复用同一个终结原语

新增会话级丢弃接口，只允许终结满足严格空会话条件的行。接口不执行物理删除：

- 有任何 lifecycle row：`409`，提示先处理具体分段；
- 已有 aid/bvid 或 submit claim：`409`，防止覆盖远端结果不确定状态；
- 已 finalized：幂等返回当前终态；
- 条件满足：调用与自动收敛相同的事务函数，写 `discarded_empty`。

页面只在 `total_expected == 0`、无 claim、无远端稿件标识时显示按钮，并要求二次确认。普通“恢复会话”
对零基线会话不再作为唯一出口。

### 5.4 历史数据采取显式收敛，不做宽泛 migration

已知空壳可通过新接口或带严格谓词的维护 SQL 收敛。不要新增“所有 0/0 都 finalized”的迁移，因为
数据库快照无法证明每个无投稿意图的空会话是否仍属于活动录制。

## 6. API 与可观测性

建议调整：

- `POST /v1/uploads/missing/rescan`：`upload_session_id` 改为可空，并返回
  `valid_candidates`/`created_session`/`queued` 等明确计数。
- 新增 `DELETE /v1/uploads/sessions/{id}` 或语义等价的
  `POST /v1/uploads/sessions/{id}/discard-empty`。若使用 `DELETE`，文档必须说明这是逻辑终结，
  不是物理删除。
- 补扫零产出时记录结构化日志：`streamer_info_id`、`scanned`、`valid_candidates=0`、
  `created_session=false`；不得打印账号或录像业务标识。
- 丢弃时记录 session id、触发来源（automatic/manual）和终结原因，不记录生产文件路径。

## 7. Ticket 与依赖

| # | Ticket | 目标 | Blocked by |
| --- | --- | --- | --- |
| [01](./issues/01-lazy-rescan-session-creation.md) | 补扫延迟创建会话 | 零有效候选时数据库零副作用 | — |
| [02](./issues/02-empty-session-terminal-state.md) | 零基线会话终结原语 | 关闭态空会话不再无限 blocked | — |
| [03](./issues/03-discard-empty-session-ui.md) | 会话丢弃 API 与 UI | 给操作员安全、可审计的出口 | 02 |
| [04](./issues/04-regression-and-history-verification.md) | 事故回归与历史验证 | 锁定跨版本链路并验证不误伤 | 01, 02, 03 |

01 与 02 可以并行；03 复用 02 的事务原语；04 最后统一收口。

## 8. 整体验收

1. 孤儿 `streamerinfo`、空工作目录、无现存会话：补扫返回零产出，数据库不新增会话。
2. `filelist` 指向已删除文件：同样不新增会话。
3. 目录只有 header-only/低于阈值媒体：只计入 invalid，不新增会话。
4. 存在一条有效未知媒体：恰好创建/复用一个会话并登记一条 lifecycle row；重复补扫保持幂等。
5. 已关闭且零基线的历史会话只终结一次，周期扫描不再选择，`blocked_count` 不再增长。
6. 活动录制中、尚未产生首段且没有投稿意图的会话不得被自动终结。
7. 人工丢弃有 lifecycle row、远端标识或 claim 的会话返回 `409`；合法空会话逻辑终结后从待投稿页
   消失，之后补扫仍识别其 finalized 身份。
8. `cargo test -p biliup-cli` 与前端检查全绿，工作区不需要任何生产录像夹具。

## Comments

- 2026-08-30：已根据 Issue #3 的数据库时间线与当前 `dev` 代码完成根因定位；现有
  `local_rescan_reuses_current_session_and_rejects_thirteen_byte_flv` 只覆盖“已有活动会话”，缺少
  “孤儿场次 + 零有效候选”回归。
- 2026-08-30：计划已在 `codex/fix-rescan-empty-sessions` 实现并通过后端全量测试、前端 lint/typecheck
  与代码索引校验。已于 2026-08-31 随 `2c1b871` 落到 `dev`，effort 结项归档。
