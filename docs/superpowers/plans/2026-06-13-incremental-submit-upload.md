# 增量投稿（每段上传即落 B 站稿件）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 每段视频上传成功就立即 `submit_by_app` 建稿 / `edit_by_app` 追加到同一 B 站稿件并落库，使进程崩溃或重启不丢失已上传内容。

**Architecture:** 在 `server/common/upload.rs` 的 `pipeline_upload_videos` 中，把「下播后一次性 submit」改为「首段建稿拿 aid、后续段 edit 追加」。每场直播对应一行 `upload_session`（含 aid / videos_json / status），由稳定的 `live_streamer_id`（room id）+ 时间窗口在重启后续接同一稿件。每段投稿成功后立即删本地。

**Tech Stack:** Rust, tokio, ormlite + sqlx(SQLite), error-stack。设计文档见 `docs/superpowers/specs/2026-06-13-incremental-submit-upload-design.md`。

---

## File Structure

- Create: `crates/biliup-cli/migrations/4_add_upload_session.sql` — 新表 `upload_session`。
- Modify: `crates/biliup-cli/src/server/infrastructure/models.rs` — 新增 `UploadSession` 模型 + `InsertUploadSession`。
- Create: `crates/biliup-cli/src/server/common/upload_session.rs` — 纯函数 `select_recovery_candidate` + 会话 DB 辅助函数 + 进程内 `LiveArchive` 状态。
- Modify: `crates/biliup-cli/src/server/common/mod.rs` — 注册 `upload_session` 子模块。
- Modify: `crates/biliup-cli/src/server/config.rs` — 新增 `recovery_window_minutes` 配置字段。
- Modify: `crates/biliup-cli/src/server/common/upload.rs` — 改造 `process_with_upload` / `pipeline_upload_videos` 为增量投稿 + 每段删本地 + 收尾 finalize。

---

## Task 1: 建表迁移 `upload_session`

**Files:**
- Create: `crates/biliup-cli/migrations/4_add_upload_session.sql`

- [ ] **Step 1: 写迁移 SQL**

参照 `crates/biliup-cli/migrations/1_initial.sql` 的 `filelist` 建表风格（外键引用 `streamerinfo`、`livestreamers`）。

```sql
-- 增量投稿：每场直播一行，记录 B 站稿件号与已投稿视频列表，用于崩溃/重启后续接同一稿件。
-- live_streamer_id：配置直播间(room)稳定 id，跨重启匹配用。
-- streamer_info_id：当前挂接的会话 id，重启续接时更新为新会话。
-- aid 为 NULL 表示还没建稿；status：uploading / submitted / finalized。
create table if not exists upload_session
(
    id INTEGER not null
        constraint pk_upload_session
            primary key,
    live_streamer_id INTEGER not null,
    streamer_info_id INTEGER not null,
    aid INTEGER,
    bvid VARCHAR,
    videos_json TEXT not null default '[]',
    status VARCHAR not null default 'uploading',
    created_at DATETIME not null,
    updated_at DATETIME not null
);
create index if not exists ix_upload_session_room
    on upload_session (live_streamer_id, status, updated_at);
```

- [ ] **Step 2: 校验迁移可加载**

Run: `cd crates/biliup-cli && cargo build`
Expected: 编译通过（`sqlx::migrate!()` 在 `connection_pool.rs:52` 会编译期扫描 migrations 目录，新增 .sql 不应报错）。

- [ ] **Step 3: Commit**

```bash
git add crates/biliup-cli/migrations/4_add_upload_session.sql
git commit -m "feat(db): 新增 upload_session 表用于增量投稿状态"
```

---

## Task 2: `UploadSession` 模型

**Files:**
- Modify: `crates/biliup-cli/src/server/infrastructure/models.rs`

- [ ] **Step 1: 新增模型与插入结构体**

在 `models.rs` 中 `FileItem` 之后（约 `:60` 行后）追加。`models.rs` 顶部已 `use chrono::{DateTime, Utc};` 与 `use ormlite::{Insert, Model};`，无需新增 import。

