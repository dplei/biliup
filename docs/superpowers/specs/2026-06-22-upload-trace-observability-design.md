# 录播上传 Trace 链路与可观测性 Design

> 日期：2026-06-22
> 方案：方案1（trace 关联日志）+ (b) 会话表持久化投稿状态 + 缺失补传页筛选

## Goal

让一次录播从「分段上传 → 落库 → 补传 → 下播一次性投稿 → 拿 aid → 加合集」的完整生命周期**可被一条 id 串起来定位**，并把当前**静默吞掉的投稿异常**（投稿成功但没拿到 aid、写回 aid 失败）显式记录、持久化、可查。同时修复「缺失补传」页面补传成功后记录从列表消失、无法追踪去向的问题。

## Context / 问题现状

现役上传链路（`crates/biliup-cli/src/server/common/upload.rs`，方案B「下播一次性提交」）存在以下可观测性盲区：

1. **投稿成功但无 aid 被静默吞掉**：`submit_by_app`（`crates/biliup/src/uploader/bilibili.rs:359`）仅在 `code==0` 返回 `Ok`；`submit_session`（`upload.rs:195`）从 `data.aid` 取 aid，取不到时只打一行 `error!("提交响应缺少 aid")` 后**照常返回 `Ok(())`**。调用方无感，会话留在 `uploading`，下次开播可能被 `prepare_archive` 当废弃会话**重复投稿**。
   - 已核实：投稿接口正常 `code==0` 时必返回 `data.aid` + `data.bvid`（现役 Rust、遗留 Python `bili_webup_sync.py:446` 直接 `ret['data']['aid']` 无兜底、网上抓包资料三处一致）。因此「成功却无 aid」属异常响应（风控、结构漂移等），本不该发生，恰是最需要捕捉的。

2. **手动补传无日志、无去向追踪**：`manual_recover_missing_segment`（`upload.rs:791`）成功路径里，「插回待提交会话」（`insert_session_video_at_order`）与「追加到已投稿件」（`edit_by_app`）两条分支**都没有成功日志**，仅有上传文件本身的两行日志。

3. **补传成功记录从列表消失**：补传成功调 `mark_retry_success` 置 `status='succeeded'`（`missing_segment.rs:67`），而列表接口 `get_missing_uploads`（`endpoints.rs`）写死 `status IN ('pending','failed','uploading')`，`succeeded` 行被过滤，UI 无法追踪。

4. **无跨生命周期关联 id**：现仅有 `tracing` 控制台日志（`main.rs:42`）+ 一个 cookie-health webhook，没有把一场录播所有日志行串起来的 id。

## 决策记录

- trace_id 载体：**复用 `upload_session.id`**（per-session 粒度），零新增 id 字段；跨重启 / reattach / 下次开播补提交都能从库读回同一 id。
- 关联方式：**`tracing` span**（自动给下游所有日志行带 `session=<id>`），非逐行手加字段。
- 落地程度：**(b)** 在 `upload_session` 上加轻量列持久化投稿状态；**不**新建独立事件表（留作以后演进）。
- 本期只**记录**异常，不改投稿 / 防重复投稿的决策逻辑；不动内存 archive 与 DB 的一致性问题（另案）。

## 架构与组件

### 组件 1：Session Span（关联日志）

**做什么**：给每场录播的所有日志行打上 `session=<upload_session.id>`，使一条 id 能在「实时日志」里 grep 出整条链路。

**怎么用**：
- `process_with_upload`（`upload.rs:71`）顶部声明 span，session 字段先留空：
  ```rust
  let span = tracing::info_span!("session", session = tracing::field::Empty);
  ```
  待 `prepare_archive` 返回、拿到 `archive.session_row_id` 后：
  ```rust
  span.record("session", id);
  ```
  用 `.instrument(span)` 包住整条流水线（`pipeline_upload_videos` / `recover_due_missing_segments` / `submit_session`），下游日志自动继承字段。
