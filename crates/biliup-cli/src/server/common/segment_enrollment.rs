use crate::server::common::recovery_eligibility::{
    finalized_session_for_streamer_info, record_recovery_audit,
};
use crate::server::core::downloader::SegmentEnrollment;
use crate::server::errors::{AppError, AppResult};
use crate::server::infrastructure::connection_pool::ConnectionPool;
use chrono::{DateTime, Utc};
use error_stack::ResultExt;
use serde::{Deserialize, Serialize};
use sqlx::{Row, Sqlite, Transaction};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;
use tracing::{error, info, warn};

pub const DEFAULT_OUTBOX_DIRECTORY: &str = "data/upload-enrollment-outbox";
const ENROLLMENT_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(20),
    Duration::from_millis(50),
    Duration::from_millis(100),
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollmentRequest {
    pub live_streamer_id: i64,
    pub streamer_info_id: i64,
    pub file_path: PathBuf,
    pub normalized_file_path: PathBuf,
    pub danmaku_file_path: Option<PathBuf>,
    pub total_bytes: u64,
    pub now: DateTime<Utc>,
    pub recovery_window_minutes: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnrollmentOutcome {
    Enrolled(SegmentEnrollment),
    Outboxed(PathBuf),
    /// The segment arrived after this exact local stream had already been finalized. It is
    /// auditable, but deliberately has no lifecycle row to avoid reopening a submitted archive.
    FinalizedRejected {
        session_id: i64,
    },
    /// A segment vanished between validation and durable enrollment. No active retry row is
    /// created for a source which cannot be opened.
    SourceMissing,
}

#[derive(Clone)]
pub struct EnrollmentStore {
    pool: ConnectionPool,
    outbox_directory: PathBuf,
}

impl EnrollmentStore {
    pub fn new(pool: ConnectionPool, outbox_directory: PathBuf) -> Self {
        Self {
            pool,
            outbox_directory,
        }
    }

    pub fn production(pool: ConnectionPool) -> Self {
        Self::new(pool, PathBuf::from(DEFAULT_OUTBOX_DIRECTORY))
    }

    pub fn outbox_directory(&self) -> &Path {
        &self.outbox_directory
    }
}

pub fn normalize_segment_path(path: &Path) -> AppResult<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .change_context(AppError::Unknown)?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.file_name().is_some() {
                    normalized.pop();
                }
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    Ok(normalized)
}

pub async fn enroll_validated_segment(
    store: &EnrollmentStore,
    request: &EnrollmentRequest,
) -> AppResult<EnrollmentOutcome> {
    if !request.file_path.is_file() {
        audit_recovery_best_effort(&store.pool, request, "source_missing_before_enrollment").await;
        return Ok(EnrollmentOutcome::SourceMissing);
    }

    if let Some(session_id) = finalized_boundary_for(&store.pool, request).await {
        audit_recovery_best_effort(
            &store.pool,
            request,
            "late_validated_segment_for_finalized_session",
        )
        .await;
        return Ok(EnrollmentOutcome::FinalizedRejected { session_id });
    }

    for delay in ENROLLMENT_RETRY_DELAYS {
        match enroll_in_database(&store.pool, request).await {
            Ok(enrollment) => {
                log_enrolled(&enrollment);
                return Ok(EnrollmentOutcome::Enrolled(enrollment));
            }
            Err(error) => {
                warn!(?error, file = %request.normalized_file_path.display(), "segment enrollment transaction failed; retrying");
                tokio::time::sleep(delay).await;
            }
        }
    }
    match enroll_in_database(&store.pool, request).await {
        Ok(enrollment) => {
            log_enrolled(&enrollment);
            Ok(EnrollmentOutcome::Enrolled(enrollment))
        }
        Err(error) => {
            let manifest = write_outbox_manifest(&store.outbox_directory, request)?;
            error!(
                ?error,
                manifest = %manifest.display(),
                file = %request.normalized_file_path.display(),
                "database enrollment unavailable; fsynced enrollment outbox manifest"
            );
            Ok(EnrollmentOutcome::Outboxed(manifest))
        }
    }
}

