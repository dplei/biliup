# 录播上传 Trace 链路与可观测性 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让一次录播的上传→投稿→拿 aid 全链路可被 `session=<upload_session.id>` 串联定位，把投稿异常（成功却无 aid、写回失败）持久化到会话行，并让「缺失补传」页能筛选查看已补传记录及其去向。

**Architecture:** 复用 `upload_session.id` 作为 per-session trace_id，用 `tracing` span 让下游日志自动带该字段；在静默节点补显式日志；给 `upload_session` 加 4 列持久化投稿状态；`get_missing_uploads` 增加 status 过滤，前端加状态下拉与去向列。

**Tech Stack:** Rust 2024 workspace（`biliup-cli` 服务端）；SQLite 迁移（`sqlx`）；ORM（`ormlite`）；异步（`tokio`）；日志（`tracing` + `tracing-futures` 的 `Instrument`）；前端 Next.js + `@douyinfe/semi-ui` + `swr`。测试 `cargo test -p biliup-cli`。

## Global Constraints

- 设计文档：`biliup/docs/superpowers/specs/2026-06-22-upload-trace-observability-design.md`，本计划须与之一致。
- 本期只**记录**投稿异常，不改投稿 / 防重复投稿决策；`ok_no_aid` 仍保持会话 `uploading` 行为不变。
- 不新建独立事件表；不接 webhook 告警；不动内存 `archive.videos` 与 DB `videos_json` 一致性问题。
- trace_id 一律使用 `upload_session.id`，日志字段名固定为 `session`。
- 投稿状态取值固定三种：`ok_with_aid` / `ok_no_aid` / `failed`（未投为 NULL）。
- 不要改动仓库内已存在的无关本地改动（如 `BUILD_AND_DEPLOY.md`）。
- 测试先写、先观察失败，再写实现（TDD）。

---

## File Structure

- 新增 `crates/biliup-cli/migrations/6_add_session_submit_trace.sql` — `upload_session` 加 4 列。
- 改 `crates/biliup-cli/src/server/infrastructure/models.rs` — `UploadSession` 加 4 字段。
- 改 `crates/biliup-cli/src/server/common/upload_session.rs` — `insert_uploading_session` 补默认值；`mark_submitted` 扩展；新增 `mark_submit_anomaly`、纯函数 `submit_state_label`、`missing_status_where`。
- 改 `crates/biliup-cli/src/server/common/upload.rs` — Session span、`submit_session` 记录投稿状态 + 显式日志、手动补传成功日志。
- 改 `crates/biliup-cli/src/server/api/endpoints.rs` — `get_missing_uploads` 接受 `status` query。
- 改 `crates/biliup-cli/src/server/router.rs` — 确认 `get_missing_uploads` 路由可带 query（一般无需改，确认即可）。
- 改 `app/(app)/missing/page.tsx` — 状态下拉、去向列、完成时间列。

---

### Task 1: 迁移与模型加列（DB 层）

**Files:**
- Create: `crates/biliup-cli/migrations/6_add_session_submit_trace.sql`
- Modify: `crates/biliup-cli/src/server/infrastructure/models.rs:66-83`
- Modify: `crates/biliup-cli/src/server/common/upload_session.rs:96-105`

**Interfaces:**
- Produces: `UploadSession` 新增字段 `submit_attempts: i64`、`last_submit_at: Option<DateTime<Utc>>`、`last_submit_error: Option<String>`、`submit_state: Option<String>`。
- Produces: `InsertUploadSession`（ormlite 自动派生）随之含同名字段。

- [ ] **Step 1: 写迁移文件**

Create `crates/biliup-cli/migrations/6_add_session_submit_trace.sql`:

```sql
-- 投稿可观测性：在会话行上持久化每次下播一次性投稿的结果，使「投稿成功却无 aid」「写回失败」
-- 这类异常不随日志滚动丢失、可查可定位。submit_state 取值：ok_with_aid / ok_no_aid / failed；NULL=未投。
alter table upload_session add column submit_attempts INTEGER not null default 0;
alter table upload_session add column last_submit_at DATETIME;
alter table upload_session add column last_submit_error TEXT;
alter table upload_session add column submit_state TEXT;
```

