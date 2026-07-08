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
        _ => "status IN ('pending', 'failed', 'uploading')",
    }
}

/// 公共：按 id 取行、就地修改、写回。
async fn mutate_session(
    pool: &ConnectionPool,
    session_row_id: i64,
    f: impl FnOnce(&mut UploadSession) -> AppResult<()>,
) -> AppResult<()> {
    let mut row = UploadSession::select()
        .where_("id = ?")
        .bind(session_row_id)
        .fetch_one(pool)
        .await
        .change_context(AppError::Unknown)?;
    f(&mut row)?;
    row.updated_at = chrono::Utc::now();
    row.update_all_fields(pool)
        .await
        .change_context(AppError::Unknown)?;
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
    aid: u64,
    bvid: Option<String>,
) -> AppResult<()> {
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
}

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
    use chrono::TimeZone;

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
            "status IN ('pending', 'failed', 'uploading')"
        );
        assert_eq!(
            missing_status_where(Some("active")),
            "status IN ('pending', 'failed', 'uploading')"
        );
        assert_eq!(
            missing_status_where(Some("succeeded")),
            "status = 'succeeded'"
        );
        assert_eq!(missing_status_where(Some("all")), "1 = 1");
        // 非法值归一到 active，避免注入与意外全表
        assert_eq!(
            missing_status_where(Some("garbage")),
            "status IN ('pending', 'failed', 'uploading')"
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