/// The one log line every validated segment produces on its way to becoming durable: without it
/// there is no way to count validated/enrolled throughput from logs alone (outbox and rejection
/// paths already log; this was the missing success case).
fn log_enrolled(enrollment: &SegmentEnrollment) {
    info!(
        missing_id = enrollment.missing_id,
        upload_session_id = enrollment.upload_session_id,
        segment_order = enrollment.segment_order,
        duplicate = enrollment.duplicate,
        total_bytes = enrollment.total_bytes,
        "segment validated and enrolled"
    );
}

/// Reports the finalized session which closes this StreamerInfo, or `None` when a non-finalized
/// session exists (a new live run may legitimately reuse the room).
///
/// A database which cannot answer must never be read as "finalized": invariant 1 requires a
/// validated segment to reach the fsynced outbox rather than be dropped, and `import_outbox_once`
/// re-checks this same boundary once the database is reachable again.
async fn finalized_boundary_for(pool: &ConnectionPool, request: &EnrollmentRequest) -> Option<i64> {
    let active_exists = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM upload_session WHERE live_streamer_id = ?1 AND streamer_info_id = ?2 \
         AND status != 'finalized' LIMIT 1",
    )
    .bind(request.live_streamer_id)
    .bind(request.streamer_info_id)
    .fetch_optional(pool)
    .await;
    match active_exists {
        Ok(Some(_)) => return None,
        Ok(None) => {}
        Err(error) => {
            warn!(
                ?error,
                file = %request.normalized_file_path.display(),
                "cannot read session boundary; deferring the finalized guard to outbox import"
            );
            return None;
        }
    }
    match finalized_session_for_streamer_info(
        pool,
        request.live_streamer_id,
        request.streamer_info_id,
    )
    .await
    {
        Ok(session_id) => session_id,
        Err(error) => {
            warn!(
                ?error,
                file = %request.normalized_file_path.display(),
                "cannot read finalized sessions; deferring the finalized guard to outbox import"
            );
            None
        }
    }
}

/// The audit row explains a rejection; losing it must not turn a decided outcome into an error.
async fn audit_recovery_best_effort(
    pool: &ConnectionPool,
    request: &EnrollmentRequest,
    reason: &str,
) {
    if let Err(error) = record_recovery_audit(
        pool,
        request.live_streamer_id,
        request.streamer_info_id,
        &request.file_path,
        reason,
        request.now,
    )
    .await
    {
        warn!(
            ?error,
            reason,
            file = %request.normalized_file_path.display(),
            "upload recovery audit row not recorded"
        );
    }
}

async fn enroll_in_database(
    pool: &ConnectionPool,
    request: &EnrollmentRequest,
) -> Result<SegmentEnrollment, sqlx::Error> {
    let mut tx = pool.begin().await?;
    if let Some(existing) = existing_enrollment(&mut tx, request).await? {
        ensure_filelist(&mut tx, request).await?;
        tx.commit().await?;
        return Ok(existing);
    }

    let (session_id, created_session) = find_or_create_session(&mut tx, request).await?;
    let baseline_count =
        sqlx::query_scalar::<_, String>("SELECT videos_json FROM upload_session WHERE id = ?")
            .bind(session_id)
            .fetch_one(&mut *tx)
            .await
            .ok()
            .and_then(|json| serde_json::from_str::<Vec<serde_json::Value>>(&json).ok())
            .and_then(|videos| i64::try_from(videos.len()).ok())
            .unwrap_or(0);
    let max_order = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT MAX(segment_order) FROM upload_missing_segment \
         WHERE upload_session_id = ? AND lifecycle_version = 2",
    )
    .bind(session_id)
    .fetch_one(&mut *tx)
    .await?;
    let segment_order = max_order
        .map(|order| order.saturating_add(1))
        .unwrap_or(0)
        .max(baseline_count);
    let total_bytes = i64::try_from(request.total_bytes).unwrap_or(i64::MAX);
    let result = sqlx::query(
        "INSERT INTO upload_missing_segment \
         (live_streamer_id, streamer_info_id, upload_session_id, aid, file_path, \
          danmaku_file_path, segment_order, status, attempts, line_index, next_retry_at, \
          last_error, created_at, updated_at, normalized_file_path, lifecycle_version, \
          video_json, total_bytes, uploaded_bytes, current_line, upload_started_at, \
          last_progress_at, attempt_token) \
         VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, 'pending', 0, 0, ?7, \
                 'validated segment accepted; awaiting upload', ?7, ?7, ?8, 2, \
                 NULL, ?9, 0, NULL, NULL, NULL, NULL)",
    )
    .bind(request.live_streamer_id)
    .bind(request.streamer_info_id)
    .bind(session_id)
    .bind(request.file_path.display().to_string())
    .bind(
        request
            .danmaku_file_path
            .as_ref()
            .map(|path| path.display().to_string()),
    )
    .bind(segment_order)
    .bind(request.now)
    .bind(request.normalized_file_path.display().to_string())
    .bind(total_bytes)
    .execute(&mut *tx)
    .await;

    let missing_id = match result {
        Ok(result) => result.last_insert_rowid(),
        Err(error) if is_constraint_error(&error) => {
            tx.rollback().await?;
            let mut retry = pool.begin().await?;
            let existing = existing_enrollment(&mut retry, request)
                .await?
                .ok_or(error)?;
            ensure_filelist(&mut retry, request).await?;
            retry.commit().await?;
            return Ok(existing);
        }
        Err(error) => return Err(error),
    };
    ensure_filelist(&mut tx, request).await?;
    tx.commit().await?;
    Ok(SegmentEnrollment {
        missing_id,
        upload_session_id: session_id,
        created_session,
        segment_order,
        normalized_file_path: request.normalized_file_path.clone(),
        total_bytes: request.total_bytes,
        duplicate: false,
    })
}