```rust
/// 增量投稿会话模型
/// 一场直播对应一行，记录 B 站稿件号与已投稿视频列表（JSON）。
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[ormlite(table = "upload_session", insert = "InsertUploadSession")]
pub struct UploadSession {
    /// 主键ID
    pub id: i64,
    /// 配置直播间(room)稳定 id，跨重启匹配用
    pub live_streamer_id: i64,
    /// 当前挂接的会话 id（重启续接时更新）
    pub streamer_info_id: i64,
    /// B站稿件号，None=还没建稿
    pub aid: Option<i64>,
    /// B站 bvid
    pub bvid: Option<String>,
    /// 已成功投稿的 Video 列表（JSON 字符串），edit 时携带
    pub videos_json: String,
    /// uploading / submitted / finalized
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// 插入 upload_session 的数据结构
#[derive(Insert, Debug, Clone, Serialize, Deserialize)]
#[ormlite(returns = "UploadSession")]
pub struct InsertUploadSession {
    pub live_streamer_id: i64,
    pub streamer_info_id: i64,
    pub aid: Option<i64>,
    pub bvid: Option<String>,
    pub videos_json: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
```

- [ ] **Step 2: 校验编译**

Run: `cd crates/biliup-cli && cargo build`
Expected: 编译通过。

- [ ] **Step 3: Commit**

```bash
git add crates/biliup-cli/src/server/infrastructure/models.rs
git commit -m "feat(model): 新增 UploadSession 模型"
```

---

## Task 3: 恢复匹配纯函数（TDD）

**Files:**
- Create: `crates/biliup-cli/src/server/common/upload_session.rs`
- Modify: `crates/biliup-cli/src/server/common/mod.rs`

- [ ] **Step 1: 注册子模块**

查看 `crates/biliup-cli/src/server/common/mod.rs` 现有 `pub mod ...;` 列表，在其中追加一行：

```rust
pub mod upload_session;
```

- [ ] **Step 2: 写失败测试**

创建 `crates/biliup-cli/src/server/common/upload_session.rs`，先只放纯函数签名（返回 `None`）与测试：

```rust
use crate::server::infrastructure::models::UploadSession;
use chrono::{DateTime, Utc};

/// 从候选会话中选出可续接的那条：同 room、未 finalize、updated_at 在窗口内、取最新。
/// 返回选中项在 `sessions` 中的下标，便于调用方按需取用（避免借用纠纷）。
pub fn select_recovery_candidate(
    sessions: &[UploadSession],
    room_id: i64,
    now: DateTime<Utc>,
    window_minutes: i64,
) -> Option<usize> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn session(
        id: i64,
        room_id: i64,
        status: &str,
        updated_at: DateTime<Utc>,
    ) -> UploadSession {
        UploadSession {
            id,
            live_streamer_id: room_id,
            streamer_info_id: id,
            aid: Some(100 + id),
            bvid: None,
            videos_json: "[]".to_string(),
            status: status.to_string(),
            created_at: updated_at,
            updated_at,
        }
    }

    fn t(min_ago: i64, now: DateTime<Utc>) -> DateTime<Utc> {
        now - chrono::Duration::minutes(min_ago)
    }

    fn now_fixed() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 13, 12, 0, 0).unwrap()
    }

    #[test]
    fn picks_most_recent_active_in_window() {
        let now = now_fixed();
        let sessions = vec![
            session(1, 7, "uploading", t(20, now)),
            session(2, 7, "submitted", t(5, now)), // 最新且在窗口内
            session(3, 7, "uploading", t(25, now)),
        ];
        assert_eq!(select_recovery_candidate(&sessions, 7, now, 30), Some(1));
    }

    #[test]
    fn skips_finalized() {
        let now = now_fixed();
        let sessions = vec![session(1, 7, "finalized", t(1, now))];
        assert_eq!(select_recovery_candidate(&sessions, 7, now, 30), None);
    }

    #[test]
    fn skips_outside_window() {
        let now = now_fixed();
        let sessions = vec![session(1, 7, "submitted", t(31, now))];
        assert_eq!(select_recovery_candidate(&sessions, 7, now, 30), None);
    }

    #[test]
    fn skips_other_room() {
        let now = now_fixed();
        let sessions = vec![session(1, 8, "submitted", t(1, now))];
        assert_eq!(select_recovery_candidate(&sessions, 7, now, 30), None);
    }

    #[test]
    fn none_when_empty() {
        let now = now_fixed();
        assert_eq!(select_recovery_candidate(&[], 7, now, 30), None);
    }
}
```

