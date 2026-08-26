//! Shared, side-effect-free recovery admission checks.
//!
//! Keeping this decision separate from claiming an upload attempt makes every caller report the
//! same reason, and lets the caller turn a vanished source into the terminal `source_missing`
//! state before it can be selected again by the silent recovery loop.

use crate::server::common::util::MediaValidation;
use crate::server::errors::{AppError, AppResult};
use crate::server::infrastructure::connection_pool::ConnectionPool;
use crate::server::infrastructure::models::UploadMissingSegment;
use chrono::{DateTime, Utc};
use error_stack::ResultExt;
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryEligibility {
    Eligible,
    AlreadySucceeded,
    AlreadyRunning,
    SourceMissing,
    FinalizedRejected,
    LegacyFinalizedEdit,
    InvalidMedia,
    Conflict,
}

/// Check a persisted recovery row without taking a lease or mutating its state.
///
/// `media_validation` is supplied by callers which have just validated a candidate. Existing
/// recovery rows have already passed validation, so they pass `None` and only re-check that the
/// source is still a regular file.
pub async fn check_recovery_eligibility(
    pool: &ConnectionPool,
    row: &UploadMissingSegment,
    media_validation: Option<&MediaValidation>,
    now: DateTime<Utc>,
) -> AppResult<RecoveryEligibility> {
    if row.status == "succeeded" {
        return Ok(RecoveryEligibility::AlreadySucceeded);
    }

    if let Some(session_id) = row.upload_session_id {
        let finalized =
            sqlx::query_scalar::<_, String>("SELECT status FROM upload_session WHERE id = ?")
                .bind(session_id)
                .fetch_optional(pool)
                .await
                .change_context(AppError::Unknown)?
                .is_some_and(|status| status == "finalized");
        if finalized {
            return Ok(
                if row.lifecycle_version < 2
                    && (row.aid.is_some() || row.upload_session_id.is_some())
                {
                    RecoveryEligibility::LegacyFinalizedEdit
                } else {
                    RecoveryEligibility::FinalizedRejected
                },
            );
        }
    } else {
        let active_exists = sqlx::query_scalar::<_, i64>(
            "SELECT id FROM upload_session WHERE live_streamer_id = ?1 AND streamer_info_id = ?2 \
             AND status != 'finalized' LIMIT 1",
        )
        .bind(row.live_streamer_id)
        .bind(row.streamer_info_id)
        .fetch_optional(pool)
        .await
        .change_context(AppError::Unknown)?;
        if active_exists.is_none()
            && finalized_session_for_streamer_info(pool, row.live_streamer_id, row.streamer_info_id)
                .await?
                .is_some()
        {
            return Ok(RecoveryEligibility::FinalizedRejected);
        }
    }

    if let Some(normalized_path) = row.normalized_file_path.as_deref() {
        let duplicate_succeeded = sqlx::query_scalar::<_, i64>(
            "SELECT id FROM upload_missing_segment \
             WHERE live_streamer_id = ?1 AND normalized_file_path = ?2 \
               AND lifecycle_version = 2 AND status = 'succeeded' AND id != ?3 LIMIT 1",
        )
        .bind(row.live_streamer_id)
        .bind(normalized_path)
        .bind(row.id)
        .fetch_optional(pool)
        .await
        .change_context(AppError::Unknown)?;
        if duplicate_succeeded.is_some() {
            return Ok(RecoveryEligibility::Conflict);
        }
    }

    if !Path::new(&row.file_path).is_file() {
        return Ok(RecoveryEligibility::SourceMissing);
    }
    if !matches!(media_validation, None | Some(MediaValidation::Valid)) {
        return Ok(RecoveryEligibility::InvalidMedia);
    }
    if row.status == "uploading" && row.attempt_token.is_some() {
        return Ok(RecoveryEligibility::AlreadyRunning);
    }
    if matches!(row.status.as_str(), "pending" | "failed") && row.next_retry_at <= now {
        return Ok(RecoveryEligibility::Eligible);
    }
    Ok(RecoveryEligibility::Conflict)
}

/// Persist the non-retryable missing-source terminal state. The `next_retry_at` column predates
/// this state and is NOT NULL, so it is retained only as an inert timestamp; due queries exclude
/// `source_missing` by status.
pub async fn mark_source_missing(
    pool: &ConnectionPool,
    missing_id: i64,
    reason: &str,
    now: DateTime<Utc>,
) -> AppResult<bool> {
    let updated = sqlx::query(
        "UPDATE upload_missing_segment \
         SET status = 'source_missing', last_error = ?1, next_retry_at = ?2, \
             attempt_token = NULL, current_line = NULL, updated_at = ?2 \
         WHERE id = ?3 AND status NOT IN ('succeeded', 'source_missing', 'deleting')",
    )
    .bind(reason)
    .bind(now)
    .bind(missing_id)
    .execute(pool)
    .await
    .change_context(AppError::Unknown)?;
    Ok(updated.rows_affected() == 1)
}

pub async fn finalized_session_for_streamer_info(
    pool: &ConnectionPool,
    live_streamer_id: i64,
    streamer_info_id: i64,
) -> AppResult<Option<i64>> {
    sqlx::query_scalar::<_, i64>(
        "SELECT id FROM upload_session WHERE live_streamer_id = ?1 AND streamer_info_id = ?2 \
         AND status = 'finalized' ORDER BY updated_at DESC, id DESC LIMIT 1",
    )
    .bind(live_streamer_id)
    .bind(streamer_info_id)
    .fetch_optional(pool)
    .await
    .change_context(AppError::Unknown)
}

pub async fn record_recovery_audit(
    pool: &ConnectionPool,
    live_streamer_id: i64,
    streamer_info_id: i64,
    file_path: &Path,
    reason: &str,
    now: DateTime<Utc>,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO upload_recovery_audit \
         (live_streamer_id, streamer_info_id, file_path, reason, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(live_streamer_id)
    .bind(streamer_info_id)
    .bind(file_path.display().to_string())
    .bind(reason)
    .bind(now)
    .execute(pool)
    .await
    .change_context(AppError::Unknown)?;
    Ok(())
}