async fn existing_enrollment(
    tx: &mut Transaction<'_, Sqlite>,
    request: &EnrollmentRequest,
) -> Result<Option<SegmentEnrollment>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, upload_session_id, segment_order, total_bytes \
         FROM upload_missing_segment \
         WHERE live_streamer_id = ? AND normalized_file_path = ? AND lifecycle_version = 2",
    )
    .bind(request.live_streamer_id)
    .bind(request.normalized_file_path.display().to_string())
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(|row| SegmentEnrollment {
        missing_id: row.get("id"),
        upload_session_id: row.get::<i64, _>("upload_session_id"),
        created_session: false,
        segment_order: row.get("segment_order"),
        normalized_file_path: request.normalized_file_path.clone(),
        total_bytes: row
            .get::<Option<i64>, _>("total_bytes")
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or(request.total_bytes),
        duplicate: true,
    }))
}

/// The live-session key of the `streamerinfo` row this enrollment belongs to.
///
/// Read inside the enrollment transaction rather than passed in, so every caller (recording,
/// rescan, outbox import) gets the same identity without having to plumb it.
async fn live_session_key_of(
    tx: &mut Transaction<'_, Sqlite>,
    streamer_info_id: i64,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT live_session_key FROM streamerinfo WHERE id = ?",
    )
    .bind(streamer_info_id)
    .fetch_optional(&mut **tx)
    .await
    .map(Option::flatten)
}

/// A session that is closed for submit cannot accept more segments; the caller defers instead.
fn reject_if_claimed(claim_token: Option<String>) -> Result<(), sqlx::Error> {
    if claim_token.is_some() {
        return Err(sqlx::Error::Protocol(
            "upload session is closed for submit; enrollment deferred".to_string(),
        ));
    }
    Ok(())
}

