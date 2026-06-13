use crate::server::errors::{AppError, AppResult};
use crate::server::infrastructure::connection_pool::ConnectionPool;
use crate::server::infrastructure::models::{InsertUploadSession, UploadSession};
use biliup::bilibili::Video;
use chrono::{DateTime, Utc};
use error_stack::ResultExt;
use ormlite::{Insert, Model};

/// 从候选会话中选出可续接的那条：同 room、未 finalize、updated_at 在窗口内、取最新。
/// 返回选中项在 `sessions` 中的下标，便于调用方按需取用（避免借用纠纷）。
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
        .where_("id = ?")
        .bind(session_row_id)
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

/// 续接行上首次建稿成功后，写回 aid/bvid/videos/status。
pub async fn update_session_after_submit(
    pool: &ConnectionPool,
    session_row_id: i64,
    aid: u64,
    bvid: Option<String>,
    videos: &[Video],
) -> AppResult<()> {
    let mut row = UploadSession::select()
        .where_("id = ?")
        .bind(session_row_id)
        .fetch_one(pool)
        .await
        .change_context(AppError::Unknown)?;
    row.aid = Some(aid as i64);
    row.bvid = bvid;
    row.videos_json = serde_json::to_string(videos).change_context(AppError::Unknown)?;
    row.status = "submitted".to_string();
    row.updated_at = chrono::Utc::now();
    row.update_all_fields(pool)
        .await
        .change_context(AppError::Unknown)?;
    Ok(())
}

/// 下播收尾：标记 finalized。
pub async fn finalize_session(pool: &ConnectionPool, session_row_id: i64) -> AppResult<()> {
    let mut row = UploadSession::select()
        .where_("id = ?")
        .bind(session_row_id)
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
