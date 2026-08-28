//! Database-driven reconciliation for closed upload sessions.
//!
//! Event wakeups keep the happy path fast, but they cannot be the source of truth: the process
//! may exit after persisting `submit_requested_at` and before spawning the coordinator. This
//! scanner closes that gap on startup and periodically afterwards. The durable submit claim in
//! `upload_session` remains the only cross-process side-effect lock.

use crate::server::common::upload::{
    SessionSubmissionOutcome, SubmissionTrigger, reconcile_session_submission,
};
use crate::server::config::Config;
use crate::server::errors::{AppError, AppResult};
use crate::server::infrastructure::connection_pool::ConnectionPool;
use chrono::{DateTime, Utc};
use error_stack::ResultExt;
use serde::Serialize;
use std::sync::{Arc, RwLock};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{error, info, warn};

const SCAN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
const BLOCKED_RECHECK_INTERVAL: chrono::Duration = chrono::Duration::minutes(10);
const MAX_CONCURRENT_SUBMISSIONS: usize = 2;
const MAX_SCAN_CANDIDATES: i64 = 128;

/// Sessions safe for automatic reconciliation at `now`.
///
/// `ok_no_aid` and a claim-less `submitting` state are treated as ambiguous even though their
/// normal paths retain a claim. The defensive exclusion keeps a damaged row from creating a
/// duplicate remote submission.
pub async fn due_submission_session_ids(
    pool: &ConnectionPool,
    now: DateTime<Utc>,
    include_recent_blocked: bool,
) -> AppResult<Vec<i64>> {
    sqlx::query_scalar(
        "SELECT id FROM upload_session \
         WHERE status != 'finalized' \
           AND submit_requested_at IS NOT NULL \
           AND submit_claim_token IS NULL \
           AND (next_submit_at IS NULL OR next_submit_at <= ?1) \
           AND COALESCE(submit_state, '') NOT IN ('ok_no_aid', 'submitting') \
           AND (?2 OR COALESCE(submit_state, '') != 'blocked_missing_segments' OR updated_at <= ?3) \
         ORDER BY COALESCE(next_submit_at, submit_requested_at) ASC, id ASC \
         LIMIT ?4",
    )
    .bind(now)
    .bind(include_recent_blocked)
    .bind(now - BLOCKED_RECHECK_INTERVAL)
    .bind(MAX_SCAN_CANDIDATES)
    .fetch_all(pool)
    .await
    .change_context(AppError::Unknown)
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct SubmissionScanSummary {
    pub candidates: usize,
    pub blocked: Vec<i64>,
    pub submitted: Vec<i64>,
    pub retry_scheduled: Vec<i64>,
    pub claimed_elsewhere: Vec<i64>,
    pub manual_inspection: Vec<i64>,
    pub skipped: Vec<i64>,
    pub failed: Vec<i64>,
}

impl SubmissionScanSummary {
    fn record(&mut self, session_id: i64, outcome: SessionSubmissionOutcome) {
        match outcome {
            SessionSubmissionOutcome::Blocked { .. } => self.blocked.push(session_id),
            SessionSubmissionOutcome::Submitted { .. } => self.submitted.push(session_id),
            SessionSubmissionOutcome::RetryScheduled { .. } => {
                self.retry_scheduled.push(session_id)
            }
            SessionSubmissionOutcome::ClaimedElsewhere => self.claimed_elsewhere.push(session_id),
            SessionSubmissionOutcome::ManualInspectionRequired { .. } => {
                self.manual_inspection.push(session_id)
            }
            SessionSubmissionOutcome::NotRequested
            | SessionSubmissionOutcome::NotDue { .. }
            | SessionSubmissionOutcome::Finalized => self.skipped.push(session_id),
        }
    }
}

/// Reconcile one due batch with bounded network concurrency.
///
/// The caller awaits the batch, which ensures periodic scans never pile up. Other event wakeups
/// may still overlap; the database submit claim resolves those races.
pub async fn scan_due_submissions(
    config: &Config,
    pool: &ConnectionPool,
    now: DateTime<Utc>,
    trigger: SubmissionTrigger,
) -> AppResult<SubmissionScanSummary> {
    let session_ids =
        due_submission_session_ids(pool, now, trigger == SubmissionTrigger::StartupScan).await?;
    let mut summary = SubmissionScanSummary {
        candidates: session_ids.len(),
        ..Default::default()
    };
    if session_ids.is_empty() {
        return Ok(summary);
    }

    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_SUBMISSIONS));
    let mut tasks = JoinSet::new();
    for session_id in session_ids {
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("submission scan semaphore is never closed");
        let config = config.clone();
        let pool = pool.clone();
        tasks.spawn(async move {
            let _permit = permit;
            let outcome = reconcile_session_submission(&config, &pool, session_id, trigger).await;
            (session_id, outcome)
        });
    }

    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok((session_id, Ok(outcome))) => summary.record(session_id, outcome),
            Ok((session_id, Err(report))) => {
                error!(session = session_id, ?report, "待投稿会话协调失败");
                summary.failed.push(session_id);
            }
            Err(join_error) => error!(?join_error, "待投稿扫描任务异常退出"),
        }
    }
    Ok(summary)
}