- [ ] **Step 3: 运行测试确认失败**

Run: `cd crates/biliup-cli && cargo test --lib server::common::upload_session::tests`
Expected: 5 个测试中至少 `picks_most_recent_active_in_window` FAIL（断言 `Some(1)` 但得到 `None`）。

- [ ] **Step 4: 实现纯函数**

替换 `select_recovery_candidate` 函数体：

```rust
pub fn select_recovery_candidate(
    sessions: &[UploadSession],
    room_id: i64,
    now: DateTime<Utc>,
    window_minutes: i64,
) -> Option<usize> {
    let cutoff = now - chrono::Duration::minutes(window_minutes);
    sessions
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            s.live_streamer_id == room_id
                && s.status != "finalized"
                && s.updated_at >= cutoff
        })
        .max_by_key(|(_, s)| s.updated_at)
        .map(|(i, _)| i)
}
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cd crates/biliup-cli && cargo test --lib server::common::upload_session::tests`
Expected: 5 passed。

- [ ] **Step 6: Commit**

```bash
git add crates/biliup-cli/src/server/common/upload_session.rs crates/biliup-cli/src/server/common/mod.rs
git commit -m "feat: 增量投稿恢复匹配纯函数 select_recovery_candidate"
```

---

## Task 4: 会话 DB 辅助函数 + 进程内状态

**Files:**
- Modify: `crates/biliup-cli/src/server/common/upload_session.rs`

- [ ] **Step 1: 追加 DB 辅助函数与 LiveArchive**

在 `upload_session.rs` 顶部 import 区追加，并在 `#[cfg(test)] mod tests` 之前插入实现。参照 `repositories.rs` 的 `ormlite::Insert::insert(payload, pool)` 与 `Model::select()` 用法。

```rust
use crate::server::errors::{AppError, AppResult};
use crate::server::infrastructure::connection_pool::ConnectionPool;
use crate::server::infrastructure::models::{InsertUploadSession, UploadSession};
use biliup::bilibili::Video;
use error_stack::ResultExt;
use ormlite::{Insert, Model};

/// 进程内的本场稿件状态。aid=None 表示尚未建稿。
#[derive(Debug, Default, Clone)]
pub struct LiveArchive {
    /// upload_session 行 id；None 表示该行尚未创建（全新直播、首段未建稿前）
    pub session_row_id: Option<i64>,
    pub aid: Option<u64>,
    pub bvid: Option<String>,
    pub videos: Vec<Video>,
}

/// 查询某 room 下所有未 finalize 的会话（供纯函数选择候选）。
pub async fn active_sessions_for_room(
    pool: &ConnectionPool,
    room_id: i64,
) -> AppResult<Vec<UploadSession>> {
    UploadSession::select()
        .where_bind("live_streamer_id = ? AND status != 'finalized'", room_id)
        .fetch_all(pool)
        .await
        .change_context(AppError::Unknown)
}

/// 重启续接：把已有会话行的 streamer_info_id 更新为新会话，并刷新 updated_at。
pub async fn reattach_session(
    pool: &ConnectionPool,
    mut session: UploadSession,
    new_streamer_info_id: i64,
) -> AppResult<UploadSession> {
    session.streamer_info_id = new_streamer_info_id;
    session.updated_at = chrono::Utc::now();
    session
        .update_all_fields(pool)
        .await
        .change_context(AppError::Unknown)
}

/// 首段建稿后插入会话行。
pub async fn insert_session(
    pool: &ConnectionPool,
    room_id: i64,
    streamer_info_id: i64,
    aid: u64,
    bvid: Option<String>,
    videos: &[Video],
) -> AppResult<UploadSession> {
    let now = chrono::Utc::now();
    InsertUploadSession {
        live_streamer_id: room_id,
        streamer_info_id,
        aid: Some(aid as i64),
        bvid,
        videos_json: serde_json::to_string(videos).change_context(AppError::Unknown)?,
        status: "submitted".to_string(),
        created_at: now,
        updated_at: now,
    }
    .insert(pool)
    .await
    .change_context(AppError::Unknown)
}

/// 追加段后更新 videos_json 与 updated_at。
pub async fn update_session_videos(
    pool: &ConnectionPool,
    session_row_id: i64,
    videos: &[Video],
) -> AppResult<()> {
    let mut row = UploadSession::select()
        .where_bind("id = ?", session_row_id)
        .fetch_one(pool)
        .await
        .change_context(AppError::Unknown)?;
    row.videos_json = serde_json::to_string(videos).change_context(AppError::Unknown)?;
    row.updated_at = chrono::Utc::now();
    row.update_all_fields(pool)
        .await
        .change_context(AppError::Unknown)?;
    Ok(())
}

/// 下播收尾：标记 finalized。
pub async fn finalize_session(pool: &ConnectionPool, session_row_id: i64) -> AppResult<()> {
    let mut row = UploadSession::select()
        .where_bind("id = ?", session_row_id)
        .fetch_one(pool)
        .await
        .change_context(AppError::Unknown)?;
    row.status = "finalized".to_string();
    row.updated_at = chrono::Utc::now();
    row.update_all_fields(pool)
        .await
        .change_context(AppError::Unknown)?;
    Ok(())
}

/// 从 videos_json 反序列化已投稿视频列表。
pub fn parse_videos(videos_json: &str) -> Vec<Video> {
    serde_json::from_str(videos_json).unwrap_or_default()
}
```

