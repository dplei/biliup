use crate::server::common::missing_segment::insert_video_at_order;
use crate::server::errors::{AppError, AppResult};
use crate::server::infrastructure::connection_pool::ConnectionPool;
use crate::server::infrastructure::models::{
    FileItem, InsertUploadSession, StreamerInfo, UploadSession,
};
use biliup::bilibili::Video;
use chrono::{DateTime, Utc};
use error_stack::ResultExt;
use ormlite::{Insert, Model};
use serde::Serialize;
use sqlx::{Row, Sqlite, Transaction};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// 从候选会话中选出可续接的那条：同 room、未 finalize、updated_at 在窗口内、取最新。
/// 返回选中项在 `sessions` 中的下标，便于调用方按需取用（避免借用纠纷）。
/// 方案B 下「未 finalize」即 status="uploading"（累积中、尚未下播提交）。
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
            s.live_streamer_id == room_id && s.status != "finalized" && s.updated_at >= cutoff
        })
        .max_by_key(|(_, s)| s.updated_at)
        .map(|(i, _)| i)
}

/// 选出「已废弃」的会话下标：同 room、未 finalize、但 updated_at 已超出窗口。
/// 这些是上一场直播累积了分段却没等到下播提交（典型：进程在下播前重启、
/// 且停机期间直播已结束）。开播时应把它们一次性补提交并 finalize，避免上传
/// 到 B 站存储的分段永远滞留未投稿。
pub fn select_stale_session_indices(
    sessions: &[UploadSession],
    room_id: i64,
    now: DateTime<Utc>,
    window_minutes: i64,
) -> Vec<usize> {
    let cutoff = now - chrono::Duration::minutes(window_minutes);
    sessions
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            s.live_streamer_id == room_id && s.status != "finalized" && s.updated_at < cutoff
        })
        .map(|(i, _)| i)
        .collect()
}

/// 进程内的本场稿件状态。aid=None 表示尚未建稿。
#[derive(Debug, Default, Clone)]
pub struct LiveArchive {
    /// upload_session 行 id；None 表示该行尚未创建（全新直播、首段未建稿前）
    pub session_row_id: Option<i64>,
    pub aid: Option<u64>,
    pub bvid: Option<String>,
    pub videos: Vec<Video>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct SessionCompleteness {
    pub total_expected: i64,
    pub valid_videos: i64,
    pub pending: i64,
    pub uploading: i64,
    pub failed: i64,
    pub source_missing: i64,
    pub deleting: i64,
    pub succeeded: i64,
    pub unknown: i64,
    pub earliest_blocking_segment_id: Option<i64>,
    pub reasons: Vec<String>,
    /// Stable, sanitized member-level identity used to notify only when the blocking set changes.
    pub blocking_fingerprints: Vec<String>,
}

impl SessionCompleteness {
    pub fn is_complete(&self) -> bool {
        self.total_expected > 0
            && self.total_expected == self.succeeded
            && self.valid_videos == self.succeeded
            && self.reasons.is_empty()
    }

    pub fn incomplete_count(&self) -> i64 {
        let missing = self.total_expected.saturating_sub(self.valid_videos);
        if missing == 0 && !self.reasons.is_empty() {
            1
        } else {
            missing
        }
    }

    fn summary(&self) -> String {
        format!(
            "expected={}, valid={}, pending={}, uploading={}, failed={}, source_missing={}, deleting={}, unknown={}; {}",
            self.total_expected,
            self.valid_videos,
            self.pending,
            self.uploading,
            self.failed,
            self.source_missing,
            self.deleting,
            self.unknown,
            self.reasons.join("; ")
        )
    }

