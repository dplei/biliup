use crate::UploadLine;
use crate::server::errors::{AppError, AppResult};
use crate::server::infrastructure::connection_pool::ConnectionPool;
use crate::server::infrastructure::models::{InsertUploadMissingSegment, UploadMissingSegment};
use biliup::bilibili::{Studio, Video};
use chrono::{DateTime, Utc};
use error_stack::ResultExt;
use std::path::Path;
use tracing::info;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryUploadLine {
    Bda2,
    Tx,
    Bldsa,
    Auto,
}

pub const FALLBACK_LINES: [RecoveryUploadLine; 4] = [
    RecoveryUploadLine::Bda2,
    RecoveryUploadLine::Tx,
    RecoveryUploadLine::Bldsa,
    RecoveryUploadLine::Auto,
];

pub fn next_line_index(current: i64) -> i64 {
    let len = FALLBACK_LINES.len() as i64;
    if current < 0 { 0 } else { (current + 1) % len }
}

pub fn retry_delay_for_attempt(attempts: i64) -> chrono::Duration {
    match attempts {
        i if i <= 0 => chrono::Duration::minutes(10),
        1 => chrono::Duration::minutes(30),
        2 => chrono::Duration::hours(1),
        3 => chrono::Duration::hours(2),
        _ => chrono::Duration::hours(6),
    }
}

pub fn normalize_segment_order(existing_count: usize, segment_order: i64) -> usize {
    if segment_order <= 0 {
        return 0;
    }
    (segment_order as usize).min(existing_count)
}

pub fn insert_video_at_order(videos: &mut Vec<Video>, video: Video, segment_order: i64) {
    let index = normalize_segment_order(videos.len(), segment_order);
    videos.insert(index, video);
}

/// Inserts the recovered video at the enqueue-relative `segment_order`; the exact server
/// position may differ if other parts were recovered between enqueue and now (best-effort).
pub fn patch_studio_videos(studio: &mut Studio, video: Video, segment_order: i64) {
    insert_video_at_order(&mut studio.videos, video, segment_order);
}

pub fn mark_retry_failure(row: &mut UploadMissingSegment, error: String, now: DateTime<Utc>) {
    row.attempts += 1;
    row.line_index = next_line_index(row.line_index);
    row.status = "failed".to_string();
    row.last_error = Some(error);
    row.next_retry_at = now + retry_delay_for_attempt(row.attempts);
    row.updated_at = now;
}

pub fn mark_retry_success(row: &mut UploadMissingSegment, now: DateTime<Utc>) {
    row.status = "succeeded".to_string();
    row.updated_at = now;
}

/// Returns a best-effort position hint captured at enqueue time; the absolute server position
/// may drift if other recoveries occur before this segment is retried.
pub fn next_segment_order(successful_count: usize, missing_before_or_at_end: usize) -> i64 {
    (successful_count + missing_before_or_at_end) as i64
}