/// Find the session this segment belongs to, or create it.
///
/// Three matches, in decreasing confidence:
///
/// 1. the exact `streamerinfo` row this segment was recorded under;
/// 2. the same **live session key** — the platform's own id for this broadcast. This is what
///    survives a restart: `monitor` reuses the `streamerinfo` row for a known key, but if it had
///    to create a new one, the key still ties the two together;
/// 3. a clock window, kept only as the fallback for platforms that give us no key.
///
/// The window used to be the only continuation path, and it was measured from `updated_at`, which
/// nothing touched during recording or uploading. A single hour-long segment therefore aged its
/// own live session out of the window — which is how one live stream ended up split across two
/// sessions and two archives.
async fn find_or_create_session(
    tx: &mut Transaction<'_, Sqlite>,
    request: &EnrollmentRequest,
) -> Result<(i64, bool), sqlx::Error> {
    if let Some((id, claim_token, updated_at)) =
        sqlx::query_as::<_, (i64, Option<String>, DateTime<Utc>)>(
            "SELECT id, submit_claim_token, updated_at FROM upload_session \
             WHERE live_streamer_id = ?1 AND streamer_info_id = ?2 AND status != 'finalized' \
             ORDER BY updated_at DESC, id DESC LIMIT 1",
        )
        .bind(request.live_streamer_id)
        .bind(request.streamer_info_id)
        .fetch_optional(&mut **tx)
        .await?
    {
        reject_if_claimed(claim_token)?;
        // Enrolling a segment proves the session is alive, even when nothing else writes to it.
        // Only issued when the timestamp is actually stale: in SQLite even a zero-row UPDATE
        // takes a write lock, and this is the recorder's hottest transaction.
        if request.now - updated_at >= chrono::Duration::seconds(60) {
            sqlx::query("UPDATE upload_session SET updated_at = ?1 WHERE id = ?2")
                .bind(request.now)
                .bind(id)
                .execute(&mut **tx)
                .await?;
        }
        return Ok((id, false));
    }
    let session_key = live_session_key_of(tx, request.streamer_info_id).await?;
    if let Some(session_key) = session_key.as_deref()
        && let Some((id, claim_token)) = sqlx::query_as::<_, (i64, Option<String>)>(
            "SELECT id, submit_claim_token FROM upload_session \
             WHERE live_streamer_id = ?1 AND live_session_key = ?2 AND status != 'finalized' \
             ORDER BY updated_at DESC, id DESC LIMIT 1",
        )
        .bind(request.live_streamer_id)
        .bind(session_key)
        .fetch_optional(&mut **tx)
        .await?
    {
        reject_if_claimed(claim_token)?;
        sqlx::query(
            "UPDATE upload_session SET streamer_info_id = ?1, updated_at = ?2 WHERE id = ?3",
        )
        .bind(request.streamer_info_id)
        .bind(request.now)
        .bind(id)
        .execute(&mut **tx)
        .await?;
        return Ok((id, false));
    }
    let cutoff = request.now - chrono::Duration::minutes(request.recovery_window_minutes);
    if let Some((id, claim_token, existing_key)) =
        sqlx::query_as::<_, (i64, Option<String>, Option<String>)>(
            "SELECT id, submit_claim_token, live_session_key FROM upload_session \
             WHERE live_streamer_id = ?1 AND status != 'finalized' AND updated_at >= ?2 \
             ORDER BY updated_at DESC, id DESC LIMIT 1",
        )
        .bind(request.live_streamer_id)
        .bind(cutoff)
        .fetch_optional(&mut **tx)
        .await?
    {
        // Two different keys mean two different broadcasts. Merging them because they happen to
        // be close in time is the one failure worse than splitting one broadcast in two, so the
        // window is only allowed to match when at least one side has no key to contradict it.
        let keys_conflict = matches!(
            (session_key.as_deref(), existing_key.as_deref()),
            (Some(incoming), Some(existing)) if incoming != existing
        );
        if !keys_conflict {
            reject_if_claimed(claim_token)?;
            sqlx::query(
                "UPDATE upload_session \
                 SET streamer_info_id = ?1, updated_at = ?2, \
                     live_session_key = COALESCE(live_session_key, ?3) \
                 WHERE id = ?4",
            )
            .bind(request.streamer_info_id)
            .bind(request.now)
            .bind(session_key.as_deref())
            .bind(id)
            .execute(&mut **tx)
            .await?;
            return Ok((id, false));
        }
    }
    // Keep the finalized boundary inside the enrollment transaction as well. The public
    // preflight above gives callers a useful outcome; this closes its check/insert race.
    if sqlx::query_scalar::<_, i64>(
        "SELECT id FROM upload_session WHERE live_streamer_id = ?1 AND streamer_info_id = ?2 \
         AND status = 'finalized' LIMIT 1",
    )
    .bind(request.live_streamer_id)
    .bind(request.streamer_info_id)
    .fetch_optional(&mut **tx)
    .await?
    .is_some()
    {
        return Err(sqlx::Error::Protocol(
            "upload session is finalized; enrollment rejected".to_string(),
        ));
    }
    let result = sqlx::query(
        "INSERT INTO upload_session \
         (live_streamer_id, streamer_info_id, aid, bvid, videos_json, status, created_at, updated_at, \
          submit_attempts, last_submit_at, last_submit_error, submit_state, live_session_key) \
         VALUES (?1, ?2, NULL, NULL, '[]', 'uploading', ?3, ?3, 0, NULL, NULL, NULL, ?4)",
    )
    .bind(request.live_streamer_id)
    .bind(request.streamer_info_id)
    .bind(request.now)
    .bind(session_key)
    .execute(&mut **tx)
    .await?;
    Ok((result.last_insert_rowid(), true))
}