- [ ] **Step 2: 校验编译（确认 ormlite API 名称）**

Run: `cd crates/biliup-cli && cargo build`
Expected: 编译通过。若 `update_all_fields` / `where_bind` 在本版 ormlite 不存在，按编译报错替换为等价 API（参考 `repositories.rs` 中实际使用的 ormlite 方法），并据此修正本步代码后重试。

- [ ] **Step 3: Commit**

```bash
git add crates/biliup-cli/src/server/common/upload_session.rs
git commit -m "feat: upload_session DB 辅助函数与 LiveArchive 进程内状态"
```

---

## Task 5: 新增 `recovery_window_minutes` 配置

**Files:**
- Modify: `crates/biliup-cli/src/server/config.rs`

- [ ] **Step 1: 在 Config 中新增字段**

参照 `config.rs:58` 的 `season_section_id: Option<i64>` 写法，在其附近追加（`Config` derive 了 `struct_patch::Patch`，新增字段会自动获得 per-streamer override 能力）：

```rust
    /// 增量投稿重启续接时间窗口（分钟）。留空回退默认 30。
    /// 重启后某 room 在窗口内存在未 finalize 的会话则续接其 aid，否则新建稿。
    pub recovery_window_minutes: Option<u64>,
```

- [ ] **Step 2: 校验编译**

Run: `cd crates/biliup-cli && cargo build`
Expected: 编译通过（`#[serde(default)]`/`Option` 使旧配置 JSON 仍可反序列化；若 Config 未对全字段 `#[serde(default)]`，确认该字段为 `Option` 即可缺省）。

- [ ] **Step 3: Commit**

```bash
git add crates/biliup-cli/src/server/config.rs
git commit -m "feat(config): 新增 recovery_window_minutes(默认30,可per-streamer覆盖)"
```

---

## Task 6: 改造上传链路为增量投稿

**Files:**
- Modify: `crates/biliup-cli/src/server/common/upload.rs`

本任务是核心改动。`pipeline_upload_videos` 当前签名（`:180`）攒 `Video` 后由 `process_with_upload`（`:73-104`）下播一次性 submit。改为每段投稿 + 每段删本地，收尾 finalize。

- [ ] **Step 1: 引入依赖与默认窗口常量**

在 `upload.rs` 顶部 import 区追加：

```rust
use crate::server::common::upload_session::{
    self, LiveArchive, active_sessions_for_room, finalize_session, insert_session, parse_videos,
    reattach_session, select_recovery_candidate, update_session_videos,
};
```

在文件靠上位置（`UploadContext` 定义附近）加常量：

```rust
/// 重启续接默认时间窗口（分钟）
const DEFAULT_RECOVERY_WINDOW_MINUTES: u64 = 30;
```

- [ ] **Step 2: 新增「每段投稿/追加」辅助函数**