/// Start with an immediate reconciliation pass, then keep scanning at a fixed interval.
pub fn start_submission_reconciliation_scan(config: Arc<RwLock<Config>>, pool: ConnectionPool) {
    tokio::spawn(async move {
        let mut trigger = SubmissionTrigger::StartupScan;
        loop {
            // Keep the non-Send std lock guard in this block and drop it before any await.
            let snapshot = {
                match config.read() {
                    Ok(config) => Some(config.clone()),
                    Err(_) => None,
                }
            };
            let Some(snapshot) = snapshot else {
                error!("读取配置失败，跳过本轮待投稿扫描");
                tokio::time::sleep(SCAN_INTERVAL).await;
                trigger = SubmissionTrigger::PeriodicScan;
                continue;
            };
            match scan_due_submissions(&snapshot, &pool, Utc::now(), trigger).await {
                Ok(summary) if summary.candidates > 0 => info!(
                    trigger = ?trigger,
                    candidates = summary.candidates,
                    blocked = ?summary.blocked,
                    submitted = ?summary.submitted,
                    retry_scheduled = ?summary.retry_scheduled,
                    claimed_elsewhere = ?summary.claimed_elsewhere,
                    manual_inspection = ?summary.manual_inspection,
                    skipped = ?summary.skipped,
                    failed = ?summary.failed,
                    "待投稿会话扫描完成"
                ),
                Ok(_) => {}
                Err(report) => warn!(?report, ?trigger, "待投稿会话扫描失败"),
            }
            trigger = SubmissionTrigger::PeriodicScan;
            tokio::time::sleep(SCAN_INTERVAL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::infrastructure::connection_pool::ConnectionManager;
    use chrono::TimeZone;

    async fn test_pool() -> (tempfile::TempDir, ConnectionPool) {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("submission-scan.db");
        let pool = ConnectionManager::new_pool(database.to_str().unwrap())
            .await
            .unwrap();
        (directory, pool)
    }

    async fn insert_session(
        pool: &ConnectionPool,
        id: i64,
        status: &str,
        requested_at: Option<DateTime<Utc>>,
        next_at: Option<DateTime<Utc>>,
        submit_state: Option<&str>,
        claim: Option<&str>,
    ) {
        let now = Utc.with_ymd_and_hms(2026, 8, 28, 12, 0, 0).unwrap();
        sqlx::query(
            "INSERT INTO upload_session \
             (id, live_streamer_id, streamer_info_id, videos_json, status, created_at, updated_at, \
              submit_requested_at, next_submit_at, submit_state, submit_claim_token) \
             VALUES (?1, 1, 1, '[]', ?2, ?3, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(id)
        .bind(status)
        .bind(now)
        .bind(requested_at)
        .bind(next_at)
        .bind(submit_state)
        .bind(claim)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn fake_clock_selects_only_safe_due_sessions() {
        let (_directory, pool) = test_pool().await;
        let now = Utc.with_ymd_and_hms(2026, 8, 28, 12, 0, 0).unwrap();
        insert_session(&pool, 1, "uploading", Some(now), None, None, None).await;
        insert_session(
            &pool,
            2,
            "uploading",
            Some(now),
            Some(now + chrono::Duration::seconds(1)),
            Some("failed"),
            None,
        )
        .await;
        insert_session(&pool, 3, "uploading", None, None, None, None).await;
        insert_session(&pool, 4, "finalized", Some(now), None, None, None).await;
        insert_session(
            &pool,
            5,
            "uploading",
            Some(now),
            None,
            Some("submitting"),
            Some("held"),
        )
        .await;
        insert_session(
            &pool,
            6,
            "uploading",
            Some(now),
            None,
            Some("ok_no_aid"),
            None,
        )
        .await;
        insert_session(
            &pool,
            7,
            "uploading",
            Some(now),
            None,
            Some("submitting"),
            None,
        )
        .await;
        insert_session(
            &pool,
            8,
            "uploading",
            Some(now),
            None,
            Some("blocked_missing_segments"),
            None,
        )
        .await;

        assert_eq!(
            due_submission_session_ids(&pool, now, false).await.unwrap(),
            vec![1]
        );
        assert_eq!(
            due_submission_session_ids(&pool, now, true).await.unwrap(),
            vec![1, 8],
            "startup rechecks recent blocked sessions, while periodic scans throttle them"
        );
        assert_eq!(
            due_submission_session_ids(&pool, now + chrono::Duration::seconds(1), false)
                .await
                .unwrap(),
            vec![1, 2]
        );
    }

    #[tokio::test]
    async fn startup_scan_reconciles_a_missed_blocked_event_without_network() {
        let (_directory, pool) = test_pool().await;
        let now = Utc.with_ymd_and_hms(2026, 8, 28, 12, 0, 0).unwrap();
        sqlx::query(
            "INSERT INTO livestreamers (id, url, remark) VALUES (1, 'test://scan', 'scan')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO streamerinfo (id, name, url, title, date, live_cover_path) \
             VALUES (1, 'scan', 'test://scan', 'scan', ?1, '')",
        )
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        insert_session(&pool, 10, "uploading", Some(now), None, None, None).await;
        sqlx::query(
            "INSERT INTO upload_missing_segment \
             (id, live_streamer_id, streamer_info_id, upload_session_id, file_path, \
              segment_order, status, next_retry_at, created_at, updated_at, lifecycle_version) \
             VALUES (100, 1, 1, 10, '/pending.flv', 0, 'pending', ?1, ?1, ?1, 2)",
        )
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();

        let summary = scan_due_submissions(
            &Config::default(),
            &pool,
            now,
            SubmissionTrigger::StartupScan,
        )
        .await
        .unwrap();

        assert_eq!(summary.candidates, 1);
        assert_eq!(summary.blocked, vec![10]);
        let state: Option<String> =
            sqlx::query_scalar("SELECT submit_state FROM upload_session WHERE id = 10")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state.as_deref(), Some("blocked_missing_segments"));
    }
}