    fn signature(&self) -> AppResult<String> {
        serde_json::to_string(self).change_context(AppError::Unknown)
    }
}

#[derive(Debug)]
pub enum SubmitClaim {
    Claimed {
        token: String,
        videos: Vec<Video>,
    },
    Blocked {
        completeness: SessionCompleteness,
        changed: bool,
    },
    AlreadyClaimed,
    Finalized,
}

#[derive(Debug)]
struct LedgerRow {
    id: i64,
    file_path: String,
    normalized_file_path: Option<String>,
    segment_order: i64,
    status: String,
    video_json: Option<String>,
}

async fn inspect_completeness(
    tx: &mut Transaction<'_, Sqlite>,
    session_row_id: i64,
) -> AppResult<(SessionCompleteness, Vec<Video>)> {
    let rows = sqlx::query(
        "SELECT id, file_path, normalized_file_path, segment_order, status, video_json \
         FROM upload_missing_segment WHERE upload_session_id = ?1 \
         ORDER BY segment_order ASC, id ASC",
    )
    .bind(session_row_id)
    .fetch_all(&mut **tx)
    .await
    .change_context(AppError::Unknown)?
    .into_iter()
    .map(|row| LedgerRow {
        id: row.get("id"),
        file_path: row.get("file_path"),
        normalized_file_path: row.get("normalized_file_path"),
        segment_order: row.get("segment_order"),
        status: row.get("status"),
        video_json: row.get("video_json"),
    })
    .collect::<Vec<_>>();

    let mut result = SessionCompleteness {
        total_expected: i64::try_from(rows.len()).unwrap_or(i64::MAX),
        ..Default::default()
    };
    let mut videos = Vec::with_capacity(rows.len());
    let mut orders = HashSet::new();
    let mut sources = HashSet::new();
    let mut remote_filenames = HashMap::<String, (i64, Option<String>)>::new();

    if rows.is_empty() {
        result
            .reasons
            .push("session has no lifecycle baseline".to_string());
    }
    for (index, row) in rows.iter().enumerate() {
        match row.status.as_str() {
            "pending" => result.pending += 1,
            "uploading" => result.uploading += 1,
            "failed" => result.failed += 1,
            "source_missing" => result.source_missing += 1,
            "deleting" => result.deleting += 1,
            "succeeded" => result.succeeded += 1,
            other => {
                result.unknown += 1;
                result
                    .reasons
                    .push(format!("segment #{} has unknown status {other}", row.id));
            }
        }
        if row.status != "succeeded" && result.earliest_blocking_segment_id.is_none() {
            result.earliest_blocking_segment_id = Some(row.id);
        }
        if row.status != "succeeded" {
            result
                .blocking_fingerprints
                .push(format!("{}:status:{}", row.id, row.status));
        }
        if !orders.insert(row.segment_order) {
            result
                .reasons
                .push(format!("duplicate segment_order {}", row.segment_order));
            result.earliest_blocking_segment_id.get_or_insert(row.id);
            result
                .blocking_fingerprints
                .push(format!("{}:duplicate_order:{}", row.id, row.segment_order));
        }
        let expected_order = i64::try_from(index).unwrap_or(i64::MAX);
        if row.segment_order != expected_order {
            result.reasons.push(format!(
                "segment_order is not contiguous: expected {expected_order}, got {}",
                row.segment_order
            ));
            result.earliest_blocking_segment_id.get_or_insert(row.id);
            result
                .blocking_fingerprints
                .push(format!("{}:order_gap:{}", row.id, row.segment_order));
        }
        let source = row
            .normalized_file_path
            .as_deref()
            .unwrap_or(row.file_path.as_str());
        if !sources.insert(source.to_string()) {
            result
                .reasons
                .push(format!("duplicate source identity {source}"));
            result.earliest_blocking_segment_id.get_or_insert(row.id);
            result
                .blocking_fingerprints
                .push(format!("{}:duplicate_source", row.id));
        }
        if row.status == "succeeded" {
            let Some(json) = row.video_json.as_deref() else {
                result
                    .reasons
                    .push(format!("segment #{} has no video_json", row.id));
                result.earliest_blocking_segment_id.get_or_insert(row.id);
                result
                    .blocking_fingerprints
                    .push(format!("{}:missing_video_json", row.id));
                continue;
            };
            match serde_json::from_str::<Video>(json) {
                Ok(video) => {
                    if let Some((first_id, first_title)) = remote_filenames
                        .insert(video.filename.clone(), (row.id, video.title.clone()))
                    {
                        result.reasons.push(format!(
                            "remote filename {} is shared by segments #{first_id} ({:?}) and #{} ({:?})",
                            video.filename, first_title, row.id, video.title
                        ));
                        result.earliest_blocking_segment_id.get_or_insert(first_id);
                        result.blocking_fingerprints.push(format!(
                            "{first_id}:{}:duplicate_remote:{}",
                            row.id, video.filename
                        ));
                    }
                    result.valid_videos += 1;
                    videos.push(video);
                }
                Err(error) => {
                    result.reasons.push(format!(
                        "segment #{} has invalid video_json: {error}",
                        row.id
                    ));
                    result.earliest_blocking_segment_id.get_or_insert(row.id);
                    result
                        .blocking_fingerprints
                        .push(format!("{}:invalid_video_json", row.id));
                }
            }
        }
    }
    if result.pending + result.uploading + result.failed + result.source_missing + result.deleting
        > 0
    {
        result
            .reasons
            .push("one or more lifecycle rows are not succeeded".to_string());
    }
    Ok((result, videos))
}

pub async fn session_completeness(
    pool: &ConnectionPool,
    session_row_id: i64,
) -> AppResult<SessionCompleteness> {
    let mut tx = pool.begin().await.change_context(AppError::Unknown)?;
    let (result, _) = inspect_completeness(&mut tx, session_row_id).await?;
    tx.commit().await.change_context(AppError::Unknown)?;
    Ok(result)
}

/// Atomically validate the permanent lifecycle ledger, rebuild videos_json and acquire submit
/// ownership. No network-facing studio construction may happen before this succeeds.
pub async fn claim_complete_session(
    pool: &ConnectionPool,
    session_row_id: i64,
) -> AppResult<SubmitClaim> {
    let mut tx = pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .change_context(AppError::Unknown)?;
    let session =
        sqlx::query("SELECT status, submit_claim_token FROM upload_session WHERE id = ?1")
            .bind(session_row_id)
            .fetch_one(&mut *tx)
            .await
            .change_context(AppError::Unknown)?;
    if session.get::<String, _>("status") == "finalized" {
        return Ok(SubmitClaim::Finalized);
    }
    if session
        .get::<Option<String>, _>("submit_claim_token")
        .is_some()
    {
        return Ok(SubmitClaim::AlreadyClaimed);
    }
    let (completeness, videos) = inspect_completeness(&mut tx, session_row_id).await?;
    let now = Utc::now();
    if !completeness.is_complete() {
        let signature = completeness.signature()?;
        let previous = sqlx::query_scalar::<_, Option<String>>(
            "SELECT blocked_signature FROM upload_session WHERE id = ?1",
        )
        .bind(session_row_id)
        .fetch_one(&mut *tx)
        .await
        .change_context(AppError::Unknown)?;
        let changed = previous.as_deref() != Some(signature.as_str());
        sqlx::query(
            "UPDATE upload_session SET submit_state = 'blocked_missing_segments', \
             last_submit_error = ?1, blocked_signature = ?2, \
             blocked_count = blocked_count + 1 WHERE id = ?3",
        )
        .bind(completeness.summary())
        .bind(signature)
        .bind(session_row_id)
        .execute(&mut *tx)
        .await
        .change_context(AppError::Unknown)?;
        tx.commit().await.change_context(AppError::Unknown)?;
        return Ok(SubmitClaim::Blocked {
            completeness,
            changed,
        });
    }

    let token = uuid::Uuid::new_v4().to_string();
    let videos_json = serde_json::to_string(&videos).change_context(AppError::Unknown)?;
    let updated = sqlx::query(
        "UPDATE upload_session SET videos_json = ?1, submit_claim_token = ?2, \
         submit_claimed_at = ?3, submit_state = 'submitting', last_submit_error = NULL, \
         blocked_signature = NULL, updated_at = ?3 WHERE id = ?4 AND status != 'finalized' \
         AND submit_claim_token IS NULL",
    )
    .bind(videos_json)
    .bind(&token)
    .bind(now)
    .bind(session_row_id)
    .execute(&mut *tx)
    .await
    .change_context(AppError::Unknown)?;
    if updated.rows_affected() != 1 {
        return Ok(SubmitClaim::AlreadyClaimed);
    }
    tx.commit().await.change_context(AppError::Unknown)?;
    Ok(SubmitClaim::Claimed { token, videos })
}

/// 查询某 room 下所有未 finalize 的会话（供纯函数选择候选）。
pub async fn active_sessions_for_room(
    pool: &ConnectionPool,
    room_id: i64,
) -> AppResult<Vec<UploadSession>> {
    UploadSession::select()
        .where_("live_streamer_id = ? AND status != 'finalized'")
        .bind(room_id)
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

/// 首段上传成功后插入会话行（uploading 态：累积中、尚未提交，aid 暂空）。
pub async fn insert_uploading_session(
    pool: &ConnectionPool,
    room_id: i64,
    streamer_info_id: i64,
    videos: &[Video],
) -> AppResult<UploadSession> {
    let now = chrono::Utc::now();
    InsertUploadSession {
        live_streamer_id: room_id,
        streamer_info_id,
        aid: None,
        bvid: None,
        videos_json: serde_json::to_string(videos).change_context(AppError::Unknown)?,
        status: "uploading".to_string(),
        created_at: now,
        updated_at: now,
        submit_attempts: 0,
        last_submit_at: None,
        last_submit_error: None,
        submit_state: None,
    }
    .insert(pool)
    .await
    .change_context(AppError::Unknown)
}

/// 按 id 查会话所属的 StreamerInfo（补提交废弃会话时，需用它当时的标题/时间构建 studio）。
pub async fn get_streamer_info(pool: &ConnectionPool, id: i64) -> AppResult<StreamerInfo> {
    StreamerInfo::select()
        .where_("id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .change_context(AppError::Unknown)
}

/// 取文件路径的「词干」：basename 去掉最后一级扩展名。
/// 用于把 get_videos 列出的磁盘文件名与 filelist.file 归一化后比对，
/// 容忍路径前缀差异与扩展名有无（stream_gears 带扩展名 / ffmpeg 去扩展名都归一到同一词干）。
pub fn filename_stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 按磁盘文件名反查它属于哪个主播（streamer_info_id）。
/// 录制时每段都往 filelist 写了 `file → streamer_info_id` 映射（见 upload.rs 的 InsertFileItem）。
/// 这里按「词干」匹配，命中返回对应 streamer_info_id；无命中返回 None（调用方据此走占位兜底）。
pub async fn match_streamer_by_filename(
    pool: &ConnectionPool,
    file: &Path,
) -> AppResult<Option<i64>> {
    let stem = filename_stem(file);
    if stem.is_empty() {
        return Ok(None);
    }
    // 先用 LIKE 把词干作为子串粗筛，再在 Rust 里按词干精确比对，避免子串误命中。
    let candidates = FileItem::select()
        .where_("file LIKE ?")
        .bind(format!("%{stem}%"))
        .fetch_all(pool)
        .await
        .change_context(AppError::Unknown)?;
    Ok(candidates
        .into_iter()
        .find(|c| filename_stem(Path::new(&c.file)) == stem)
        .map(|c| c.streamer_info_id))
}

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
        _ => "status IN ('pending', 'failed', 'uploading', 'source_missing')",
    }
}

/// 公共：按 id 取行、就地修改、写回。
async fn mutate_session(
    pool: &ConnectionPool,
    session_row_id: i64,
    f: impl FnOnce(&mut UploadSession) -> AppResult<()>,
) -> AppResult<()> {
    let mut tx = pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .change_context(AppError::Unknown)?;
    let mut row = UploadSession::select()
        .where_("id = ? AND status != 'finalized' AND submit_claim_token IS NULL")
        .bind(session_row_id)
        .fetch_one(&mut *tx)
        .await
        .change_context(AppError::Unknown)?;
    f(&mut row)?;
    row.updated_at = chrono::Utc::now();
    row.update_all_fields(&mut *tx)
        .await
        .change_context(AppError::Unknown)?;
    tx.commit().await.change_context(AppError::Unknown)?;
    Ok(())
}

/// 追加段后更新 videos_json 与 updated_at。
pub async fn update_session_videos(
    pool: &ConnectionPool,
    session_row_id: i64,
    videos: &[Video],
) -> AppResult<()> {
    let videos_json = serde_json::to_string(videos).change_context(AppError::Unknown)?;
    mutate_session(pool, session_row_id, |row| {
        row.videos_json = videos_json;
        Ok(())
    })
    .await
}

/// 下播后一次性提交成功：写回 aid/bvid 并标记 finalized（本场结束、不再参与续接/补提交）。
pub async fn mark_submitted(
    pool: &ConnectionPool,
    session_row_id: i64,
    claim_token: &str,
    aid: u64,
    bvid: Option<String>,
) -> AppResult<()> {
    let updated = sqlx::query(
        "UPDATE upload_session SET aid = ?1, bvid = ?2, status = 'finalized', \
         submit_state = 'ok_with_aid', last_submit_at = ?3, last_submit_error = NULL, \
         submit_attempts = submit_attempts + 1, submit_claim_token = NULL, \
         submit_claimed_at = NULL, updated_at = ?3 \
         WHERE id = ?4 AND status != 'finalized' AND submit_claim_token = ?5",
    )
    .bind(i64::try_from(aid).unwrap_or(i64::MAX))
    .bind(bvid)
    .bind(Utc::now())
    .bind(session_row_id)
    .bind(claim_token)
    .execute(pool)
    .await
    .change_context(AppError::Unknown)?;
    if updated.rows_affected() != 1 {
        return Err(error_stack::Report::new(AppError::Custom(
            "submit claim no longer owns session during finalize".to_string(),
        )));
    }
    Ok(())
}

/// 记录一次投稿异常（ok_no_aid / failed）。不改 status/aid，仅落投稿状态，
/// 使「投稿成功却无 aid」「投稿接口失败」可持久查证。
pub async fn mark_submit_anomaly(
    pool: &ConnectionPool,
    session_row_id: i64,
    claim_token: &str,
    state: &str,
    error: String,
    release_claim: bool,
) -> AppResult<()> {
    let updated = sqlx::query(
        "UPDATE upload_session SET submit_state = ?1, last_submit_error = ?2, \
         last_submit_at = ?3, submit_attempts = submit_attempts + 1, \
         submit_claim_token = CASE WHEN ?4 THEN NULL ELSE submit_claim_token END, \
         submit_claimed_at = CASE WHEN ?4 THEN NULL ELSE submit_claimed_at END, updated_at = ?3 \
         WHERE id = ?5 AND submit_claim_token = ?6",
    )
    .bind(state)
    .bind(error)
    .bind(Utc::now())
    .bind(release_claim)
    .bind(session_row_id)
    .bind(claim_token)
    .execute(pool)
    .await
    .change_context(AppError::Unknown)?;
    if updated.rows_affected() != 1 {
        return Err(error_stack::Report::new(AppError::Custom(
            "submit claim no longer owns session while recording result".to_string(),
        )));
    }
    Ok(())
}

pub async fn submit_claim_is_owned(
    pool: &ConnectionPool,
    session_row_id: i64,
    claim_token: &str,
) -> AppResult<bool> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM upload_session WHERE id = ?1 \
         AND status != 'finalized' AND submit_claim_token = ?2)",
    )
    .bind(session_row_id)
    .bind(claim_token)
    .fetch_one(pool)
    .await
    .change_context(AppError::Unknown)
}