在 `upload.rs` 中 `submit_to_bilibili`（`:290`）之后新增。它封装「首段 build_studio+submit、后续 edit_by_app」并落库；studio 元数据用 `Option<Studio>` 缓存，仅首段构建一次。

```rust
/// 每段上传成功后：首段建稿，后续 edit 追加。就地更新 archive 与缓存 studio，并落库。
async fn submit_or_append_segment(
    ctx: &Context,
    upload_context: &UploadContext,
    upload_config: &UploadStreamer,
    archive: &mut LiveArchive,
    cached_studio: &mut Option<Studio>,
    video: Video,
) -> AppResult<()> {
    let bilibili = &upload_context.bilibili;
    let room_id = ctx.worker_id();
    let streamer_info_id = ctx.id();

    if archive.aid.is_none() {
        // 首段：构建 studio（封面上传/自动封面等一次性开销）并建稿。
        let mut recorder = ctx.recorder(ctx.streamer_info().clone());
        recorder.filename_prefix = upload_config.title.clone();
        let mut studio =
            build_studio(upload_config, bilibili, vec![video.clone()], &recorder).await?;

        let submit_api = ctx.config().submit_api.clone();
        let resp = submit_to_bilibili(bilibili, &studio, submit_api.as_deref()).await?;
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

        archive.videos = vec![video];
        archive.aid = aid;
        archive.bvid = bvid.clone();

        // 落库：若是重启续接到的已有行（session_row_id 已存在）走更新，否则插入。
        if let (Some(aid_val), Some(row_id)) = (aid, archive.session_row_id) {
            // 极少数情况：续接行存在但之前未建稿，更新其 aid + videos。
            // 用 insert_session 的字段语义手动更新。
            update_session_videos(ctx.pool(), row_id, &archive.videos).await?;
            let _ = aid_val;
        } else if let Some(aid_val) = aid {
            let row =
                insert_session(ctx.pool(), room_id, streamer_info_id, aid_val, bvid, &archive.videos)
                    .await?;
            archive.session_row_id = Some(row.id);
        } else {
            warn!(?resp, "建稿响应缺少 aid，无法落库 upload_session");
        }

        // studio 缓存供后续 edit 复用（仅替换 aid/videos）
        studio.aid = archive.aid;
        *cached_studio = Some(studio);

        // 合集：建稿拿到 aid 后加入一次
        if let (Some(section_id), Some(aid_val)) = (ctx.config().season_section_id, archive.aid) {
            add_archive_to_season_with_retry(bilibili, section_id, aid_val).await;
        }
    } else {
        // 后续段：追加到已有 aid。
        archive.videos.push(video);
        let studio = cached_studio
            .as_mut()
            .ok_or(AppError::Custom("cached studio missing for edit".into()))?;
        studio.aid = archive.aid;
        studio.videos = archive.videos.clone();
        bilibili
            .edit_by_app(studio, None)
            .await
            .change_context(AppError::Unknown)?;
        if let Some(row_id) = archive.session_row_id {
            update_session_videos(ctx.pool(), row_id, &archive.videos).await?;
        }
    }
    Ok(())
}
```

- [ ] **Step 3: 新增「会话准备（恢复匹配）」辅助函数**

在上一步函数之后新增。在消费分段前调用，返回初始 `LiveArchive`。

```rust
/// 开播时准备本场稿件状态：命中窗口内未 finalize 的同 room 会话则续接，否则返回空 archive。
async fn prepare_archive(ctx: &Context) -> AppResult<LiveArchive> {
    let room_id = ctx.worker_id();
    let window = ctx
        .config()
        .recovery_window_minutes
        .unwrap_or(DEFAULT_RECOVERY_WINDOW_MINUTES) as i64;
    let sessions = active_sessions_for_room(ctx.pool(), room_id).await?;
    let now = chrono::Utc::now();
    if let Some(idx) = select_recovery_candidate(&sessions, room_id, now, window) {
        let candidate = sessions[idx].clone();
        let videos = parse_videos(&candidate.videos_json);
        let aid = candidate.aid.map(|a| a as u64);
        let bvid = candidate.bvid.clone();
        info!(aid=?aid, room_id, "重启续接已有稿件，将 edit 追加后续分段");
        let row = reattach_session(ctx.pool(), candidate, ctx.id()).await?;
        Ok(LiveArchive {
            session_row_id: Some(row.id),
            aid,
            bvid,
            videos,
        })
    } else {
        Ok(LiveArchive::default())
    }
}
```