- 手动补传 handler `manual_recover_missing_segment`（`upload.rs:791`）单独开 span：`info_span!("session", session = row.upload_session_id)`，使手动补传日志与同一会话对齐。

**依赖**：`tracing` / `tracing-futures`（`Instrument`）。无 schema 依赖。

### 组件 2：生命周期显式日志点

**做什么**：在当前静默的关键节点补显式日志行（依赖组件 1 的 span 自动带 `session`）。

清单（event 名为日志语义，非强制字符串）：

| 阶段 | 事件 | 级别 | 现状 | 字段 |
|------|------|------|------|------|
| 投稿 | submit_attempt | info | 无 | n_videos, title |
| 投稿 | submit_ok_with_aid | info | 无 | aid, bvid |
| 投稿 | submit_ok_no_aid | **error** | 有（混在普通 error） | resp 摘要 |
| 投稿 | aid_writeback_ok / fail | info / error | 仅失败有 | aid |
| 投稿 | season_add_ok / fail | info / error | 有 | aid, section_id |
| 补传 | missing_enqueue | error | 有 | segment_order |
| 补传 | silent_recover_ok / fail | info / error | 有 | row_id |
| 补传 | manual_recover_to_session | info | **无** | session, segment_order |
| 补传 | manual_recover_edit_archive | info | **无** | aid, segment_order |
| 分段 | Upload completed | info | 有 | （仅靠 span 带 session，不新增行） |

**依赖**：组件 1。

### 组件 3：upload_session 投稿状态持久化（方案 b）

**做什么**：把投稿异常持久化到会话行，使「投稿成功却无 aid」「写回失败」不随日志滚动丢失、可查、可作未来防重复投稿钩子。

**数据模型**：迁移 `crates/biliup-cli/migrations/6_add_session_submit_trace.sql`，给 `upload_session` 加列：
- `submit_attempts INTEGER NOT NULL DEFAULT 0` — 投稿尝试次数。
- `last_submit_at DATETIME` — 最近一次投稿时间。
- `last_submit_error TEXT` — 最近一次投稿异常摘要（成功且有 aid 时清空 / 置 NULL）。
- `submit_state TEXT` — `ok_with_aid` / `ok_no_aid` / `failed`；NULL = 未投。

对应 `UploadSession` ORM 模型（`models.rs:66`）补 4 个字段（`submit_attempts: i64`、`last_submit_at: Option<DateTime<Utc>>`、`last_submit_error: Option<String>`、`submit_state: Option<String>`）。

**写入点**：`submit_session`（`upload.rs:195`）每次投稿后：
- 接口失败（`submit_to_bilibili` 返回 Err）→ `submit_state='failed'`，记 `last_submit_error`，`submit_attempts += 1`。
- `code==0` 且有 aid → `submit_state='ok_with_aid'`，清 `last_submit_error`（由 `mark_submitted` 顺带写，扩展该函数 `upload_session.rs:156`）。
- `code==0` 无 aid → `submit_state='ok_no_aid'`，记 `last_submit_error`（保持 `uploading`，待下次补提交）。

**纯函数（便于 TDD）**：抽一个 `submit_state_from_response(resp: &ResponseData) -> SubmitState`（或等价），把「resp → ok_with_aid / ok_no_aid / failed」判定逻辑独立、可单测。

**依赖**：迁移 + 模型；被组件 2 的投稿日志共用判定结果。

### 组件 4：缺失补传页 status 筛选（追踪闭环）

**做什么**：让「缺失补传」页能查看已补传成功的记录及其去向。

**后端**：`get_missing_uploads`（`endpoints.rs`）加可选 query 参数 `status`：
- 缺省 / `active` → 现状 `status IN ('pending','failed','uploading')`。
- `succeeded` → `status = 'succeeded'`。
- `all` → 不限 status。
- 排序维持 `created_at DESC`。