- [ ] **Step 2: 模型加字段**

Modify `crates/biliup-cli/src/server/infrastructure/models.rs`，在 `UploadSession` 的 `updated_at` 字段之后（`models.rs:82` `pub updated_at` 行下、`}` 之前）加：

```rust
    /// 本场下播一次性投稿的累计尝试次数。
    pub submit_attempts: i64,
    /// 最近一次投稿时间。
    pub last_submit_at: Option<DateTime<Utc>>,
    /// 最近一次投稿异常摘要（成功且有 aid 时为 None）。
    pub last_submit_error: Option<String>,
    /// 投稿结果：ok_with_aid / ok_no_aid / failed；None=未投。
    pub submit_state: Option<String>,
```

- [ ] **Step 3: 插入点补默认值**

Modify `crates/biliup-cli/src/server/common/upload_session.rs` 的 `insert_uploading_session`（`upload_session.rs:96`），在 `InsertUploadSession { ... }` 里 `updated_at: now,` 之后加：

```rust
        submit_attempts: 0,
        last_submit_at: None,
        last_submit_error: None,
        submit_state: None,
```

- [ ] **Step 4: 编译验证**

Run: `cargo check -p biliup-cli`
Expected: 编译通过（无 “missing field” 错误）。若 `InsertUploadSession` 还有其它构造点报缺字段，按相同默认值补齐——经检索当前仅 `insert_uploading_session` 一处。

- [ ] **Step 5: Commit**

```bash
git add crates/biliup-cli/migrations/6_add_session_submit_trace.sql crates/biliup-cli/src/server/infrastructure/models.rs crates/biliup-cli/src/server/common/upload_session.rs
git commit -m "feat: add submit trace columns to upload_session"
```

---

### Task 2: 投稿状态纯函数与 DB 写入 helper

**Files:**
- Modify: `crates/biliup-cli/src/server/common/upload_session.rs`
- Test: `crates/biliup-cli/src/server/common/upload_session.rs`（`#[cfg(test)] mod tests`）

**Interfaces:**
- Consumes: `mutate_session`（`upload_session.rs:122`，私有，仅同模块可用）。
- Produces: `pub fn submit_state_label(aid: Option<u64>, submit_failed: bool) -> &'static str`
- Produces: `pub fn missing_status_where(status: Option<&str>) -> &'static str`
- Produces: 扩展 `pub async fn mark_submitted(pool, session_row_id, aid: u64, bvid: Option<String>) -> AppResult<()>`，额外写 `submit_state="ok_with_aid"`、`last_submit_at=now`、`last_submit_error=None`、`submit_attempts+=1`。
- Produces: `pub async fn mark_submit_anomaly(pool: &ConnectionPool, session_row_id: i64, state: &str, error: String) -> AppResult<()>`（写 `submit_state`/`last_submit_error`/`last_submit_at`/`submit_attempts+=1`，不改 `status`/`aid`）。

- [ ] **Step 1: 写失败测试（纯函数）**

在 `crates/biliup-cli/src/server/common/upload_session.rs` 的 `#[cfg(test)] mod tests` 内追加（若无 tests 模块则在文件末尾新建 `#[cfg(test)] mod tests { use super::*; ... }`）：

```rust
#[test]
fn submit_state_label_classifies_three_outcomes() {
    assert_eq!(submit_state_label(Some(123), false), "ok_with_aid");
    assert_eq!(submit_state_label(None, false), "ok_no_aid");
    assert_eq!(submit_state_label(None, true), "failed");
    // submit_failed 优先于 aid 判定
    assert_eq!(submit_state_label(Some(123), true), "failed");
}

#[test]
fn missing_status_where_maps_filter_to_sql() {
    assert_eq!(
        missing_status_where(None),
        "status IN ('pending', 'failed', 'uploading')"
    );
    assert_eq!(
        missing_status_where(Some("active")),
        "status IN ('pending', 'failed', 'uploading')"
    );
    assert_eq!(missing_status_where(Some("succeeded")), "status = 'succeeded'");
    assert_eq!(missing_status_where(Some("all")), "1 = 1");
    // 非法值归一到 active，避免注入与意外全表
    assert_eq!(
        missing_status_where(Some("garbage")),
        "status IN ('pending', 'failed', 'uploading')"
    );
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p biliup-cli upload_session -- --nocapture`
Expected: 编译失败，提示 `submit_state_label` / `missing_status_where` 未定义。

