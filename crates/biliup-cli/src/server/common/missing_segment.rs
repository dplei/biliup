use crate::server::common::attempt_lease::{AttemptPhase, LeaseSnapshot, classify_stale_lease};
use crate::server::common::upload::{
    CancelAttemptResult, cancel_registered_attempt, fail_enrolled_attempt_with_outcome,
};
use crate::server::errors::{AppError, AppResult};
use crate::server::infrastructure::connection_pool::ConnectionPool;
use crate::server::infrastructure::models::{InsertUploadMissingSegment, UploadMissingSegment};
use biliup::bilibili::{Studio, Video};
use chrono::{DateTime, Utc};
use error_stack::ResultExt;
use std::path::Path;
use tracing::{error, info};

/// `line_index` is a wrapping failure counter on legacy (lifecycle_version 1) rows. It no longer
/// selects anything: line choice belongs to `upload_line_selection`, which reads `config.lines`
/// and the persistent cooldown state instead of rotating through a fixed list.
const LINE_INDEX_MODULUS: i64 = 3;

pub fn next_line_index(current: i64) -> i64 {
    if current < 0 {
        0
    } else {
        (current + 1) % LINE_INDEX_MODULUS
    }
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

/// One `uploading` row, in the shape the stale verdict needs.
#[derive(sqlx::FromRow)]
struct LeaseRow {
    id: i64,
    attempt_token: Option<String>,
    attempt_phase: Option<String>,
    phase_started_at: Option<DateTime<Utc>>,
    last_heartbeat_at: Option<DateTime<Utc>>,
    last_progress_at: Option<DateTime<Utc>>,
    upload_started_at: Option<DateTime<Utc>>,
    updated_at: DateTime<Utc>,
    total_bytes: Option<i64>,
}

impl LeaseRow {
    fn snapshot(&self) -> LeaseSnapshot {
        LeaseSnapshot {
            phase: self.attempt_phase.as_deref().and_then(AttemptPhase::parse),
            phase_started_at: self.phase_started_at,
            last_heartbeat_at: self.last_heartbeat_at,
            last_progress_at: self.last_progress_at,
            upload_started_at: self.upload_started_at,
            updated_at: self.updated_at,
            total_bytes: self.total_bytes,
        }
    }
}

async fn uploading_leases(pool: &ConnectionPool) -> AppResult<Vec<LeaseRow>> {
    sqlx::query_as::<_, LeaseRow>(
        "SELECT id, attempt_token, attempt_phase, phase_started_at, last_heartbeat_at, \
                last_progress_at, upload_started_at, updated_at, total_bytes \
         FROM upload_missing_segment \
         WHERE lifecycle_version = 2 AND status = 'uploading'",
    )
    .fetch_all(pool)
    .await
    .change_context(AppError::Unknown)
}

/// Converge leases whose owner is gone or whose phase ran past its deadline.
///
/// Two things this does that the old five-minute blanket rule did not:
///
/// 1. It asks [`AttemptPhase`] which deadline applies, so a 3.32 GB audio normalization is not
///    mistaken for a stalled network transfer;
/// 2. it cancels a still-running in-process attempt and waits for it to exit *before* rewriting
///    the row. The old reaper was completely decoupled from the attempt registry, so it produced
///    ghost uploads that raced the replacement attempt for the same lifecycle row.
pub async fn recover_stale_upload_attempts(
    pool: &ConnectionPool,
    now: DateTime<Utc>,
) -> AppResult<u64> {
    let mut recovered = 0u64;
    for row in uploading_leases(pool).await? {
        let Some(reason) = classify_stale_lease(&row.snapshot(), now) else {
            continue;
        };
        let Some(token) = row.attempt_token.clone() else {
            // No token means no lease to CAS against; leave it for manual inspection rather than
            // stomping a row whose ownership cannot be proven.
            error!(
                missing_id = row.id,
                ?reason,
                "uploading row has no attempt token; leaving it for manual inspection"
            );
            continue;
        };
        let outcome = match cancel_registered_attempt(row.id, &token).await {
            CancelAttemptResult::Exited => {
                info!(
                    missing_id = row.id,
                    ?reason,
                    "cancelled a locally running attempt before converging its lease"
                );
                "cancelled"
            }
            CancelAttemptResult::TimedOut => {
                // It is still running and holding the file. Reissuing the lease now would give us
                // two uploads of the same segment, which is worse than waiting one more cycle.
                error!(
                    missing_id = row.id,
                    ?reason,
                    "stale attempt did not exit within the cancellation wait; retrying next cycle"
                );
                continue;
            }
            CancelAttemptResult::NotRegistered => "stale",
        };
        if fail_enrolled_attempt_with_outcome(
            pool,
            row.id,
            &token,
            reason.as_error().to_string(),
            outcome,
            now,
        )
        .await?
        {
            recovered += 1;
        }
    }
    Ok(recovered)
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct MissingSegmentHealth {
    /// `upload_missing_segment.status` counts across all lifecycle_version=2 rows — the direct
    /// answer to "how many pending/uploading/failed/succeeded/source_missing segments exist now".
    pub status_counts: std::collections::HashMap<String, i64>,
    /// Rows still `uploading` past `STALE_ATTEMPT_AFTER` that the background recovery loop
    /// (`start_stale_attempt_recovery`) has not yet converged back to `failed`.
    pub stale_uploading_count: i64,
    pub oldest_stale_uploading_secs: Option<i64>,
}

pub async fn missing_segment_health(
    pool: &ConnectionPool,
    now: DateTime<Utc>,
) -> AppResult<MissingSegmentHealth> {
    let counts: Vec<(String, i64)> = sqlx::query_as(
        "SELECT status, COUNT(*) FROM upload_missing_segment \
         WHERE lifecycle_version = 2 GROUP BY status",
    )
    .fetch_all(pool)
    .await
    .change_context(AppError::Unknown)?;
    // Same verdict as the reaper, so the health endpoint can never disagree with what the
    // background loop is about to do.
    let stale = uploading_leases(pool)
        .await?
        .into_iter()
        .filter_map(|row| {
            let snapshot = row.snapshot();
            classify_stale_lease(&snapshot, now).map(|_| {
                snapshot
                    .last_progress_at
                    .or(snapshot.phase_started_at)
                    .or(snapshot.upload_started_at)
                    .unwrap_or(snapshot.updated_at)
            })
        })
        .collect::<Vec<_>>();
    Ok(MissingSegmentHealth {
        status_counts: counts.into_iter().collect(),
        stale_uploading_count: stale.len() as i64,
        oldest_stale_uploading_secs: stale.iter().map(|since| (now - *since).num_seconds()).max(),
    })
}

pub fn start_stale_attempt_recovery(pool: ConnectionPool) {
    tokio::spawn(async move {
        loop {
            match recover_stale_upload_attempts(&pool, Utc::now()).await {
                Ok(recovered) if recovered > 0 => {
                    info!(recovered, "recovered stale upload attempt leases");
                }
                Ok(_) => {}
                Err(error) => error!(?error, "failed to recover stale upload attempt leases"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });
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
    enqueue_segment(
        pool,
        live_streamer_id,
        streamer_info_id,
        upload_session_id,
        aid,
        file_path,
        danmaku_file_path,
        segment_order,
        "failed",
        1,
        1,
        now + retry_delay_for_attempt(1),
        error,
        now,
    )
    .await
}

/// Queue a segment that never reached the upload attempt because uploader initialization failed.
/// It is immediately due so the next healthy session can recover it without waiting for the
/// ordinary upload-failure backoff.
#[allow(clippy::too_many_arguments)]
pub async fn enqueue_pending_segment(
    pool: &ConnectionPool,
    live_streamer_id: i64,
    streamer_info_id: i64,
    upload_session_id: Option<i64>,
    aid: Option<i64>,
    file_path: &Path,
    danmaku_file_path: Option<&Path>,
    segment_order: i64,
    reason: String,
    now: DateTime<Utc>,
) -> AppResult<()> {
    enqueue_segment(
        pool,
        live_streamer_id,
        streamer_info_id,
        upload_session_id,
        aid,
        file_path,
        danmaku_file_path,
        segment_order,
        "pending",
        0,
        0,
        now,
        reason,
        now,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn enqueue_segment(
    pool: &ConnectionPool,
    live_streamer_id: i64,
    streamer_info_id: i64,
    upload_session_id: Option<i64>,
    aid: Option<i64>,
    file_path: &Path,
    danmaku_file_path: Option<&Path>,
    segment_order: i64,
    status: &str,
    attempts: i64,
    line_index: i64,
    next_retry_at: DateTime<Utc>,
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
        status: status.to_string(),
        attempts,
        line_index,
        next_retry_at,
        last_error: Some(error),
        created_at: now,
        updated_at: now,
        normalized_file_path: None,
        lifecycle_version: 1,
        video_json: None,
        total_bytes: None,
        uploaded_bytes: 0,
        current_line: None,
        upload_started_at: None,
        last_progress_at: None,
        attempt_token: None,
        attempt_phase: None,
        phase_started_at: None,
        last_heartbeat_at: None,
        line_source: None,
        last_chunk_index: None,
        last_chunk_started_at: None,
        last_chunk_error: None,
    };

    let sql = r#"
        INSERT INTO upload_missing_segment
            (live_streamer_id, streamer_info_id, upload_session_id, aid, file_path, danmaku_file_path,
             segment_order, status, attempts, line_index, next_retry_at, last_error, created_at, updated_at)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
        ON CONFLICT(live_streamer_id, file_path) WHERE status IN ('pending', 'uploading', 'failed')
        DO UPDATE SET
            upload_session_id = CASE WHEN excluded.status = 'failed' THEN excluded.upload_session_id ELSE COALESCE(upload_missing_segment.upload_session_id, excluded.upload_session_id) END,
            aid = COALESCE(upload_missing_segment.aid, excluded.aid),
            segment_order = excluded.segment_order,
            status = CASE WHEN excluded.status = 'failed' THEN 'failed' ELSE upload_missing_segment.status END,
            attempts = upload_missing_segment.attempts + excluded.attempts,
            line_index = CASE WHEN excluded.status = 'failed' THEN excluded.line_index ELSE upload_missing_segment.line_index END,
            next_retry_at = CASE WHEN excluded.status = 'failed' THEN excluded.next_retry_at ELSE upload_missing_segment.next_retry_at END,
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

/// Return the first unused ordering hint for another missing segment in a local upload session.
pub async fn next_missing_segment_order(
    pool: &ConnectionPool,
    upload_session_id: i64,
    successful_count: usize,
) -> AppResult<i64> {
    let successful_count = i64::try_from(successful_count).unwrap_or(i64::MAX);
    let (max_queued, active_missing_count) = sqlx::query_as::<_, (Option<i64>, i64)>(
        "SELECT MAX(segment_order), \
                COUNT(CASE WHEN status IN ('pending', 'uploading', 'failed') THEN 1 END) \
         FROM upload_missing_segment WHERE upload_session_id = ?",
    )
    .bind(upload_session_id)
    .fetch_one(pool)
    .await
    .change_context(AppError::Unknown)?;
    Ok(max_queued
        .map(|order| order.saturating_add(1))
        .unwrap_or(0)
        .max(successful_count.saturating_add(active_missing_count)))
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
    use ormlite::Model;

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
            normalized_file_path: None,
            lifecycle_version: 1,
            video_json: None,
            total_bytes: None,
            uploaded_bytes: 0,
            current_line: None,
            upload_started_at: None,
            last_progress_at: None,
            attempt_token: None,
            attempt_phase: None,
            phase_started_at: None,
            last_heartbeat_at: None,
            line_source: None,
            last_chunk_index: None,
            last_chunk_started_at: None,
            last_chunk_error: None,
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

    async fn insert_upload_session(pool: &ConnectionPool, id: i64) {
        let now = chrono::Utc.with_ymd_and_hms(2026, 6, 18, 12, 0, 0).unwrap();
        sqlx::query(
            "INSERT INTO upload_session \
             (id, live_streamer_id, streamer_info_id, aid, bvid, videos_json, status, created_at, updated_at) \
             VALUES (?1, 10, 20, NULL, NULL, '[]', 'uploading', ?2, ?2)",
        )
        .bind(id)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn stale_uploading_lease_waits_five_minutes_and_converges_once() {
        let (_dir, pool) = test_pool().await;
        let started = chrono::Utc.with_ymd_and_hms(2026, 6, 18, 12, 0, 0).unwrap();
        let id = insert_missing_row_with_status(&pool, "uploading", "/tmp/stale.flv").await;
        sqlx::query(
            "UPDATE upload_missing_segment \
             SET lifecycle_version = 2, attempts = 2, line_index = 1, attempt_token = 'lease-a', \
                 upload_started_at = ?1, last_progress_at = ?1, updated_at = ?1 WHERE id = ?2",
        )
        .bind(started)
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(
            recover_stale_upload_attempts(
                &pool,
                started + chrono::Duration::minutes(4) + chrono::Duration::seconds(59)
            )
            .await
            .unwrap(),
            0
        );
        assert_eq!(
            recover_stale_upload_attempts(&pool, started + chrono::Duration::minutes(5))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            recover_stale_upload_attempts(&pool, started + chrono::Duration::minutes(6))
                .await
                .unwrap(),
            0,
            "the already-converged lease must not increment twice"
        );

        let state = sqlx::query_as::<_, (String, i64, i64, Option<String>, String)>(
            "SELECT status, attempts, line_index, attempt_token, last_error \
             FROM upload_missing_segment WHERE id = ?",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(state.0, "failed");
        assert_eq!((state.1, state.2), (3, 2));
        assert_eq!(state.3, None);
        assert!(
            state.4.starts_with("stale_uploading_lease"),
            "the reason must name the lease, got {:?}",
            state.4
        );
    }

    #[tokio::test]
    async fn health_reports_status_counts_and_only_the_stale_uploading_row() {
        let (_dir, pool) = test_pool().await;
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 6, 18, 12, 10, 0)
            .unwrap();

        let pending = insert_missing_row_with_status(&pool, "pending", "/tmp/pending.flv").await;
        let fresh_uploading =
            insert_missing_row_with_status(&pool, "uploading", "/tmp/fresh.flv").await;
        let stale_uploading =
            insert_missing_row_with_status(&pool, "uploading", "/tmp/stale.flv").await;
        for (id, progress_age) in [
            (fresh_uploading, chrono::Duration::seconds(30)),
            (stale_uploading, chrono::Duration::minutes(9)),
        ] {
            sqlx::query(
                "UPDATE upload_missing_segment \
                 SET lifecycle_version = 2, last_progress_at = ?1 WHERE id = ?2",
            )
            .bind(now - progress_age)
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::query("UPDATE upload_missing_segment SET lifecycle_version = 2 WHERE id = ?")
            .bind(pending)
            .execute(&pool)
            .await
            .unwrap();

        let health = missing_segment_health(&pool, now).await.unwrap();
        assert_eq!(health.status_counts.get("pending"), Some(&1));
        assert_eq!(health.status_counts.get("uploading"), Some(&2));
        assert_eq!(
            health.stale_uploading_count, 1,
            "only the row idle past STALE_ATTEMPT_AFTER counts as stale"
        );
        assert_eq!(
            health.oldest_stale_uploading_secs,
            Some(chrono::Duration::minutes(9).num_seconds())
        );
    }

    #[tokio::test]
    async fn pending_segment_is_immediately_due_without_counting_an_upload_attempt() {
        let (_dir, pool) = test_pool().await;
        insert_upload_session(&pool, 30).await;
        let now = chrono::Utc.with_ymd_and_hms(2026, 6, 18, 12, 5, 0).unwrap();

        enqueue_pending_segment(
            &pool,
            10,
            20,
            Some(30),
            None,
            Path::new("/tmp/init-failed.flv"),
            Some(Path::new("/tmp/init-failed.xml")),
            3,
            "login unavailable".to_string(),
            now,
        )
        .await
        .unwrap();

        let row = UploadMissingSegment::select()
            .where_("file_path = ?")
            .bind("/tmp/init-failed.flv")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.status, "pending");
        assert_eq!(row.attempts, 0);
        assert_eq!(row.line_index, 0);
        assert_eq!(row.next_retry_at, now);
        assert_eq!(row.upload_session_id, Some(30));

        let due = due_missing_segments_for_session(&pool, 30, now)
            .await
            .unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(next_missing_segment_order(&pool, 30, 1).await.unwrap(), 4);
    }

    #[tokio::test]
    async fn duplicate_pending_enqueue_keeps_retry_state_and_links_existing_row() {
        let (_dir, pool) = test_pool().await;
        insert_upload_session(&pool, 30).await;
        let first = chrono::Utc.with_ymd_and_hms(2026, 6, 18, 12, 5, 0).unwrap();
        let second = first + chrono::Duration::minutes(1);
        let path = Path::new("/tmp/replayed.flv");

        enqueue_pending_segment(
            &pool,
            10,
            20,
            None,
            None,
            path,
            None,
            0,
            "first".to_string(),
            first,
        )
        .await
        .unwrap();
        enqueue_pending_segment(
            &pool,
            10,
            20,
            Some(30),
            None,
            path,
            None,
            1,
            "second".to_string(),
            second,
        )
        .await
        .unwrap();

        let rows = UploadMissingSegment::select()
            .where_("file_path = ?")
            .bind(path.display().to_string())
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "pending");
        assert_eq!(rows[0].attempts, 0);
        assert_eq!(rows[0].next_retry_at, first);
        assert_eq!(rows[0].upload_session_id, Some(30));
        assert_eq!(rows[0].segment_order, 1);
    }

    #[tokio::test]
    async fn next_order_counts_missing_segment_between_successful_segments() {
        let (_dir, pool) = test_pool().await;
        insert_upload_session(&pool, 30).await;
        let now = chrono::Utc.with_ymd_and_hms(2026, 6, 18, 12, 5, 0).unwrap();
        enqueue_missing_segment(
            &pool,
            10,
            20,
            Some(30),
            None,
            Path::new("/tmp/middle-missing.flv"),
            None,
            1,
            "upload failed".to_string(),
            now,
        )
        .await
        .unwrap();

        // Original order: success(0), missing(1), success(2). `videos.len()` alone is only 2,
        // but the next segment must be assigned order 3 rather than collide with success(2).
        assert_eq!(next_missing_segment_order(&pool, 30, 2).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn rebuilt_pipeline_starts_after_init_failure_pending_segment() {
        let (_dir, pool) = test_pool().await;
        insert_upload_session(&pool, 30).await;
        let now = chrono::Utc.with_ymd_and_hms(2026, 6, 18, 12, 5, 0).unwrap();
        enqueue_pending_segment(
            &pool,
            10,
            20,
            Some(30),
            None,
            Path::new("/tmp/init-failure-first.flv"),
            None,
            0,
            "login unavailable".to_string(),
            now,
        )
        .await
        .unwrap();

        assert_eq!(
            next_missing_segment_order(&pool, 30, 0).await.unwrap(),
            1,
            "the rebuilt pipeline must not reuse order 0 for its next failed upload"
        );
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
        assert_eq!(next_line_index(2), 0);
        assert_eq!(next_line_index(3), 1);
        assert_eq!(next_line_index(7), 2);
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