pub async fn release_submit_claim(
    pool: &ConnectionPool,
    session_row_id: i64,
    claim_token: &str,
    error: String,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE upload_session SET submit_state = 'failed', last_submit_error = ?1, \
         submit_claim_token = NULL, submit_claimed_at = NULL, updated_at = ?2 \
         WHERE id = ?3 AND submit_claim_token = ?4",
    )
    .bind(error)
    .bind(Utc::now())
    .bind(session_row_id)
    .bind(claim_token)
    .execute(pool)
    .await
    .change_context(AppError::Unknown)?;
    Ok(())
}

/// 从 videos_json 反序列化已投稿视频列表。
pub fn parse_videos(videos_json: &str) -> Vec<Video> {
    serde_json::from_str(videos_json).unwrap_or_default()
}

pub fn videos_with_inserted_segment(
    videos_json: &str,
    video: Video,
    segment_order: i64,
) -> AppResult<Vec<Video>> {
    let mut videos: Vec<Video> = serde_json::from_str(videos_json).unwrap_or_default();
    insert_video_at_order(&mut videos, video, segment_order);
    Ok(videos)
}

pub async fn insert_session_video_at_order(
    pool: &ConnectionPool,
    session_row_id: i64,
    video: Video,
    segment_order: i64,
) -> AppResult<Vec<Video>> {
    let mut updated = Vec::new();
    mutate_session(pool, session_row_id, |row| {
        updated = videos_with_inserted_segment(&row.videos_json, video, segment_order)?;
        row.videos_json = serde_json::to_string(&updated).change_context(AppError::Unknown)?;
        Ok(())
    })
    .await?;
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::infrastructure::connection_pool::ConnectionManager;
    use chrono::TimeZone;

    async fn completeness_pool() -> (tempfile::TempDir, ConnectionPool) {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("completeness.db");
        let pool = ConnectionManager::new_pool(database.to_str().unwrap())
            .await
            .unwrap();
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO streamerinfo (id, name, url, title, date, live_cover_path) \
             VALUES (9, 'gate-test', 'https://example.invalid/live', 'gate test', ?1, '')",
        )
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO upload_session \
             (id, live_streamer_id, streamer_info_id, videos_json, status, created_at, updated_at) \
             VALUES (70, 7, 9, '[]', 'uploading', ?1, ?1)",
        )
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        (directory, pool)
    }

    async fn insert_ledger(
        pool: &ConnectionPool,
        id: i64,
        order: i64,
        status: &str,
        path: &str,
        video: Option<&Video>,
    ) {
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO upload_missing_segment \
             (id, live_streamer_id, streamer_info_id, upload_session_id, file_path, \
              segment_order, status, next_retry_at, created_at, updated_at, \
              normalized_file_path, lifecycle_version, video_json) \
             VALUES (?1, 7, 9, 70, ?2, ?3, ?4, ?5, ?5, ?5, ?2, 2, ?6)",
        )
        .bind(id)
        .bind(path)
        .bind(order)
        .bind(status)
        .bind(now)
        .bind(video.map(|video| serde_json::to_string(video).unwrap()))
        .execute(pool)
        .await
        .unwrap();
    }

    fn session(id: i64, room_id: i64, status: &str, updated_at: DateTime<Utc>) -> UploadSession {
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
            submit_attempts: 0,
            last_submit_at: None,
            last_submit_error: None,
            submit_state: None,
        }
    }

    fn t(min_ago: i64, now: DateTime<Utc>) -> DateTime<Utc> {
        now - chrono::Duration::minutes(min_ago)
    }

    fn now_fixed() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 13, 12, 0, 0).unwrap()
    }

    #[tokio::test]
    async fn every_active_or_terminal_failure_status_blocks_submit_claim() {
        for status in [
            "pending",
            "uploading",
            "failed",
            "source_missing",
            "deleting",
            "mystery",
        ] {
            let (_directory, pool) = completeness_pool().await;
            insert_ledger(&pool, 1, 0, status, &format!("/{status}.flv"), None).await;
            let claim = claim_complete_session(&pool, 70).await.unwrap();
            let SubmitClaim::Blocked { completeness, .. } = claim else {
                panic!("{status} unexpectedly acquired submit claim")
            };
            assert!(!completeness.is_complete());
            let (state, attempts): (Option<String>, i64) = sqlx::query_as(
                "SELECT submit_state, submit_attempts FROM upload_session WHERE id = 70",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(state.as_deref(), Some("blocked_missing_segments"));
            assert_eq!(attempts, 0, "blocked checks are not remote submit attempts");
        }
    }

    #[tokio::test]
    async fn complete_claim_rebuilds_stable_order_and_preserves_equal_titles() {
        let (_directory, pool) = completeness_pool().await;
        let mut first = Video::new("remote-first");
        first.title = Some("04:54:30".to_string());
        let mut second = Video::new("remote-second");
        second.title = Some("04:54:30".to_string());
        insert_ledger(&pool, 2, 1, "succeeded", "/second.flv", Some(&second)).await;
        insert_ledger(&pool, 1, 0, "succeeded", "/first.flv", Some(&first)).await;

        let SubmitClaim::Claimed { videos, .. } = claim_complete_session(&pool, 70).await.unwrap()
        else {
            panic!("complete ledger should be claimable")
        };
        assert_eq!(
            videos
                .iter()
                .map(|v| v.filename.as_str())
                .collect::<Vec<_>>(),
            ["remote-first", "remote-second"]
        );
        let stored: String =
            sqlx::query_scalar("SELECT videos_json FROM upload_session WHERE id = 70")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<Video>>(&stored).unwrap().len(),
            2
        );
    }

    #[tokio::test]
    async fn duplicate_remote_identity_and_corrupt_json_block_claim() {
        let (_directory, pool) = completeness_pool().await;
        let video = Video::new("same-remote-file");
        insert_ledger(&pool, 1, 0, "succeeded", "/one.flv", Some(&video)).await;
        insert_ledger(&pool, 2, 1, "succeeded", "/two.flv", Some(&video)).await;
        let SubmitClaim::Blocked { completeness, .. } =
            claim_complete_session(&pool, 70).await.unwrap()
        else {
            panic!("duplicate remote identity must block")
        };
        assert!(
            completeness
                .reasons
                .iter()
                .any(|reason| reason.contains("remote filename"))
        );

        sqlx::query("UPDATE upload_missing_segment SET video_json = '{broken' WHERE id = 2")
            .execute(&pool)
            .await
            .unwrap();
        let SubmitClaim::Blocked { completeness, .. } =
            claim_complete_session(&pool, 70).await.unwrap()
        else {
            panic!("corrupt video_json must block")
        };
        assert!(
            completeness
                .reasons
                .iter()
                .any(|reason| reason.contains("invalid video_json"))
        );
    }

    #[tokio::test]
    async fn concurrent_finalize_has_exactly_one_owner() {
        let (_directory, pool) = completeness_pool().await;
        insert_ledger(
            &pool,
            1,
            0,
            "succeeded",
            "/one.flv",
            Some(&Video::new("one")),
        )
        .await;
        let (left, right) = tokio::join!(
            claim_complete_session(&pool, 70),
            claim_complete_session(&pool, 70)
        );
        let claims = [left.unwrap(), right.unwrap()];
        assert_eq!(
            claims
                .iter()
                .filter(|claim| matches!(claim, SubmitClaim::Claimed { .. }))
                .count(),
            1
        );
        assert_eq!(
            claims
                .iter()
                .filter(|claim| matches!(claim, SubmitClaim::AlreadyClaimed))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn final_missing_success_reopens_gate_for_exactly_one_submit() {
        let (_directory, pool) = completeness_pool().await;
        insert_ledger(&pool, 1, 0, "pending", "/last.flv", None).await;
        assert!(matches!(
            claim_complete_session(&pool, 70).await.unwrap(),
            SubmitClaim::Blocked { .. }
        ));
        sqlx::query(
            "UPDATE upload_missing_segment SET status = 'succeeded', video_json = ?1 WHERE id = 1",
        )
        .bind(serde_json::to_string(&Video::new("last")).unwrap())
        .execute(&pool)
        .await
        .unwrap();
        assert!(matches!(
            claim_complete_session(&pool, 70).await.unwrap(),
            SubmitClaim::Claimed { .. }
        ));
        assert!(matches!(
            claim_complete_session(&pool, 70).await.unwrap(),
            SubmitClaim::AlreadyClaimed
        ));
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

    #[test]
    fn stale_picks_only_out_of_window_non_finalized_same_room() {
        let now = now_fixed();
        let sessions = vec![
            session(1, 7, "uploading", t(5, now)),  // 窗口内 -> 不算废弃
            session(2, 7, "uploading", t(31, now)), // 超窗口 -> 废弃
            session(3, 7, "finalized", t(40, now)), // 已 finalize -> 跳过
            session(4, 8, "uploading", t(40, now)), // 别的 room -> 跳过
            session(5, 7, "uploading", t(90, now)), // 超窗口 -> 废弃
        ];
        assert_eq!(
            select_stale_session_indices(&sessions, 7, now, 30),
            vec![1, 4]
        );
    }

    #[test]
    fn stale_empty_when_all_in_window() {
        let now = now_fixed();
        let sessions = vec![session(1, 7, "uploading", t(10, now))];
        assert!(select_stale_session_indices(&sessions, 7, now, 30).is_empty());
    }

    fn video(name: &str) -> Video {
        Video {
            title: Some(name.to_string()),
            filename: name.to_string(),
            desc: String::new(),
        }
    }

    #[test]
    fn inserts_video_into_session_json_at_recorded_order() {
        let videos = vec![video("p1"), video("p2"), video("p4")];
        let json = serde_json::to_string(&videos).unwrap();

        let result = videos_with_inserted_segment(&json, video("p3"), 2).unwrap();

        let names: Vec<_> = result.into_iter().map(|v| v.filename).collect();
        assert_eq!(names, vec!["p1", "p2", "p3", "p4"]);
    }

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
            "status IN ('pending', 'failed', 'uploading', 'source_missing')"
        );
        assert_eq!(
            missing_status_where(Some("active")),
            "status IN ('pending', 'failed', 'uploading', 'source_missing')"
        );
        assert_eq!(
            missing_status_where(Some("succeeded")),
            "status = 'succeeded'"
        );
        assert_eq!(missing_status_where(Some("all")), "1 = 1");
        // 非法值归一到 active，避免注入与意外全表
        assert_eq!(
            missing_status_where(Some("garbage")),
            "status IN ('pending', 'failed', 'uploading', 'source_missing')"
        );
    }

    #[test]
    fn filename_stem_normalizes_prefix_and_extension() {
        // 带扩展名（stream_gears）与不带扩展名（ffmpeg 去扩展名）归一到同一词干
        assert_eq!(
            filename_stem(Path::new("小黄人2026-07-08T12_00_00.flv")),
            "小黄人2026-07-08T12_00_00"
        );
        assert_eq!(
            filename_stem(Path::new("小黄人2026-07-08T12_00_00")),
            "小黄人2026-07-08T12_00_00"
        );
        // 带路径前缀也归一到同一词干
        assert_eq!(
            filename_stem(Path::new("./data/小黄人2026-07-08T12_00_00.mp4")),
            "小黄人2026-07-08T12_00_00"
        );
        // 空/无文件名
        assert_eq!(filename_stem(Path::new("")), "");
    }

    #[tokio::test]
    async fn match_streamer_by_filename_resolves_via_filelist() {
        use crate::server::infrastructure::connection_pool::ConnectionManager;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let pool = ConnectionManager::new_pool(db_path.to_str().unwrap())
            .await
            .unwrap();
        // filelist.streamer_info_id 外键指向 streamerinfo，先建对应主播行
        sqlx::query(
            "INSERT INTO streamerinfo (id, name, url, title, date, live_cover_path) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(42_i64)
        .bind("小黄人")
        .bind("https://example.com/live")
        .bind("直播标题")
        .bind(Utc::now())
        .bind("")
        .execute(&pool)
        .await
        .unwrap();
        // filelist 存的是录制时的文件名（带扩展名，stream_gears）
        sqlx::query("INSERT INTO filelist (file, streamer_info_id) VALUES (?1, ?2)")
            .bind("小黄人2026-07-08T12_00_00.flv")
            .bind(42_i64)
            .execute(&pool)
            .await
            .unwrap();

        // 磁盘列表给的同名 basename → 命中 42
        assert_eq!(
            match_streamer_by_filename(&pool, Path::new("小黄人2026-07-08T12_00_00.flv"))
                .await
                .unwrap(),
            Some(42)
        );
        // 扩展名不同/带路径前缀 → 仍按词干命中 42
        assert_eq!(
            match_streamer_by_filename(&pool, Path::new("./小黄人2026-07-08T12_00_00.mp4"))
                .await
                .unwrap(),
            Some(42)
        );
        // 不存在的文件 → None（调用方走占位兜底）
        assert_eq!(
            match_streamer_by_filename(&pool, Path::new("别人2099-01-01T00_00_00.flv"))
                .await
                .unwrap(),
            None
        );
    }
}