- [ ] **Step 3: 实现两个纯函数**

在 `crates/biliup-cli/src/server/common/upload_session.rs` 顶部 helper 区（如 `mutate_session` 之上）加：

```rust
/// 把投稿结果映射为持久化标签。submit_failed=接口未返回成功（code!=0 或网络错误）。
pub fn submit_state_label(aid: Option<u64>, submit_failed: bool) -> &'static str {
    if submit_failed {
        "failed"
    } else if aid.is_some() {
        "ok_with_aid"
    } else {
        "ok_no_aid"
    }
}

/// 「缺失补传」列表 status 过滤参数 → SQL where 片段。非法值归一到 active。
/// 返回静态串，杜绝拼接外部输入（防注入）。
pub fn missing_status_where(status: Option<&str>) -> &'static str {
    match status {
        Some("succeeded") => "status = 'succeeded'",
        Some("all") => "1 = 1",
        _ => "status IN ('pending', 'failed', 'uploading')",
    }
}
```

- [ ] **Step 4: 跑测试确认纯函数通过**

Run: `cargo test -p biliup-cli upload_session -- --nocapture`
Expected: 上述两个测试 PASS。

- [ ] **Step 5: 扩展 mark_submitted 并新增 mark_submit_anomaly**

Modify `crates/biliup-cli/src/server/common/upload_session.rs` 的 `mark_submitted`（`upload_session.rs:156`），把闭包体改为：

```rust
    mutate_session(pool, session_row_id, |row| {
        row.aid = Some(aid as i64);
        row.bvid = bvid;
        row.status = "finalized".to_string();
        row.submit_state = Some("ok_with_aid".to_string());
        row.last_submit_at = Some(chrono::Utc::now());
        row.last_submit_error = None;
        row.submit_attempts += 1;
        Ok(())
    })
    .await
```

在 `mark_submitted` 之后新增：

```rust
/// 记录一次投稿异常（ok_no_aid / failed）。不改 status/aid，仅落投稿状态，
/// 使「投稿成功却无 aid」「投稿接口失败」可持久查证。
pub async fn mark_submit_anomaly(
    pool: &ConnectionPool,
    session_row_id: i64,
    state: &str,
    error: String,
) -> AppResult<()> {
    let state = state.to_string();
    mutate_session(pool, session_row_id, |row| {
        row.submit_state = Some(state);
        row.last_submit_error = Some(error);
        row.last_submit_at = Some(chrono::Utc::now());
        row.submit_attempts += 1;
        Ok(())
    })
    .await
}
```

- [ ] **Step 6: 编译并跑相关测试**

Run: `cargo test -p biliup-cli upload_session -- --nocapture`
Expected: 全部 PASS，且 `cargo check -p biliup-cli` 通过。

- [ ] **Step 7: Commit**

```bash
git add crates/biliup-cli/src/server/common/upload_session.rs
git commit -m "feat: persist submit outcome on upload_session"
```

---

### Task 3: submit_session 记录投稿状态 + 全链路日志

**Files:**
- Modify: `crates/biliup-cli/src/server/common/upload.rs`

**Interfaces:**
- Consumes: `mark_submitted`、`mark_submit_anomaly`、`submit_state_label`（Task 2）。
- Consumes: `tracing::Instrument`（span 包裹 future）。
- Produces: `submit_session` 在三种结果下写库 + 打显式日志；`process_with_upload` 的 Session span；手动补传两条成功日志。

- [ ] **Step 1: 引入依赖**

Modify `crates/biliup-cli/src/server/common/upload.rs` 顶部：
- 在 `use crate::server::common::upload_session::{...}` 的导入列表中加入 `mark_submit_anomaly, submit_state_label`。
- 在文件 use 区加：