**前端**：`app/(app)/missing/page.tsx`：
- 顶部加状态下拉（Semi-UI `Select`）：`待补传`（默认，对应 active）/ `已补传`（succeeded）/ `全部`（all）。
- SWR key 带参数：`/v1/uploads/missing?status=<sel>`，切换时重新拉取。
- 表格按筛选动态列：
  - 「已补传」「全部」时增加**去向**列：`aid != null` → 「已追加到稿件 av{aid}」（链接 `https://www.bilibili.com/video/av{aid}`）；`aid == null && upload_session_id != null` → 「已补进待提交会话 #{upload_session_id}」。
  - **完成时间**列用 `updated_at`（已有字段，`fmtTime`）。
- `#{upload_session_id}` 即 trace_id，提示用户可拿它去「实时日志」grep 整条链路。

**依赖**：复用既有 `upload_missing_segment` 表，无新表。

## 数据流

```
录制分段 ──► upload_single_file ──► persist_segment ──► upload_session(uploading)
   │ (失败)                                                     │
   └─► enqueue_missing_segment ──► upload_missing_segment       │
                                                                ▼
下播 ──► recover_due_missing_segments ──► submit_session ──► [submit_state 落库 + 显式日志]
                                              │                  ├─ ok_with_aid → mark_submitted(finalized, aid)
                                              │                  ├─ ok_no_aid   → 保持 uploading + error 日志
                                              │                  └─ failed      → 保持 uploading + error 日志
手动补传(HTTP) ──► manual_recover_missing_segment
        ├─ 会话有 aid → edit_by_app ──► [manual_recover_edit_archive 日志]
        └─ 会话无 aid → insert_session_video_at_order ──► [manual_recover_to_session 日志]
        └─► mark_retry_success(succeeded) ──► 缺失补传页「已补传」筛选可见 + 去向
```

全程所有日志行经 Session Span 带 `session=<upload_session.id>`。

## 错误处理

- span `record` 在 session 尚未创建（首段未落库前无 id）时字段为空，属正常——空会话不投稿。
- `submit_state` 写库失败仅告警，不影响投稿主流程（稿件已在 B 站，重复提交风险大于收益，沿用现有 `mark_submitted` 写回失败的处理策略）。
- 前端 status 参数非法值由后端归一到 `active`，不报错。

## 测试

TDD，先写失败测试：
- Rust `submit_state_from_response` 纯函数：`code==0 + data.aid` → ok_with_aid；`code==0` 无 aid → ok_no_aid；构造失败响应 → failed。
- Rust `get_missing_uploads` status → SQL where 分支映射（active / succeeded / all）可抽纯函数测 where 片段。
- 前端：筛选下拉切换拉取、去向列渲染（aid 链接 / 会话 #id）手动验证。

## 不做（YAGNI）

- 不新建独立事件表（`upload_trace_event` 之类），留作以后真有需要时的演进（原方案 c）。
- 不改投稿 / 防重复投稿决策逻辑——本期 `submit_state` 只记录，`ok_no_aid` 仍保持 `uploading` 行为不变。
- 不修内存 `archive.videos` 与 DB `videos_json` 在「直播进行中手动补传」时的潜在不一致（另案）。
- 不接 webhook 告警（本期纯日志 + 持久状态；告警是以后增量）。

## 涉及文件

- 新增 `crates/biliup-cli/migrations/6_add_session_submit_trace.sql`
- 改 `crates/biliup-cli/src/server/infrastructure/models.rs`（`UploadSession` 加 4 列）
- 改 `crates/biliup-cli/src/server/common/upload_session.rs`（`mark_submitted` 扩展 + 投稿状态写入 helper）
- 改 `crates/biliup-cli/src/server/common/upload.rs`（Session Span、显式日志点、`submit_session` 写 submit_state、手动补传日志、`submit_state_from_response` 纯函数）
- 改 `crates/biliup-cli/src/server/api/endpoints.rs`（`get_missing_uploads` status 过滤）
- 改 `app/(app)/missing/page.tsx`（状态下拉、去向列、完成时间列）
