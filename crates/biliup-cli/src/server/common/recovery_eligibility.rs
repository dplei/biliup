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
    let event_uid = uuid::Uuid::new_v4();
    sqlx::query(
        "INSERT INTO upload_recovery_audit \
         (live_streamer_id, streamer_info_id, file_path, reason, created_at, event_uid) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(live_streamer_id)
    .bind(streamer_info_id)
    .bind(file_path.display().to_string())
    .bind(reason)
    .bind(now)
    .bind(event_uid.to_string())
    .execute(pool)
    .await
    .change_context(AppError::Unknown)?;
    // The insert above is the reliability boundary.  Projection is deliberately best effort and
    // is retried from the durable row by the server importer.
    crate::observe::audit::operation_projected(
        event_uid,
        now.timestamp_millis(),
        live_streamer_id,
        streamer_info_id,
        file_path,
        reason,
    );
    Ok(())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AuditReplay {
    pub last_id: i64,
    pub rows: usize,
    pub accepted: usize,
}

/// Replay a bounded page of durable audit rows in ascending order.  Legacy rows receive their UID
/// in the business database before projection; later retries therefore submit the same event.
pub async fn replay_recovery_audits(
    pool: &ConnectionPool,
    after_id: i64,
    limit: usize,
) -> AppResult<AuditReplay> {
    let limit = limit.clamp(1, 256) as i64;
    let rows = sqlx::query_as::<_, (i64, i64, i64, String, String, DateTime<Utc>, Option<String>)>(
        "SELECT id, live_streamer_id, streamer_info_id, file_path, reason, created_at, event_uid \
         FROM upload_recovery_audit WHERE id > ?1 ORDER BY id LIMIT ?2",
    )
    .bind(after_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .change_context(AppError::Unknown)?;

    let mut replay = AuditReplay {
        last_id: after_id,
        rows: rows.len(),
        accepted: 0,
    };
    for (id, live_id, session_id, file_path, reason, created_at, stored_uid) in rows {
        replay.last_id = id;
        let uid = match stored_uid.and_then(|value| uuid::Uuid::parse_str(&value).ok()) {
            Some(uid) => uid,
            None => {
                let candidate = uuid::Uuid::new_v4();
                let updated = sqlx::query(
                    "UPDATE upload_recovery_audit SET event_uid = ?1 \
                     WHERE id = ?2 AND event_uid IS NULL",
                )
                .bind(candidate.to_string())
                .bind(id)
                .execute(pool)
                .await
                .change_context(AppError::Unknown)?;
                if updated.rows_affected() == 1 {
                    candidate
                } else {
                    let value: String = sqlx::query_scalar(
                        "SELECT event_uid FROM upload_recovery_audit WHERE id = ?1",
                    )
                    .bind(id)
                    .fetch_one(pool)
                    .await
                    .change_context(AppError::Unknown)?;
                    uuid::Uuid::parse_str(&value).change_context(AppError::Unknown)?
                }
            }
        };
        replay.accepted += usize::from(crate::observe::audit::operation_projected(
            uid,
            created_at.timestamp_millis(),
            live_id,
            session_id,
            Path::new(&file_path),
            &reason,
        ));
    }
    Ok(replay)
}

#[cfg(test)]
mod audit_projection_tests {
    use super::*;
    use crate::server::infrastructure::connection_pool::test_support::migrated_pool;
    use biliup_observability::{
        CaptureLayer, Options, Runtime,
        sqlite::{Query, Repository, SqliteStore, StoreOptions},
    };
    use std::time::Duration;
    use tracing_subscriber::prelude::*;

    #[tokio::test]
    async fn durable_audit_replay_keeps_one_event_per_business_uid() {
        let (_business_dir, pool) = migrated_pool().await;
        let event_dir = tempfile::tempdir().unwrap();
        let event_path = event_dir.path().join("events.sqlite");
        let options = StoreOptions::new(&event_path);
        let mut runtime = Runtime::start(
            "audit-test",
            "test",
            Options {
                enabled: true,
                ..Options::default()
            },
            move || SqliteStore::open(options.clone()),
        )
        .unwrap();
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::registry().with(CaptureLayer::new(runtime.emitter()).filtered()),
        );

        let now = Utc::now();
        record_recovery_audit(
            &pool,
            7,
            9,
            Path::new("/private/source.flv"),
            "source_missing_before_enrollment",
            now,
        )
        .await
        .unwrap();
        // Simulate a row created by migration 14 before stable projection UIDs existed.
        sqlx::query(
            "INSERT INTO upload_recovery_audit \
             (live_streamer_id, streamer_info_id, file_path, reason, created_at) \
             VALUES (7, 9, '/private/late.flv', \
                     'late_validated_segment_for_finalized_session', ?1)",
        )
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(replay_recovery_audits(&pool, 0, 32).await.unwrap().rows, 2);
        assert_eq!(replay_recovery_audits(&pool, 0, 32).await.unwrap().rows, 2);
        assert!(runtime.shutdown(Duration::from_secs(2)).closed);

        let business_rows: Vec<(String, String)> =
            sqlx::query_as("SELECT event_uid, reason FROM upload_recovery_audit ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(business_rows.len(), 2);
        assert!(
            business_rows
                .iter()
                .all(|(uid, _)| uuid::Uuid::parse_str(uid).is_ok())
        );
        assert_ne!(business_rows[0].0, business_rows[1].0);

        let repository = Repository::open(&event_path).await.unwrap();
        let page = repository
            .query(&Query {
                event_name: Some("audit.operation_projected".into()),
                limit: 10,
                ..Query::default()
            })
            .await
            .unwrap();
        assert_eq!(
            page.events.len(),
            2,
            "stable event_uid must de-duplicate replay"
        );
        assert!(page.events.iter().all(|event| {
            event
                .data
                .fields
                .get("original_file")
                .and_then(|v| v.as_str())
                .is_some_and(|file| !file.contains('/'))
        }));
        repository.close().await;
    }
}