```rust
use tracing::Instrument;
```

（`mark_submitted` 已在 `upload.rs:195` 内通过 `submit_session` 用到；确认它已在导入列表，没有则一并加入。）

- [ ] **Step 2: 重写 submit_session 的提交结果处理**

Modify `crates/biliup-cli/src/server/common/upload.rs` 的 `submit_session`（`upload.rs:195-236`）。把从 `let resp = submit_to_bilibili(...).await?;` 到函数结尾 `Ok(())` 的整段替换为：

```rust
    info!(
        n_videos = videos.len(),
        title = %recorder.format_filename(),
        "submit_attempt：开始下播一次性投稿"
    );
    let resp = match submit_to_bilibili(bilibili, &studio, submit_api).await {
        Ok(resp) => resp,
        Err(e) => {
            let msg = format!("{e:?}");
            let state = submit_state_label(None, true); // "failed"
            error!(error = %msg, "submit_failed：投稿接口失败，保持 uploading 待补提交");
            if let Err(db) = mark_submit_anomaly(pool, session_row_id, state, msg).await {
                error!(?db, "写回 submit_state=failed 失败");
            }
            return Err(e);
        }
    };
    let aid = resp
        .data
        .as_ref()
        .and_then(|d| d.get("aid"))
        .and_then(|a| a.as_u64());
    let bvid = resp
        .data
        .as_ref()
        .and_then(|d| d.get("bvid"))
        .and_then(|b| b.as_str())
        .map(|s| s.to_string());

    match aid {
        Some(aid_val) => {
            // 提交成功即 finalize（mark_submitted 内部写 submit_state="ok_with_aid"）；
            // 写回失败仅告警（稿件已在 B 站，重复提交风险大于收益）。
            if let Err(e) = mark_submitted(pool, session_row_id, aid_val, bvid.clone()).await {
                error!(?e, aid = aid_val, "aid_writeback_fail：提交成功但写回 upload_session 失败");
            } else {
                info!(aid = aid_val, bvid = ?bvid, "submit_ok_with_aid：投稿成功并已写回 aid");
            }
            if let Some(section_id) = season_section_id {
                add_archive_to_season_with_retry(bilibili, section_id, aid_val).await;
            }
        }
        None => {
            let state = submit_state_label(aid, false); // "ok_no_aid"
            let msg = format!("submit_ok_no_aid: {resp:?}");
            error!(resp = ?resp, "submit_ok_no_aid：投稿 code==0 但响应缺少 aid，未 finalize（待下次开播补提交）");
            if let Err(db) = mark_submit_anomaly(pool, session_row_id, state, msg).await {
                error!(?db, "写回 submit_state=ok_no_aid 失败");
            }
        }
    }
    Ok(())
```

- [ ] **Step 3: 给 process_with_upload 加 Session span**

Modify `crates/biliup-cli/src/server/common/upload.rs` 的 `process_with_upload`（`upload.rs:71`）。在 `initialize_upload_context` 之后、`pipeline_upload_videos` 调用之前，声明 span；并把后续流水线（`pipeline_upload_videos` + 下播收尾 `recover_due_missing_segments`/`submit_session`）放进 `.instrument(span)` 的同一 async 作用域。最小改法：包一层 async 块。

将 `process_with_upload` 中从 `let archive = pipeline_upload_videos(...)` 到方法结尾 `Ok(())` 的主体替换为：

```rust
    let span = tracing::info_span!("session", session = tracing::field::Empty);
    async {
        let archive =
            pipeline_upload_videos(rx, &upload_context, upload_config, &segment_processors, ctx)
                .await?;

        if let Some(mut archive) = archive
            && !archive.videos.is_empty()
            && let Some(row_id) = archive.session_row_id
        {
            tracing::Span::current().record("session", row_id);
            if let Err(e) =
                recover_due_missing_segments(&upload_context, ctx, row_id, &mut archive).await
            {
                error!(?e, row_id, "静默补传缺失分段失败，继续提交已成功分段");
            }
            let config = ctx.config();
            if let Err(e) = submit_session(
                &upload_context,
                ctx.pool(),
                upload_config,
                config.season_section_id,
                config.submit_api.as_deref(),
                ctx.streamer_info(),
                row_id,
                &archive.videos,
            )
            .await
            {
                error!(?e, "下播一次性提交失败，保持 uploading 待下次补提交");
            }
        }
        Ok::<(), error_stack::Report<AppError>>(())
    }
    .instrument(span)
    .await
```