async fn ensure_filelist(
    tx: &mut Transaction<'_, Sqlite>,
    request: &EnrollmentRequest,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO filelist (file, streamer_info_id) \
         SELECT ?1, ?2 WHERE NOT EXISTS \
         (SELECT 1 FROM filelist WHERE file = ?1 AND streamer_info_id = ?2)",
    )
    .bind(request.file_path.display().to_string())
    .bind(request.streamer_info_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn is_constraint_error(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(database) if database.is_unique_violation())
}

fn write_outbox_manifest(directory: &Path, request: &EnrollmentRequest) -> AppResult<PathBuf> {
    std::fs::create_dir_all(directory).change_context(AppError::Unknown)?;
    let nonce: u128 = rand::random();
    let name = format!(
        "enrollment-{}-{nonce:032x}.json",
        request.now.timestamp_millis()
    );
    let final_path = directory.join(name);
    let temp_path = final_path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(request).change_context(AppError::Unknown)?;
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp_path)
        .change_context(AppError::Unknown)?;
    use std::io::Write;
    file.write_all(&bytes).change_context(AppError::Unknown)?;
    file.sync_all().change_context(AppError::Unknown)?;
    std::fs::rename(&temp_path, &final_path).change_context(AppError::Unknown)?;
    if let Ok(directory_file) = std::fs::File::open(directory) {
        directory_file
            .sync_all()
            .change_context(AppError::Unknown)?;
    }
    Ok(final_path)
}

pub async fn import_outbox_once(store: &EnrollmentStore) -> AppResult<usize> {
    let entries = match std::fs::read_dir(&store.outbox_directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error).change_context(AppError::Unknown),
    };
    let mut imported = 0;
    for entry in entries {
        let path = entry.change_context(AppError::Unknown)?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let bytes = std::fs::read(&path).change_context(AppError::Unknown)?;
        let request: EnrollmentRequest =
            serde_json::from_slice(&bytes).change_context(AppError::Unknown)?;
        // The manifest was written while the database was unreachable, so the finalized guard in
        // enroll_validated_segment could not run. Re-check it here before the row is created.
        if let Some(session_id) = finalized_boundary_for(&store.pool, &request).await {
            audit_recovery_best_effort(
                &store.pool,
                &request,
                "late_outbox_manifest_for_finalized_session",
            )
            .await;
            std::fs::remove_file(&path).change_context(AppError::Unknown)?;
            warn!(
                session_id,
                manifest = %path.display(),
                "discarded outbox manifest for an already finalized session"
            );
            continue;
        }
        match enroll_in_database(&store.pool, &request).await {
            Ok(enrollment) => {
                std::fs::remove_file(&path).change_context(AppError::Unknown)?;
                imported += 1;
                info!(missing_id = enrollment.missing_id, manifest = %path.display(), "imported upload enrollment outbox manifest");
            }
            Err(error) => {
                warn!(?error, manifest = %path.display(), "upload enrollment outbox import deferred")
            }
        }
    }
    Ok(imported)
}

pub fn spawn_outbox_importer(pool: ConnectionPool) {
    let store = EnrollmentStore::production(pool);
    tokio::spawn(async move {
        loop {
            if let Err(error) = import_outbox_once(&store).await {
                error!(?error, "upload enrollment outbox scan failed");
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    });
}

pub fn outbox_health(directory: &Path) -> serde_json::Value {
    let mut count = 0_u64;
    let mut oldest = None;
    if let Ok(entries) = std::fs::read_dir(directory) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            count += 1;
            if let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified()) {
                let modified: DateTime<Utc> = modified.into();
                oldest =
                    Some(oldest.map_or(modified, |current: DateTime<Utc>| current.min(modified)));
            }
        }
    }
    serde_json::json!({ "count": count, "oldest_created_at": oldest })
}