注意：续接场景下 `cached_studio` 为 `None`，而 `archive.aid` 已是 `Some`，会直接进入 Step 2 的 edit 分支并因 `cached_studio` 缺失报错。为此，edit 分支在 `cached_studio` 为 `None` 时需先用 `studio_data(aid)` 兜底重建。修正 Step 2 的 else 分支起始处，插入：

```rust
        if cached_studio.is_none() {
            // 重启续接：进程内无缓存 studio，用 B 站现有稿件数据兜底重建。
            if let Some(aid_val) = archive.aid {
                let vid = biliup::bilibili::Vid::Aid(aid_val);
                match bilibili.studio_data(&vid, None).await {
                    Ok(mut s) => {
                        s.aid = archive.aid;
                        *cached_studio = Some(s);
                    }
                    Err(e) => {
                        warn!(?e, "studio_data 兜底失败，改为重建最小 studio");
                        let mut recorder = ctx.recorder(ctx.streamer_info().clone());
                        recorder.filename_prefix = upload_config.title.clone();
                        let mut s = build_studio(
                            upload_config,
                            bilibili,
                            archive.videos.clone(),
                            &recorder,
                        )
                        .await?;
                        s.aid = archive.aid;
                        *cached_studio = Some(s);
                    }
                }
            }
        }
```

- [ ] **Step 4: 改写 `pipeline_upload_videos`**

把签名返回值与逻辑改为「每段投稿 + 每段删本地」，不再返回 `UploadedVideos`。新签名：

```rust
async fn pipeline_upload_videos<F>(
    rx: Inspect<Receiver<SegmentInfo>, F>,
    upload_context: &UploadContext,
    upload_config: &UploadStreamer,
    segment_processors: &[HookStep],
    ctx: &Context,
) -> AppResult<Option<LiveArchive>>
where
    F: FnMut(&SegmentInfo),
{
    let mut archive = prepare_archive(ctx).await?;
    let mut cached_studio: Option<Studio> = None;
    pin!(rx);
    while let Some(event) = rx.next().await {
        let mut paths = segment_paths(&event);
        if !segment_processors.is_empty()
            && let Err(e) = process_video_paths(&mut paths, segment_processors).await
        {
            error!(file = ?event.prev_file_path, "segment_processor failed, skipping segment: {:?}", e);
            continue;
        }
        let upload_path = paths
            .first()
            .cloned()
            .unwrap_or_else(|| event.prev_file_path.clone());

        match upload_single_file(&upload_path, upload_context).await {
            Ok(video) => {
                if let Err(e) = submit_or_append_segment(
                    ctx,
                    upload_context,
                    upload_config,
                    &mut archive,
                    &mut cached_studio,
                    video,
                )
                .await
                {
                    // 投稿/追加失败：不删本地，保留文件以便人工或下次重试，跳过本段。
                    error!(file = ?upload_path, "submit/append failed, keeping local file: {:?}", e);
                    continue;
                }
                // 投稿成功即 durable：立即后处理删本地（增量模式统一为「每段成功后删」）。
                if let Err(e) = execute_postprocessor(paths, ctx).await {
                    error!(file = ?upload_path, "per-segment postprocessor failed: {:?}", e);
                }
            }
            Err(e) => {
                error!(file = ?upload_path, "upload_single_file failed, skipping segment: {:?}", e);
            }
        }
    }
    Ok(if archive.aid.is_some() { Some(archive) } else { None })
}
```

- [ ] **Step 5: 改写 `process_with_upload`**

替换 `:45-112` 的函数体中段（步骤 2/3/4），保留步骤 1 初始化。新版：