> 注：`session` 字段在首段落库拿到 `row_id` 前为空属正常（空会话不投稿）。`record` 接受 i64，`row_id` 即 `upload_session.id`。

- [ ] **Step 4: 手动补传两条成功日志**

Modify `crates/biliup-cli/src/server/common/upload.rs` 的 `manual_recover_missing_segment`（`upload.rs:791`）。

(a) 在函数体顶部、`let mut row = ...fetch_one...` 之后，开 Session span 包住后续逻辑——最简做法：把核心 `upload_result` async 块用 span instrument。找到 `let upload_result: AppResult<()> = async { ... }.await;`，改为：

```rust
    let span = tracing::info_span!("session", session = row.upload_session_id);
    let upload_result: AppResult<()> = async {
        // ...原有 async 块内容保持不变...
    }
    .instrument(span)
    .await;
```

(b) 在 async 块内的两条成功分支补日志。`edit_by_app` 成功后（`upload.rs:870-874` 两处 `edit_by_app(...).await?` 之后各一条），加：

```rust
                info!(aid, segment_order = row.segment_order, "manual_recover_edit_archive：手动补传已追加到稿件");
```

在 `insert_session_video_at_order(...).await?;`（`upload.rs:876`）之后加：

```rust
                info!(
                    session = session_id,
                    segment_order = row.segment_order,
                    "manual_recover_to_session：手动补传已补进待提交会话，待下播投稿"
                );
```

> `aid` 在两处 edit 分支分别是 `session.aid.or(row.aid)` 解出的值与 `row.aid`；用各分支已绑定的变量名。

- [ ] **Step 5: 编译 + 已有测试不回归**

Run: `cargo test -p biliup-cli -- --nocapture`
Expected: 编译通过，既有测试全 PASS（本任务无新单测，逻辑由 Task 2 纯函数覆盖；此处为接线）。若 `submit_to_bilibili` 的错误类型导致 `return Err(e)` 类型不匹配，用 `.change_context(AppError::Unknown)` 对齐函数返回类型 `AppResult<()>`。

- [ ] **Step 6: Commit**

```bash
git add crates/biliup-cli/src/server/common/upload.rs
git commit -m "feat: trace span and explicit logs across upload/submit/recovery"
```

---

### Task 4: 缺失补传列表 status 过滤（后端）

**Files:**
- Modify: `crates/biliup-cli/src/server/api/endpoints.rs:612-625`

**Interfaces:**
- Consumes: `missing_status_where`（Task 2）。
- Produces: `get_missing_uploads` 接受 `?status=active|succeeded|all`，缺省 active。

- [ ] **Step 1: 引入纯函数与 Query 提取器**

Modify `crates/biliup-cli/src/server/api/endpoints.rs` 顶部 use 区：
- 加 `use crate::server::common::upload_session::missing_status_where;`
- 确认 `axum::extract::Query` 可用（文件已用 `axum::extract::*` 系列；若没有则加 `use axum::extract::Query;`）。

- [ ] **Step 2: 改写 get_missing_uploads**

Modify `crates/biliup-cli/src/server/api/endpoints.rs` 的 `get_missing_uploads`（`endpoints.rs:612`），整体替换为：

```rust
#[derive(serde::Deserialize)]
pub struct MissingQuery {
    pub status: Option<String>,
}

pub async fn get_missing_uploads(
    State(service_register): State<ServiceRegister>,
    Query(q): Query<MissingQuery>,
) -> Result<Json<Vec<UploadMissingSegment>>, Response> {
    let where_clause = missing_status_where(q.status.as_deref());
    let sql = format!(
        "SELECT * FROM upload_missing_segment WHERE {where_clause} ORDER BY created_at DESC"
    );
    let rows = sqlx::query_as::<_, UploadMissingSegment>(&sql)
        .fetch_all(&service_register.pool)
        .await
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;
    Ok(Json(rows))
}
```