pub fn is_due_for_silent_recovery(
    status: &str,
    next_retry_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> bool {
    matches!(status, "pending" | "failed") && next_retry_at <= now
}

pub fn can_delete_missing_segment(status: &str) -> bool {
    matches!(status, "pending" | "failed")
}

pub fn reset_for_manual_retry(row: &mut UploadMissingSegment, now: DateTime<Utc>) {
    row.status = "failed".to_string();
    row.last_error = Some("manual retry requested from uploading state".to_string());
    row.next_retry_at = now;
    row.updated_at = now;
}

#[derive(Debug)]
pub enum MissingSegmentDeleteClaim {
    Claimed(UploadMissingSegment),
    NotFound,
    NotDeletable { status: String },
}

pub async fn claim_missing_segment_for_delete(
    pool: &ConnectionPool,
    id: i64,
    now: DateTime<Utc>,
) -> AppResult<MissingSegmentDeleteClaim> {
    let claim = sqlx::query(
        "UPDATE upload_missing_segment SET status = 'deleting', updated_at = ?1 \
         WHERE id = ?2 AND status IN ('pending', 'failed')",
    )
    .bind(now)
    .bind(id)
    .execute(pool)
    .await
    .change_context(AppError::Unknown)?;

    if claim.rows_affected() == 1 {
        let row = sqlx::query_as::<_, UploadMissingSegment>(
            "SELECT * FROM upload_missing_segment WHERE id = ?",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .change_context(AppError::Unknown)?;
        return Ok(MissingSegmentDeleteClaim::Claimed(row));
    }

    let status =
        sqlx::query_scalar::<_, String>("SELECT status FROM upload_missing_segment WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await
            .change_context(AppError::Unknown)?;

    Ok(match status {
        Some(status) => MissingSegmentDeleteClaim::NotDeletable { status },
        None => MissingSegmentDeleteClaim::NotFound,
    })
}

pub async fn remove_missing_segment_files(
    file_path: &Path,
    danmaku_file_path: Option<&Path>,
) -> AppResult<()> {
    async fn remove_one(path: &Path) -> AppResult<()> {
        match tokio::fs::remove_file(path).await {
            Ok(()) => {
                info!(file = %path.display(), "deleted missing upload local file");
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                info!(file = %path.display(), "missing upload local file already absent");
                Ok(())
            }
            Err(e) => Err(e).change_context(AppError::Unknown),
        }
    }

    remove_one(file_path).await?;
    if let Some(path) = danmaku_file_path {
        remove_one(path).await?;
    }
    Ok(())
}

pub async fn enqueue_missing_segment(
    pool: &ConnectionPool,
    live_streamer_id: i64,
    streamer_info_id: i64,
    upload_session_id: Option<i64>,
    aid: Option<i64>,
    file_path: &Path,
    danmaku_file_path: Option<&Path>,
    segment_order: i64,
    error: String,
    now: DateTime<Utc>,
) -> AppResult<()> {
    let item = InsertUploadMissingSegment {
        live_streamer_id,
        streamer_info_id,
        upload_session_id,
        aid,
        file_path: file_path.display().to_string(),
        danmaku_file_path: danmaku_file_path.map(|p| p.display().to_string()),
        segment_order,
        status: "failed".to_string(),
        attempts: 1,
        line_index: 1,
        next_retry_at: now + retry_delay_for_attempt(1),
        last_error: Some(error),
        created_at: now,
        updated_at: now,
    };

    let sql = r#"
        INSERT INTO upload_missing_segment
            (live_streamer_id, streamer_info_id, upload_session_id, aid, file_path, danmaku_file_path,
             segment_order, status, attempts, line_index, next_retry_at, last_error, created_at, updated_at)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
        ON CONFLICT(live_streamer_id, file_path) WHERE status IN ('pending', 'uploading', 'failed')
        DO UPDATE SET
            upload_session_id = excluded.upload_session_id,
            aid = COALESCE(upload_missing_segment.aid, excluded.aid),
            segment_order = excluded.segment_order,
            status = 'failed',
            attempts = upload_missing_segment.attempts + 1,
            line_index = excluded.line_index,
            next_retry_at = excluded.next_retry_at,
            last_error = excluded.last_error,
            updated_at = excluded.updated_at
    "#;

    sqlx::query(sql)
        .bind(item.live_streamer_id)
        .bind(item.streamer_info_id)
        .bind(item.upload_session_id)
        .bind(item.aid)
        .bind(item.file_path)
        .bind(item.danmaku_file_path)
        .bind(item.segment_order)
        .bind(item.status)
        .bind(item.attempts)
        .bind(item.line_index)
        .bind(item.next_retry_at)
        .bind(item.last_error)
        .bind(item.created_at)
        .bind(item.updated_at)
        .execute(pool)
        .await
        .change_context(AppError::Unknown)?;

    Ok(())
}

pub fn upload_line_for_recovery(index: i64) -> Option<UploadLine> {
    match FALLBACK_LINES[index.rem_euclid(FALLBACK_LINES.len() as i64) as usize] {
        RecoveryUploadLine::Bda2 => Some(UploadLine::Bda2),
        RecoveryUploadLine::Tx => Some(UploadLine::Tx),
        RecoveryUploadLine::Bldsa => Some(UploadLine::Bldsa),
        RecoveryUploadLine::Auto => None,
    }
}

pub async fn due_missing_segments_for_session(
    pool: &ConnectionPool,
    upload_session_id: i64,
    now: DateTime<Utc>,
) -> AppResult<Vec<UploadMissingSegment>> {
    sqlx::query_as::<_, UploadMissingSegment>(
        "SELECT * FROM upload_missing_segment \
         WHERE upload_session_id = ? AND status IN ('pending', 'failed') AND next_retry_at <= ? \
         ORDER BY segment_order ASC, id ASC",
    )
    .bind(upload_session_id)
    .bind(now)
    .fetch_all(pool)
    .await
    .change_context(AppError::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::infrastructure::connection_pool::ConnectionManager;
    use chrono::TimeZone;

    fn video(name: &str) -> Video {
        Video {
            title: Some(name.to_string()),
            filename: name.to_string(),
            desc: String::new(),
        }
    }

    fn missing_row() -> UploadMissingSegment {
        let now = chrono::Utc.with_ymd_and_hms(2026, 6, 18, 12, 0, 0).unwrap();
        UploadMissingSegment {
            id: 1,
            live_streamer_id: 10,
            streamer_info_id: 20,
            upload_session_id: Some(30),
            aid: None,
            file_path: "/opt/p3.flv".to_string(),
            danmaku_file_path: None,
            segment_order: 2,
            status: "pending".to_string(),
            attempts: 0,
            line_index: 0,
            next_retry_at: now,
            last_error: None,
            created_at: now,
            updated_at: now,
        }
    }

    async fn test_pool() -> (tempfile::TempDir, ConnectionPool) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let pool = ConnectionManager::new_pool(db_path.to_str().unwrap())
            .await
            .unwrap();
        let now = chrono::Utc.with_ymd_and_hms(2026, 6, 18, 12, 0, 0).unwrap();
        sqlx::query(
            "INSERT INTO streamerinfo (id, name, url, title, date, live_cover_path) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(20_i64)
        .bind("test")
        .bind("https://example.com/live")
        .bind("test stream")
        .bind(now)
        .bind("")
        .execute(&pool)
        .await
        .unwrap();
        (dir, pool)
    }

    async fn insert_missing_row_with_status(
        pool: &ConnectionPool,
        status: &str,
        file_name: &str,
    ) -> i64 {
        let now = chrono::Utc.with_ymd_and_hms(2026, 6, 18, 12, 0, 0).unwrap();
        let file_path = Path::new(file_name);
        enqueue_missing_segment(
            pool,
            10,
            20,
            None,
            None,
            file_path,
            None,
            0,
            "test".to_string(),
            now,
        )
        .await
        .unwrap();

        let id = sqlx::query_scalar::<_, i64>(
            "SELECT id FROM upload_missing_segment WHERE file_path = ?",
        )
        .bind(file_path.display().to_string())
        .fetch_one(pool)
        .await
        .unwrap();

        sqlx::query("UPDATE upload_missing_segment SET status = ? WHERE id = ?")
            .bind(status)
            .bind(id)
            .execute(pool)
            .await
            .unwrap();

        id
    }

    #[test]
    fn delete_is_allowed_only_for_unrecovered_rows() {
        assert!(can_delete_missing_segment("pending"));
        assert!(can_delete_missing_segment("failed"));
        assert!(!can_delete_missing_segment("uploading"));
        assert!(!can_delete_missing_segment("succeeded"));
    }

    #[tokio::test]
    async fn claim_missing_segment_for_delete_marks_failed_row_deleting() {
        let (_dir, pool) = test_pool().await;
        let id = insert_missing_row_with_status(&pool, "failed", "/tmp/delete-failed.flv").await;
        let now = chrono::Utc.with_ymd_and_hms(2026, 6, 18, 12, 5, 0).unwrap();

        let claim = claim_missing_segment_for_delete(&pool, id, now)
            .await
            .unwrap();

        let MissingSegmentDeleteClaim::Claimed(row) = claim else {
            panic!("failed row should be claimed for deletion");
        };
        assert_eq!(row.id, id);
        assert_eq!(row.status, "deleting");
        assert_eq!(row.updated_at, now);

        let status = sqlx::query_scalar::<_, String>(
            "SELECT status FROM upload_missing_segment WHERE id = ?",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "deleting");
    }

    #[tokio::test]
    async fn claim_missing_segment_for_delete_rejects_non_deletable_rows() {
        let (_dir, pool) = test_pool().await;
        let now = chrono::Utc.with_ymd_and_hms(2026, 6, 18, 12, 5, 0).unwrap();

        for status in ["uploading", "succeeded"] {
            let id =
                insert_missing_row_with_status(&pool, status, &format!("/tmp/delete-{status}.flv"))
                    .await;

            let claim = claim_missing_segment_for_delete(&pool, id, now)
                .await
                .unwrap();

            match claim {
                MissingSegmentDeleteClaim::NotDeletable { status: actual } => {
                    assert_eq!(actual, status);
                }
                other => panic!("expected non-deletable claim result, got {other:?}"),
            }

            let stored_status = sqlx::query_scalar::<_, String>(
                "SELECT status FROM upload_missing_segment WHERE id = ?",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(stored_status, status);
        }
    }

    #[tokio::test]
    async fn remove_missing_segment_files_deletes_video_and_danmaku() {
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("part.flv");
        let danmaku = dir.path().join("part.xml");
        tokio::fs::write(&video, b"video").await.unwrap();
        tokio::fs::write(&danmaku, b"danmaku").await.unwrap();

        remove_missing_segment_files(&video, Some(&danmaku))
            .await
            .unwrap();

        assert!(!video.exists());
        assert!(!danmaku.exists());
    }

    #[tokio::test]
    async fn remove_missing_segment_files_ignores_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("missing.flv");
        let danmaku = dir.path().join("missing.xml");

        remove_missing_segment_files(&video, Some(&danmaku))
            .await
            .unwrap();

        assert!(!video.exists());
        assert!(!danmaku.exists());
    }

    #[test]
    fn retry_reset_turns_uploading_into_due_failed_row() {
        let mut row = missing_row();
        row.status = "uploading".to_string();
        row.attempts = 7;
        row.next_retry_at = chrono::Utc.with_ymd_and_hms(2026, 6, 18, 13, 0, 0).unwrap();
        let now = chrono::Utc.with_ymd_and_hms(2026, 6, 18, 12, 5, 0).unwrap();

        reset_for_manual_retry(&mut row, now);

        assert_eq!(row.status, "failed");
        assert_eq!(row.attempts, 7);
        assert_eq!(
            row.last_error.as_deref(),
            Some("manual retry requested from uploading state")
        );
        assert_eq!(row.next_retry_at, now);
        assert_eq!(row.updated_at, now);
    }

    #[test]
    fn inserts_missing_video_at_recorded_zero_based_order() {
        let mut videos = vec![video("p1"), video("p2"), video("p4")];

        insert_video_at_order(&mut videos, video("p3"), 2);

        let names: Vec<_> = videos.into_iter().map(|v| v.filename).collect();
        assert_eq!(names, vec!["p1", "p2", "p3", "p4"]);
    }

    #[test]
    fn appends_when_recorded_order_is_after_current_end() {
        let mut videos = vec![video("p1"), video("p2")];

        insert_video_at_order(&mut videos, video("p9"), 8);

        let names: Vec<_> = videos.into_iter().map(|v| v.filename).collect();
        assert_eq!(names, vec!["p1", "p2", "p9"]);
    }

    #[test]
    fn clamps_negative_order_to_front() {
        assert_eq!(normalize_segment_order(3, -5), 0);
    }

    #[test]
    fn rotates_upload_lines_through_all_fallbacks() {
        assert_eq!(next_line_index(0), 1);
        assert_eq!(next_line_index(1), 2);
        assert_eq!(next_line_index(2), 3);
        assert_eq!(next_line_index(3), 0);
        assert_eq!(next_line_index(7), 0);
    }

    #[test]
    fn retry_delay_starts_at_ten_minutes_and_caps_at_six_hours() {
        assert_eq!(retry_delay_for_attempt(0), chrono::Duration::minutes(10));
        assert_eq!(retry_delay_for_attempt(1), chrono::Duration::minutes(30));
        assert_eq!(retry_delay_for_attempt(2), chrono::Duration::hours(1));
        assert_eq!(retry_delay_for_attempt(3), chrono::Duration::hours(2));
        assert_eq!(retry_delay_for_attempt(9), chrono::Duration::hours(6));
    }

    #[test]
    fn failure_transition_rotates_line_and_schedules_retry() {
        let mut row = missing_row();
        let now = chrono::Utc.with_ymd_and_hms(2026, 6, 18, 12, 5, 0).unwrap();

        mark_retry_failure(&mut row, "timeout".to_string(), now);

        assert_eq!(row.status, "failed");
        assert_eq!(row.attempts, 1);
        assert_eq!(row.line_index, 1);
        assert_eq!(row.last_error.as_deref(), Some("timeout"));
        assert_eq!(row.next_retry_at, now + chrono::Duration::minutes(30));
        assert_eq!(row.updated_at, now);
    }

    #[test]
    fn success_transition_marks_row_succeeded_without_changing_order() {
        let mut row = missing_row();
        row.status = "uploading".to_string();
        row.attempts = 2;
        let now = chrono::Utc.with_ymd_and_hms(2026, 6, 18, 12, 5, 0).unwrap();

        mark_retry_success(&mut row, now);

        assert_eq!(row.status, "succeeded");
        assert_eq!(row.attempts, 2);
        assert_eq!(row.segment_order, 2);
        assert_eq!(row.updated_at, now);
    }

    #[test]
    fn next_segment_order_counts_successes_and_prior_missing_segments() {
        assert_eq!(next_segment_order(0, 0), 0);
        assert_eq!(next_segment_order(2, 0), 2);
        assert_eq!(next_segment_order(2, 1), 3);
    }

    #[test]
    fn silent_recovery_only_picks_pending_or_failed_rows_that_are_due() {
        let now = chrono::Utc.with_ymd_and_hms(2026, 6, 18, 12, 0, 0).unwrap();
        assert!(is_due_for_silent_recovery("pending", now, now));
        assert!(is_due_for_silent_recovery(
            "failed",
            now - chrono::Duration::minutes(1),
            now
        ));
        assert!(!is_due_for_silent_recovery(
            "failed",
            now + chrono::Duration::minutes(1),
            now
        ));
        assert!(!is_due_for_silent_recovery("uploading", now, now));
        assert!(!is_due_for_silent_recovery("succeeded", now, now));
    }

    #[test]
    fn patch_studio_inserts_video_at_recorded_order() {
        let mut studio = Studio::builder()
            .title("archive".to_string())
            .videos(vec![video("p1"), video("p2"), video("p4")])
            .source(String::new())
            .cover(String::new())
            .desc(String::new())
            .dynamic(String::new())
            .tag(String::new())
            .dolby(0)
            .no_reprint(0)
            .charging_pay(0)
            .up_selection_reply(false)
            .up_close_reply(false)
            .up_close_danmu(false)
            .build();

        patch_studio_videos(&mut studio, video("p3"), 2);

        let names: Vec<_> = studio.videos.into_iter().map(|v| v.filename).collect();
        assert_eq!(names, vec!["p1", "p2", "p3", "p4"]);
    }
}