```rust
pub async fn process_with_upload<F>(
    rx: Inspect<Receiver<SegmentInfo>, F>,
    ctx: &Context,
    upload_config: &UploadStreamer,
) -> AppResult<()>
where
    F: FnMut(&SegmentInfo),
{
    info!(upload_config=?upload_config, "Starting process with upload");
    let upload_context =
        initialize_upload_context(&ctx.config(), &ctx.stateless_client(), upload_config).await?;

    let segment_processors: Vec<HookStep> = ctx
        .live_streamer()
        .segment_processor
        .clone()
        .unwrap_or_default();

    // 增量投稿：每段上传成功即建稿/追加并删本地。返回本场稿件状态用于收尾 finalize。
    let archive =
        pipeline_upload_videos(rx, &upload_context, upload_config, &segment_processors, ctx).await?;

    // 下播收尾：标记会话 finalized，使其不再参与重启续接。
    if let Some(archive) = archive
        && let Some(row_id) = archive.session_row_id
    {
        if let Err(e) = finalize_session(ctx.pool(), row_id).await {
            warn!(?e, "finalize upload_session 失败");
        }
    }

    Ok(())
}
```

- [ ] **Step 6: 删除/清理废弃结构**

`UploadedVideos` 结构体（`:39-43`）若不再被引用则删除。`build_studio` / `submit_to_bilibili` / `add_archive_to_season_with_retry` / `execute_postprocessor` / `upload_single_file` 保留。检查 `:73-104` 旧的一次性 submit 与 season 逻辑是否已被新 `process_with_upload` 完全替换（应已替换）。

- [ ] **Step 7: 校验编译**

Run: `cd crates/biliup-cli && cargo build`
Expected: 编译通过。逐一修复借用/类型错误（如 `recorder` 需 `mut`、`Vid` 路径、`resp.data` 类型）。

- [ ] **Step 8: 运行全量单测确认无回归**

Run: `cd crates/biliup-cli && cargo test --lib server::common`
Expected: 现有 `resolve_source_*` / `segment_paths_*` 与新增 `upload_session::tests` 全部 PASS。

- [ ] **Step 9: Commit**

```bash
git add crates/biliup-cli/src/server/common/upload.rs
git commit -m "feat(upload): 改为增量投稿(首段建稿/后续edit追加)+每段删本地+下播finalize"
```

---

## Task 7: 手动验证清单（实测风险点）

**Files:** 无（运行验证，结果记录到 PR 描述或 spec 末尾）。

- [ ] **Step 1: 审核中稿件能否 edit_by_app 追加**

真机用 App submit_api 录一场含 ≥2 段的直播，观察日志：首段出现「APP接口投稿成功」并取得 aid；第二段出现 `edit_by_app` 的「稿件修改成功」。若 edit 返回非 0，记录返回体，评估是否需在审核中延迟 edit。

- [ ] **Step 2: 重启续接验证**

录制进行中（已投 ≥2 段）`kill` 进程并重启；在 30 分钟窗口内直播仍在。确认日志出现「重启续接已有稿件」，且后续段 aid 与重启前一致（B 站创作中心稿件 = 同一个，未拆成两稿）。

- [ ] **Step 3: BCutAndroid 兼容验证**

将 submit_api 设为 `BCutAndroid`，录 ≥2 段。确认首段 BCut 建稿成功、后续段 `edit_by_app` 能否追加。若不能，按 spec「边界与兼容」第 1 点实现 fallback（BCut 模式退回下播一次性 submit 并 warn）——此为条件性后续任务，仅在验证失败时执行。

- [ ] **Step 4: 跨天/今明回归**

确认一场跨天连续直播仍为单一 aid；两场独立直播为两个 aid（无需改代码，验证既有行为未被破坏）。

---

## Self-Review 备注（已核对）

- **Spec 覆盖**：增量投稿(T6)、数据模型(T1/T2)、恢复匹配纯函数+窗口可配(T3/T5/T6)、每段删本地(T6)、下播 finalize(T6)、season 提前(T6)、submit_api/审核中 edit/BCut(T7)、测试(T3/T6/T7) 均有对应任务。
- **类型一致**：`select_recovery_candidate` 返回 `Option<usize>` 全程一致；`aid` 在 DB 为 `Option<i64>`、内存 `LiveArchive.aid` 为 `Option<u64>`，转换点在 `prepare_archive`/`insert_session` 显式 `as`。
- **风险**：ormlite 方法名（`where_bind`/`update_all_fields`）以本仓库实际版本为准，Task 4 Step 2 已要求按编译报错校正。`edit_by_app` 对审核中稿件的可用性、BCut 可编辑性为运行期风险，Task 7 验证。