> `where_clause` 只来自 `missing_status_where` 返回的静态串，不含任何外部输入，`format!` 拼接安全。

- [ ] **Step 3: 确认路由无需改**

查看 `crates/biliup-cli/src/server/router.rs` 中 `get_missing_uploads` 的路由行——`Query` 提取器不影响路由注册，路径不变。无需修改，仅确认。

Run: `cargo check -p biliup-cli`
Expected: 编译通过。

- [ ] **Step 4: 跑测试**

Run: `cargo test -p biliup-cli upload_session -- --nocapture`
Expected: `missing_status_where` 单测仍 PASS（后端复用同一函数）。

- [ ] **Step 5: Commit**

```bash
git add crates/biliup-cli/src/server/api/endpoints.rs
git commit -m "feat: filter missing uploads by status"
```

---

### Task 5: 缺失补传页筛选与去向列（前端）

**Files:**
- Modify: `app/(app)/missing/page.tsx`

**Interfaces:**
- Consumes: 后端 `/v1/uploads/missing?status=<active|succeeded|all>`（Task 4）。
- Produces: 状态下拉、SWR key 带参、去向列、完成时间列。

- [ ] **Step 1: 引入 Select 与筛选状态**

Modify `app/(app)/missing/page.tsx`：
- 第 2 行 import 加入 `Select`：
  ```tsx
  import { Button, Layout, Popconfirm, Select, Table, Tag, Toast, Typography } from '@douyinfe/semi-ui'
  ```
- 在组件内 `const { Text } = Typography` 之后加筛选状态，并把 SWR key 改为带参：
  ```tsx
  const [statusFilter, setStatusFilter] = useState<'active' | 'succeeded' | 'all'>('active')
  ```
  把原 `useSWR<MissingSegment[]>('/v1/uploads/missing', fetcher)` 改为：
  ```tsx
  } = useSWR<MissingSegment[]>(`/v1/uploads/missing?status=${statusFilter}`, fetcher)
  ```

- [ ] **Step 2: 顶栏加状态下拉**

Modify `app/(app)/missing/page.tsx`，在 Header 的「刷新」`Button`（`page.tsx:158`）之前，同一 `<nav>` 右侧容器内加下拉。把刷新按钮那段包进一个 flex 容器：

```tsx
          <div style={{ display: 'flex', gap: 10, alignItems: 'center' }}>
            <Select
              value={statusFilter}
              onChange={(v) => setStatusFilter(v as 'active' | 'succeeded' | 'all')}
              style={{ width: 130 }}
              optionList={[
                { value: 'active', label: '待补传' },
                { value: 'succeeded', label: '已补传' },
                { value: 'all', label: '全部' },
              ]}
            />
            <Button icon={<IconRefresh />} type="tertiary" onClick={() => mutate()}>
              刷新
            </Button>
          </div>
```

（删除原先单独的 `<Button icon={<IconRefresh />} ...>刷新</Button>`，避免重复。）

- [ ] **Step 3: 加「去向」与「完成时间」列**

Modify `app/(app)/missing/page.tsx`，在 `columns` 数组里「最后错误」列对象之后、「操作」列之前插入两列。去向渲染按 spec：

```tsx
    {
      title: '去向',
      dataIndex: 'destination',
      width: 200,
      render: (_: unknown, record: MissingSegment) => {
        if (record.status !== 'succeeded') return '—'
        if (record.aid != null) {
          return (
            <a href={`https://www.bilibili.com/video/av${record.aid}`} target="_blank" rel="noreferrer">
              已追加到稿件 av{record.aid}
            </a>
          )
        }
        if (record.upload_session_id != null) {
          return <Text type="tertiary">已补进待提交会话 #{record.upload_session_id}</Text>
        }
        return '—'
      },
    },
    {
      title: '完成时间',
      dataIndex: 'updated_at',
      width: 180,
      render: (s: string, record: MissingSegment) =>
        record.status === 'succeeded' ? fmtTime(s) : '—',
    },
```

- [ ] **Step 4: 提示文案补充 trace 用法**

Modify `app/(app)/missing/page.tsx` 的说明 `Text`（`page.tsx:164-167`），在末尾追加一句，告知用户去向里的 `#会话号` 可用于日志追踪：

```tsx
          录制期间上传失败、尚未补传的分段。下播提交前会自动换线重试到期的分段；这里可手动立即补传，
          补传成功后会按原分 P 位置补进对应稿件或待提交会话。切换「已补传」可查看历史记录与去向，
          其中「#会话号」即日志里的 session，可在「实时日志」按该号检索整条上传链路。
```

- [ ] **Step 5: 前端构建/类型检查**

Run（在 `biliup/` 下，按项目实际包管理器，二选一）：
```bash
npm run build
# 或仅类型检查： npx tsc --noEmit
```
Expected: 构建/类型检查通过，无 TS 报错。若本机无前端依赖，跳过并在 PR 说明里标注需 CI 验证。

- [ ] **Step 6: 手动验证（运行态，可选）**

启动服务后打开「缺失补传」页：
- 默认「待补传」与原行为一致；
- 切「已补传」能看到 `succeeded` 行，去向显示 av 链接或 `#会话号`，完成时间有值；
- 切「全部」三态都在。

- [ ] **Step 7: Commit**

```bash
git add "app/(app)/missing/page.tsx"
git commit -m "feat: missing recovery page status filter and destination column"
```

---

### Task 6: 收尾——格式化与整体验证

**Files:**
- 仅在前序任务文件需要时改动。

**Interfaces:**
- Consumes: 全部前序任务。

- [ ] **Step 1: Rust 格式化**

Run: `cargo fmt`
Expected: 退出 0。

- [ ] **Step 2: 跑 biliup-cli 全量测试**

Run: `cargo test -p biliup-cli`
Expected: 通过。若有与本计划无关的既有失败，原样记录失败输出并停下复核，不要强行改动无关代码。

- [ ] **Step 3: 编译整工作区**

Run: `cargo check`
Expected: 通过。

- [ ] **Step 4: 检查改动面**

Run: `git status --short`
Expected: 仅本计划涉及文件 + 可能的 `cargo fmt` 改动；不包含无关的 `BUILD_AND_DEPLOY.md` 等。

- [ ] **Step 5: 如格式化产生改动则提交**

```bash
git add crates/biliup-cli
git commit -m "chore: format upload trace observability"
```

---

## Self-Review

- **Spec 覆盖**：
  - 组件1 Session span → Task 3 Step 3/4；
  - 组件2 显式日志点 → Task 3 Step 2（投稿）、Step 4（手动补传）；既有 enqueue/silent_recover 日志经 span 自动带 `session`；
  - 组件3 会话表持久化 → Task 1（列）+ Task 2（写入 helper/纯函数）+ Task 3 Step 2（写入点）；
  - 组件4 缺失补传筛选 → Task 4（后端）+ Task 5（前端去向/完成时间/筛选）。
- **占位符扫描**：无 TBD/TODO；每个改码步骤均附完整代码。前端构建命令给了二选一并说明降级，非占位。
- **类型一致**：`submit_state_label(Option<u64>, bool)`、`missing_status_where(Option<&str>)`、`mark_submit_anomaly(&ConnectionPool,i64,&str,String)` 在定义（Task 2）与使用（Task 3/4）处签名一致；`submit_state` 取值串 `ok_with_aid/ok_no_aid/failed` 全程一致；前端 `MissingSegment` 字段（`aid`/`upload_session_id`/`updated_at`/`status`）均为接口已有字段。
- **已知实现风险已标注**：`submit_to_bilibili` 错误类型对齐（Task 3 Step 5）、`InsertUploadSession` 仅一处构造点（Task 1 Step 4）、`Query` 提取器导入（Task 4 Step 1）。
```
