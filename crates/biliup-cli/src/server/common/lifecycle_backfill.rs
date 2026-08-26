//! Legacy (v1) → lifecycle (v2) backfill for `upload_missing_segment`.
//!
//! Sessions created before task 02 have no lifecycle ledger: their parts live only inside
//! `upload_session.videos_json`, and their missing rows are a transient retry queue which may
//! contain duplicates of the same source file. The strict completeness gate from task 05 reads the
//! ledger, so those sessions would be permanently blocked without a baseline.
//!
//! This backfill is deliberately conservative:
//!
//! - it never deletes a historical row (duplicates are detached and marked, not dropped);
//! - it never submits, and never opens a finalized session — those only produce audit events;
//! - it never guesses between two rows which each claim a *different* remote Video: that is a
//!   `conflict` status which keeps the session blocked until a human looks at it.
//!
//! Progress is journaled per session inside the same transaction as the data it describes, so an
//! interrupted run resumes without replaying committed work and without duplicating synthetic rows.

use crate::server::common::segment_enrollment::normalize_segment_path;
use crate::server::common::upload_session::filename_stem;
use crate::server::errors::{AppError, AppResult};
use crate::server::infrastructure::connection_pool::ConnectionPool;
use biliup::bilibili::Video;
use chrono::{DateTime, Utc};
use error_stack::ResultExt;
use sqlx::{Row, Sqlite, Transaction};
use std::collections::HashMap;
use std::path::Path;
use tracing::{info, warn};

pub const BACKFILL_NAME: &str = "upload_lifecycle_v2";
const SESSION_BATCH: i64 = 50;

/// A duplicate row keeps its history but leaves the session ledger, so it can no longer block the
/// completeness gate for a source another row already owns.
const MERGED_DUPLICATE: &str = "merged_duplicate";
/// Two rows claim different remote Videos for one source. Unknown to the gate, therefore blocking.
const CONFLICT: &str = "conflict";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackfillSummary {
    pub processed_sessions: i64,
    pub migrated_rows: i64,
    pub synthetic_rows: i64,
    pub conflict_rows: i64,
    pub completed: bool,
}

/// One historical `upload_missing_segment` row, as read before any classification.
#[derive(Debug, Clone)]
pub struct LegacyRow {
    pub id: i64,
    pub file_path: String,
    pub segment_order: i64,
    pub status: String,
    pub attempts: i64,
    pub aid: Option<i64>,
    pub last_error: Option<String>,
    pub video_json: Option<String>,
    pub updated_at: DateTime<Utc>,
}

impl LegacyRow {
    /// The identity two historical rows are merged on. Normalization can fail for a path which is
    /// no longer resolvable; the raw string is still a stable identity for that case.
    fn identity(&self) -> String {
        normalize_segment_path(Path::new(&self.file_path))
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| self.file_path.clone())
    }

    fn remote_filename(&self) -> Option<String> {
        let video: Video = serde_json::from_str(self.video_json.as_deref()?).ok()?;
        Some(video.filename)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedRow {
    pub id: i64,
    pub segment_order: i64,
    pub status: String,
    pub normalized_file_path: String,
    pub video_json: Option<String>,
    pub attempts: i64,
    pub aid: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntheticRow {
    pub segment_order: i64,
    pub file_path: String,
    pub video_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEvent {
    pub missing_segment_id: Option<i64>,
    pub kind: String,
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackfillPlan {
    pub keep: Vec<PlannedRow>,
    /// Historical duplicates: kept as rows, detached from the session ledger.
    pub detach: Vec<i64>,
    pub synthetic: Vec<SyntheticRow>,
    pub events: Vec<PlanEvent>,
}

impl BackfillPlan {
    pub fn conflict_count(&self) -> i64 {
        self.keep
            .iter()
            .filter(|row| row.status == CONFLICT)
            .count() as i64
    }
}

/// Decide what one legacy session should look like as a v2 ledger. Pure so the merge, ordering and
/// conflict rules can be exercised without a database.
///
/// `videos` is the authoritative remote part order: the archive is already published in that
/// sequence, so the rebuilt ledger must reproduce it rather than the local retry order.
pub fn plan_session(session_id: i64, videos: &[Video], rows: Vec<LegacyRow>) -> BackfillPlan {
    let mut plan = BackfillPlan::default();
    let mut groups: Vec<(String, Vec<LegacyRow>)> = Vec::new();
    for row in rows {
        let identity = row.identity();
        match groups.iter_mut().find(|(key, _)| key == &identity) {
            Some((_, bucket)) => bucket.push(row),
            None => groups.push((identity, vec![row])),
        }
    }

    let mut merged = Vec::new();
    for (identity, bucket) in groups {
        let (winner, mut events) = merge_group(&identity, bucket);
        plan.events.append(&mut events);
        for loser in &winner.detached {
            plan.detach.push(*loser);
        }
        merged.push((identity, winner));
    }

    // Match each published part back to the local row which produced it. Bilibili's `filename` is
    // the remote object name, so the local identity lives in `title` (the upload set it from the
    // file stem); fall back to the remote name for very old rows which never carried a title.
    let mut by_stem: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, (identity, _)) in merged.iter().enumerate() {
        by_stem
            .entry(filename_stem(Path::new(identity)))
            .or_default()
            .push(index);
    }

    let mut claimed = vec![false; merged.len()];
    let mut ordered: Vec<Result<usize, SyntheticRow>> = Vec::new();
    for (part_index, video) in videos.iter().enumerate() {
        let key = video
            .title
            .clone()
            .unwrap_or_else(|| video.filename.clone());
        let stem = filename_stem(Path::new(&key));
        let matched = by_stem
            .get(&stem)
            .and_then(|candidates| candidates.iter().find(|index| !claimed[**index]).copied());
        match matched {
            Some(index) => {
                claimed[index] = true;
                let video_json = serde_json::to_string(video).unwrap_or_default();
                let row = &mut merged[index].1;
                if row.status != CONFLICT {
                    if let Some(existing) = row.remote_filename.as_deref()
                        && existing != video.filename
                    {
                        // The row already claims a different published part. Never overwrite it.
                        row.status = CONFLICT.to_string();
                        plan.events.push(PlanEvent {
                            missing_segment_id: Some(row.id),
                            kind: CONFLICT.to_string(),
                            detail: format!(
                                "row claims remote {existing}, session part {part_index} is {}",
                                video.filename
                            ),
                        });
                        ordered.push(Ok(index));
                        continue;
                    }
                    row.status = "succeeded".to_string();
                    row.video_json = Some(video_json);
                }
                ordered.push(Ok(index));
            }
            None => ordered.push(Err(SyntheticRow {
                segment_order: 0,
                file_path: format!("legacy://session/{session_id}/part/{part_index}"),
                video_json: serde_json::to_string(video).unwrap_or_default(),
            })),
        }
    }

    // Anything the archive does not account for is still owed: keep it after the published parts,
    // in its original local order, so it stays recoverable.
    let mut unmatched: Vec<usize> = (0..merged.len()).filter(|index| !claimed[*index]).collect();
    unmatched.sort_by_key(|index| {
        let row = &merged[*index].1;
        (row.segment_order, row.id)
    });

    let mut order = 0_i64;
    for slot in ordered {
        match slot {
            Ok(index) => {
                let row = &merged[index].1;
                plan.keep.push(row.planned(order));
            }
            Err(mut synthetic) => {
                synthetic.segment_order = order;
                plan.events.push(PlanEvent {
                    missing_segment_id: None,
                    kind: "synthetic_baseline".to_string(),
                    detail: synthetic.file_path.clone(),
                });
                plan.synthetic.push(synthetic);
            }
        }
        order += 1;
    }
    for index in unmatched {
        plan.keep.push(merged[index].1.planned(order));
        order += 1;
    }
    plan
}

/// `None` means the snapshot exists but cannot be trusted. An absent or empty snapshot is a
/// legitimate "never submitted" session and parses as no published parts.
fn published_parts(videos_json: &str) -> Option<Vec<Video>> {
    let trimmed = videos_json.trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Some(Vec::new());
    }
    serde_json::from_str(trimmed).ok()
}

#[derive(Debug, Clone)]
struct MergedRow {
    id: i64,
    identity: String,
    segment_order: i64,
    status: String,
    video_json: Option<String>,
    remote_filename: Option<String>,
    attempts: i64,
    aid: Option<i64>,
    last_error: Option<String>,
    detached: Vec<i64>,
}

impl MergedRow {
    fn planned(&self, segment_order: i64) -> PlannedRow {
        PlannedRow {
            id: self.id,
            segment_order,
            status: self.status.clone(),
            normalized_file_path: self.identity.clone(),
            video_json: self.video_json.clone(),
            attempts: self.attempts,
            aid: self.aid,
            last_error: self.last_error.clone(),
        }
    }
}

/// Collapse every historical row for one source into a single lifecycle row: succeeded wins,
/// otherwise the most recently updated state; attempts, the newest error and any target binding
/// are merged so no diagnostic is lost.
fn merge_group(identity: &str, mut bucket: Vec<LegacyRow>) -> (MergedRow, Vec<PlanEvent>) {
    let mut events = Vec::new();
    bucket.sort_by(|left, right| {
        let succeeded = |row: &LegacyRow| i32::from(row.status == "succeeded");
        succeeded(right)
            .cmp(&succeeded(left))
            .then(right.updated_at.cmp(&left.updated_at))
            .then(right.id.cmp(&left.id))
    });
    let winner = bucket[0].clone();
    let mut merged = MergedRow {
        id: winner.id,
        identity: identity.to_string(),
        segment_order: winner.segment_order,
        status: winner.status.clone(),
        video_json: winner.video_json.clone(),
        remote_filename: winner.remote_filename(),
        attempts: winner.attempts,
        aid: winner.aid,
        last_error: winner.last_error.clone(),
        detached: Vec::new(),
    };
    for loser in bucket.iter().skip(1) {
        merged.attempts = merged.attempts.max(loser.attempts);
        merged.aid = merged.aid.or(loser.aid);
        if merged.last_error.is_none() {
            merged.last_error = loser.last_error.clone();
        }
        match (merged.remote_filename.as_deref(), loser.remote_filename()) {
            (Some(kept), Some(other)) if kept != other => {
                merged.status = CONFLICT.to_string();
                events.push(PlanEvent {
                    missing_segment_id: Some(merged.id),
                    kind: CONFLICT.to_string(),
                    detail: format!("row #{} claims remote {other}, kept {kept}", loser.id),
                });
            }
            (None, Some(other)) => {
                merged.remote_filename = Some(other);
                merged.video_json = loser.video_json.clone();
                merged.status = "succeeded".to_string();
            }
            _ => {}
        }
        merged.detached.push(loser.id);
        events.push(PlanEvent {
            missing_segment_id: Some(loser.id),
            kind: MERGED_DUPLICATE.to_string(),
            detail: format!("merged into #{} for {identity}", merged.id),
        });
    }
    (merged, events)
}

pub async fn run_lifecycle_backfill(
    pool: &ConnectionPool,
    dry_run: bool,
) -> AppResult<BackfillSummary> {
    ensure_journal(pool).await?;
    let mut summary = read_journal(pool).await?;
    if summary.completed {
        info!(?summary, "upload lifecycle backfill already completed");
        return Ok(summary);
    }
    loop {
        let batch = next_sessions(pool, cursor(pool).await?).await?;
        if batch.is_empty() {
            if !dry_run {
                sqlx::query(
                    "UPDATE upload_lifecycle_backfill SET state = 'completed', \
                     completed_at = ?1, updated_at = ?1 WHERE name = ?2",
                )
                .bind(Utc::now())
                .bind(BACKFILL_NAME)
                .execute(pool)
                .await
                .change_context(AppError::Unknown)?;
            }
            summary.completed = true;
            info!(?summary, dry_run, "upload lifecycle backfill finished");
            return Ok(summary);
        }
        for (session_id, status, videos_json) in batch {
            let counts = backfill_one_session(pool, session_id, &status, &videos_json, dry_run)
                .await
                .attach_with(|| format!("session #{session_id}"))?;
            summary.processed_sessions += 1;
            summary.migrated_rows += counts.0;
            summary.synthetic_rows += counts.1;
            summary.conflict_rows += counts.2;
            if dry_run {
                info!(
                    session_id,
                    migrated = counts.0,
                    synthetic = counts.1,
                    conflicts = counts.2,
                    "dry-run backfill plan"
                );
            }
        }
        if dry_run {
            // Nothing was committed, so the cursor never moves: report the first batch and stop
            // rather than loop forever over the same sessions.
            return Ok(summary);
        }
    }
}

async fn backfill_one_session(
    pool: &ConnectionPool,
    session_id: i64,
    status: &str,
    videos_json: &str,
    dry_run: bool,
) -> AppResult<(i64, i64, i64)> {
    let mut tx = pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .change_context(AppError::Unknown)?;
    let rows = legacy_rows(&mut tx, session_id).await?;
    let mut counts = (0_i64, 0_i64, 0_i64);

    if rows.is_empty() {
        record_event(&mut tx, session_id, None, "skipped", "no lifecycle rows").await?;
    } else if status == "finalized" {
        // A published archive is a closed boundary: describe it, never rewrite it.
        record_event(
            &mut tx,
            session_id,
            None,
            "skipped_finalized",
            &format!("{} legacy rows left untouched", rows.len()),
        )
        .await?;
    } else if rows.iter().all(|row| row.lifecycle_version == 2) {
        record_event(&mut tx, session_id, None, "skipped", "already lifecycle v2").await?;
    } else if let Some(videos) = published_parts(videos_json) {
        let plan = plan_session(
            session_id,
            &videos,
            rows.into_iter().map(|row| row.row).collect(),
        );
        counts = apply_plan(&mut tx, session_id, &plan).await?;
    } else {
        // Reading a corrupt snapshot as "nothing published yet" would mark every local row as
        // still owed and re-upload parts the archive may already contain. Leave the session on v1
        // so the gate keeps blocking it, and let an operator repair the snapshot.
        record_event(
            &mut tx,
            session_id,
            None,
            "corrupt_videos_json",
            "published part snapshot is unparseable; session left at lifecycle v1",
        )
        .await?;
    }

    advance_journal(&mut tx, session_id, counts).await?;
    if dry_run {
        tx.rollback().await.change_context(AppError::Unknown)?;
    } else {
        tx.commit().await.change_context(AppError::Unknown)?;
    }
    Ok(counts)
}

async fn apply_plan(
    tx: &mut Transaction<'_, Sqlite>,
    session_id: i64,
    plan: &BackfillPlan,
) -> AppResult<(i64, i64, i64)> {
    // `ux_upload_segment_v2_session_order` is unique, and rows are renumbered in place, so park
    // every order in a disjoint negative range before writing the final sequence.
    for (index, row) in plan.keep.iter().enumerate() {
        sqlx::query("UPDATE upload_missing_segment SET segment_order = ?1 WHERE id = ?2")
            .bind(-1 - index as i64)
            .bind(row.id)
            .execute(&mut **tx)
            .await
            .change_context(AppError::Unknown)?;
    }
    for id in &plan.detach {
        sqlx::query(
            "UPDATE upload_missing_segment SET upload_session_id = NULL, status = ?1, \
             updated_at = ?2 WHERE id = ?3",
        )
        .bind(MERGED_DUPLICATE)
        .bind(Utc::now())
        .bind(id)
        .execute(&mut **tx)
        .await
        .change_context(AppError::Unknown)?;
    }
    for row in &plan.keep {
        sqlx::query(
            "UPDATE upload_missing_segment \
             SET segment_order = ?1, status = ?2, normalized_file_path = ?3, video_json = ?4, \
                 attempts = ?5, aid = COALESCE(?6, aid), last_error = ?7, \
                 lifecycle_version = 2, updated_at = ?8 \
             WHERE id = ?9",
        )
        .bind(row.segment_order)
        .bind(&row.status)
        .bind(&row.normalized_file_path)
        .bind(row.video_json.as_deref())
        .bind(row.attempts)
        .bind(row.aid)
        .bind(row.last_error.as_deref())
        .bind(Utc::now())
        .bind(row.id)
        .execute(&mut **tx)
        .await
        .change_context(AppError::Unknown)?;
    }
    for synthetic in &plan.synthetic {
        insert_synthetic(tx, session_id, synthetic).await?;
    }
    for event in &plan.events {
        record_event(
            tx,
            session_id,
            event.missing_segment_id,
            &event.kind,
            &event.detail,
        )
        .await?;
    }
    Ok((
        plan.keep.len() as i64,
        plan.synthetic.len() as i64,
        plan.conflict_count(),
    ))
}

/// A synthetic row is a completeness baseline for a part whose local source can no longer be
/// identified. It is born succeeded and its `legacy://` path never resolves, so no recovery path
/// will try to re-upload it.
async fn insert_synthetic(
    tx: &mut Transaction<'_, Sqlite>,
    session_id: i64,
    synthetic: &SyntheticRow,
) -> AppResult<()> {
    let owner = sqlx::query(
        "SELECT live_streamer_id, streamer_info_id, aid FROM upload_session WHERE id = ?1",
    )
    .bind(session_id)
    .fetch_one(&mut **tx)
    .await
    .change_context(AppError::Unknown)?;
    let now = Utc::now();
    sqlx::query(
        "INSERT INTO upload_missing_segment \
         (live_streamer_id, streamer_info_id, upload_session_id, aid, file_path, \
          normalized_file_path, danmaku_file_path, segment_order, status, attempts, line_index, \
          next_retry_at, last_error, video_json, lifecycle_version, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?5, NULL, ?6, 'succeeded', 0, 0, ?7, NULL, ?8, 2, ?7, ?7) \
         ON CONFLICT DO NOTHING",
    )
    .bind(owner.get::<i64, _>("live_streamer_id"))
    .bind(owner.get::<i64, _>("streamer_info_id"))
    .bind(session_id)
    .bind(owner.get::<Option<i64>, _>("aid"))
    .bind(&synthetic.file_path)
    .bind(synthetic.segment_order)
    .bind(now)
    .bind(&synthetic.video_json)
    .execute(&mut **tx)
    .await
    .change_context(AppError::Unknown)?;
    Ok(())
}

struct VersionedRow {
    row: LegacyRow,
    lifecycle_version: i64,
}

async fn legacy_rows(
    tx: &mut Transaction<'_, Sqlite>,
    session_id: i64,
) -> AppResult<Vec<VersionedRow>> {
    let rows = sqlx::query(
        "SELECT id, file_path, segment_order, status, attempts, aid, last_error, video_json, \
                lifecycle_version, updated_at \
         FROM upload_missing_segment WHERE upload_session_id = ?1 ORDER BY segment_order, id",
    )
    .bind(session_id)
    .fetch_all(&mut **tx)
    .await
    .change_context(AppError::Unknown)?;
    Ok(rows
        .into_iter()
        .map(|row| VersionedRow {
            lifecycle_version: row.get("lifecycle_version"),
            row: LegacyRow {
                id: row.get("id"),
                file_path: row.get("file_path"),
                segment_order: row.get("segment_order"),
                status: row.get("status"),
                attempts: row.get("attempts"),
                aid: row.get("aid"),
                last_error: row.get("last_error"),
                video_json: row.get("video_json"),
                updated_at: row.get("updated_at"),
            },
        })
        .collect())
}

async fn ensure_journal(pool: &ConnectionPool) -> AppResult<()> {
    let now = Utc::now();
    sqlx::query(
        "INSERT INTO upload_lifecycle_backfill (name, state, started_at, updated_at) \
         VALUES (?1, 'running', ?2, ?2) ON CONFLICT (name) DO NOTHING",
    )
    .bind(BACKFILL_NAME)
    .bind(now)
    .execute(pool)
    .await
    .change_context(AppError::Unknown)?;
    Ok(())
}

async fn read_journal(pool: &ConnectionPool) -> AppResult<BackfillSummary> {
    let row = sqlx::query(
        "SELECT state, processed_sessions, migrated_rows, synthetic_rows, conflict_rows \
         FROM upload_lifecycle_backfill WHERE name = ?1",
    )
    .bind(BACKFILL_NAME)
    .fetch_one(pool)
    .await
    .change_context(AppError::Unknown)?;
    Ok(BackfillSummary {
        processed_sessions: row.get("processed_sessions"),
        migrated_rows: row.get("migrated_rows"),
        synthetic_rows: row.get("synthetic_rows"),
        conflict_rows: row.get("conflict_rows"),
        completed: row.get::<String, _>("state") == "completed",
    })
}

async fn cursor(pool: &ConnectionPool) -> AppResult<i64> {
    sqlx::query_scalar::<_, i64>(
        "SELECT last_session_id FROM upload_lifecycle_backfill WHERE name = ?1",
    )
    .bind(BACKFILL_NAME)
    .fetch_one(pool)
    .await
    .change_context(AppError::Unknown)
}

async fn next_sessions(pool: &ConnectionPool, after: i64) -> AppResult<Vec<(i64, String, String)>> {
    let rows = sqlx::query(
        "SELECT id, status, videos_json FROM upload_session WHERE id > ?1 ORDER BY id LIMIT ?2",
    )
    .bind(after)
    .bind(SESSION_BATCH)
    .fetch_all(pool)
    .await
    .change_context(AppError::Unknown)?;
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.get("id"),
                row.get("status"),
                row.get::<Option<String>, _>("videos_json")
                    .unwrap_or_default(),
            )
        })
        .collect())
}

/// The cursor moves in the same transaction as the rows it covers, so a crash resumes exactly
/// where it stopped and never replays a committed session.
async fn advance_journal(
    tx: &mut Transaction<'_, Sqlite>,
    session_id: i64,
    counts: (i64, i64, i64),
) -> AppResult<()> {
    sqlx::query(
        "UPDATE upload_lifecycle_backfill \
         SET last_session_id = ?1, processed_sessions = processed_sessions + 1, \
             migrated_rows = migrated_rows + ?2, synthetic_rows = synthetic_rows + ?3, \
             conflict_rows = conflict_rows + ?4, updated_at = ?5 \
         WHERE name = ?6 AND last_session_id < ?1",
    )
    .bind(session_id)
    .bind(counts.0)
    .bind(counts.1)
    .bind(counts.2)
    .bind(Utc::now())
    .bind(BACKFILL_NAME)
    .execute(&mut **tx)
    .await
    .change_context(AppError::Unknown)?;
    Ok(())
}

async fn record_event(
    tx: &mut Transaction<'_, Sqlite>,
    session_id: i64,
    missing_segment_id: Option<i64>,
    kind: &str,
    detail: &str,
) -> AppResult<()> {
    if kind == CONFLICT {
        warn!(
            session_id,
            ?missing_segment_id,
            detail,
            "backfill conflict blocks submission"
        );
    }
    sqlx::query(
        "INSERT INTO upload_lifecycle_backfill_event \
         (backfill_name, upload_session_id, missing_segment_id, kind, detail, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(BACKFILL_NAME)
    .bind(session_id)
    .bind(missing_segment_id)
    .bind(kind)
    .bind(detail)
    .bind(Utc::now())
    .execute(&mut **tx)
    .await
    .change_context(AppError::Unknown)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::common::upload_session::session_completeness;
    use crate::server::infrastructure::connection_pool::test_support::migrated_pool;

    fn video(title: &str, remote: &str) -> Video {
        Video {
            title: Some(title.to_string()),
            filename: remote.to_string(),
            desc: String::new(),
        }
    }

    fn legacy(id: i64, path: &str, order: i64, status: &str) -> LegacyRow {
        LegacyRow {
            id,
            file_path: path.to_string(),
            segment_order: order,
            status: status.to_string(),
            attempts: 0,
            aid: None,
            last_error: None,
            video_json: None,
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn published_parts_define_the_rebuilt_order() {
        let rows = vec![
            legacy(2, "/media/part-1.flv", 7, "failed"),
            legacy(1, "/media/part-0.flv", 3, "pending"),
        ];
        let plan = plan_session(
            9,
            &[video("part-0", "remote-0"), video("part-1", "remote-1")],
            rows,
        );
        assert_eq!(
            plan.keep
                .iter()
                .map(|row| (row.id, row.segment_order, row.status.as_str()))
                .collect::<Vec<_>>(),
            [(1, 0, "succeeded"), (2, 1, "succeeded")]
        );
        assert!(plan.synthetic.is_empty());
    }

    #[test]
    fn unmatched_parts_become_synthetic_and_unmatched_rows_keep_owing() {
        let rows = vec![legacy(1, "/media/still-owed.flv", 0, "pending")];
        let plan = plan_session(4, &[video("vanished-source", "remote-0")], rows);
        assert_eq!(
            plan.synthetic,
            [SyntheticRow {
                segment_order: 0,
                file_path: "legacy://session/4/part/0".to_string(),
                video_json: serde_json::to_string(&video("vanished-source", "remote-0")).unwrap(),
            }]
        );
        assert_eq!(plan.keep.len(), 1);
        assert_eq!(plan.keep[0].segment_order, 1);
        assert_eq!(plan.keep[0].status, "pending");
    }

    #[test]
    fn duplicate_rows_merge_into_the_succeeded_one_without_losing_diagnostics() {
        let mut succeeded = legacy(2, "/media/dup.flv", 1, "succeeded");
        succeeded.video_json = Some(serde_json::to_string(&video("dup", "remote-a")).unwrap());
        let mut failed = legacy(1, "/media/./dup.flv", 0, "failed");
        failed.attempts = 9;
        failed.last_error = Some("boom".to_string());
        failed.aid = Some(77);

        let plan = plan_session(1, &[], vec![failed, succeeded]);
        assert_eq!(plan.keep.len(), 1, "one identity keeps one lifecycle row");
        assert_eq!(plan.keep[0].id, 2);
        assert_eq!(plan.keep[0].attempts, 9, "attempts merge to the maximum");
        assert_eq!(plan.keep[0].aid, Some(77), "target binding survives");
        assert_eq!(plan.keep[0].last_error.as_deref(), Some("boom"));
        assert_eq!(plan.detach, [1]);
    }

    #[test]
    fn two_different_remote_claims_are_a_conflict_not_a_guess() {
        let mut first = legacy(1, "/media/dup.flv", 0, "succeeded");
        first.video_json = Some(serde_json::to_string(&video("dup", "remote-a")).unwrap());
        let mut second = legacy(2, "/media/dup.flv", 1, "succeeded");
        second.video_json = Some(serde_json::to_string(&video("dup", "remote-b")).unwrap());

        let plan = plan_session(1, &[], vec![first, second]);
        assert_eq!(plan.keep.len(), 1);
        assert_eq!(plan.keep[0].status, CONFLICT);
        assert_eq!(plan.conflict_count(), 1);
    }

    async fn seeded_pool() -> (tempfile::TempDir, ConnectionPool) {
        let (dir, pool) = migrated_pool().await;
        sqlx::query("INSERT INTO livestreamers (id, url, remark) VALUES (1, ?1, 'backfill')")
            .bind("https://example.invalid/live/backfill")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO streamerinfo (id, name, url, title, date, live_cover_path) \
             VALUES (2, 'backfill', ?1, 'legacy session', ?2, '')",
        )
        .bind("https://example.invalid/live/backfill")
        .bind(Utc::now())
        .execute(&pool)
        .await
        .unwrap();
        (dir, pool)
    }

    async fn legacy_session(pool: &ConnectionPool, status: &str, videos_json: &str) -> i64 {
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO upload_session \
             (live_streamer_id, streamer_info_id, aid, bvid, videos_json, status, created_at, updated_at) \
             VALUES (1, 2, NULL, NULL, ?1, ?2, ?3, ?3)",
        )
        .bind(videos_json)
        .bind(status)
        .bind(now)
        .execute(pool)
        .await
        .unwrap()
        .last_insert_rowid()
    }

    async fn legacy_missing(pool: &ConnectionPool, session_id: i64, path: &str, order: i64) {
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO upload_missing_segment \
             (live_streamer_id, streamer_info_id, upload_session_id, aid, file_path, \
              danmaku_file_path, segment_order, status, attempts, line_index, next_retry_at, \
              last_error, created_at, updated_at) \
             VALUES (1, 2, ?1, NULL, ?2, NULL, ?3, 'pending', 0, 0, ?4, NULL, ?4, ?4)",
        )
        .bind(session_id)
        .bind(path)
        .bind(order)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn backfilled_legacy_session_passes_the_completeness_gate() {
        let (_dir, pool) = seeded_pool().await;
        let videos =
            serde_json::to_string(&[video("part-0", "remote-0"), video("part-1", "remote-1")])
                .unwrap();
        let session = legacy_session(&pool, "uploading", &videos).await;
        legacy_missing(&pool, session, "/media/part-0.flv", 0).await;
        legacy_missing(&pool, session, "/media/part-1.flv", 1).await;

        let summary = run_lifecycle_backfill(&pool, false).await.unwrap();
        assert!(summary.completed);
        assert_eq!(summary.migrated_rows, 2);
        assert_eq!(summary.synthetic_rows, 0);

        let completeness = session_completeness(&pool, session).await.unwrap();
        assert!(
            completeness.is_complete(),
            "a rebuilt legacy ledger must stop blocking submission: {completeness:?}"
        );
    }

    #[tokio::test]
    async fn rerunning_and_resuming_never_duplicates_a_synthetic_baseline() {
        let (_dir, pool) = seeded_pool().await;
        let videos = serde_json::to_string(&[video("gone", "remote-0")]).unwrap();
        let session = legacy_session(&pool, "uploading", &videos).await;
        legacy_missing(&pool, session, "/media/still-owed.flv", 0).await;

        let first = run_lifecycle_backfill(&pool, false).await.unwrap();
        assert_eq!(first.synthetic_rows, 1);

        // A second run must be a no-op, and so must a run whose journal was rewound by a crash
        // before the cursor moved.
        assert_eq!(run_lifecycle_backfill(&pool, false).await.unwrap(), first);
        sqlx::query(
            "UPDATE upload_lifecycle_backfill SET state = 'running', last_session_id = 0 \
             WHERE name = ?1",
        )
        .bind(BACKFILL_NAME)
        .execute(&pool)
        .await
        .unwrap();
        run_lifecycle_backfill(&pool, false).await.unwrap();

        let synthetic = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM upload_missing_segment WHERE file_path LIKE 'legacy://%'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            synthetic, 1,
            "a resumed run must not re-create the baseline"
        );
    }

    #[tokio::test]
    async fn finalized_sessions_and_unusable_videos_json_are_only_audited() {
        let (_dir, pool) = seeded_pool().await;
        let finalized = legacy_session(&pool, "finalized", "[{\"broken\"").await;
        legacy_missing(&pool, finalized, "/media/published.flv", 0).await;
        let empty = legacy_session(&pool, "uploading", "").await;

        let summary = run_lifecycle_backfill(&pool, false).await.unwrap();
        assert!(summary.completed);
        assert_eq!(summary.migrated_rows, 0);
        assert_eq!(summary.synthetic_rows, 0);

        let untouched = sqlx::query_scalar::<_, i64>(
            "SELECT lifecycle_version FROM upload_missing_segment WHERE upload_session_id = ?1",
        )
        .bind(finalized)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(untouched, 1, "a published archive is never rewritten");
        let kinds = sqlx::query_scalar::<_, String>(
            "SELECT kind FROM upload_lifecycle_backfill_event ORDER BY upload_session_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(kinds, ["skipped_finalized", "skipped"]);
        assert_eq!(empty, finalized + 1);
    }

    #[tokio::test]
    async fn duplicate_legacy_rows_survive_migration_and_merge_on_backfill() {
        let (_dir, pool) = seeded_pool().await;
        let videos = serde_json::to_string(&[video("dup", "remote-0")]).unwrap();
        let session = legacy_session(&pool, "uploading", &videos).await;
        // The legacy schema already made file_path unique per streamer, so a historical duplicate
        // can only be the same file spelled differently — which is exactly why rows are merged on
        // the normalized path rather than on the stored string.
        legacy_missing(&pool, session, "/media/dup.flv", 0).await;
        legacy_missing(&pool, session, "/media/./dup.flv", 1).await;
        legacy_missing(&pool, session, "/media/sub/../dup.flv", 2).await;

        let summary = run_lifecycle_backfill(&pool, false).await.unwrap();
        assert_eq!(summary.migrated_rows, 1, "three rows, one source identity");
        assert_eq!(summary.conflict_rows, 0);

        let ledger = sqlx::query_scalar::<_, String>(
            "SELECT status FROM upload_missing_segment WHERE upload_session_id = ?1",
        )
        .bind(session)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(ledger, ["succeeded"]);
        let detached = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM upload_missing_segment \
             WHERE upload_session_id IS NULL AND status = ?1",
        )
        .bind(MERGED_DUPLICATE)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(detached, 2, "duplicates are detached, never deleted");
        assert!(
            session_completeness(&pool, session)
                .await
                .unwrap()
                .is_complete()
        );
    }

    #[tokio::test]
    async fn a_corrupt_published_snapshot_blocks_instead_of_re_uploading() {
        let (_dir, pool) = seeded_pool().await;
        let session = legacy_session(&pool, "uploading", "[{\"filename\":").await;
        legacy_missing(&pool, session, "/media/part-0.flv", 0).await;

        let summary = run_lifecycle_backfill(&pool, false).await.unwrap();
        assert_eq!(summary.processed_sessions, 1);
        assert_eq!(
            summary.migrated_rows, 0,
            "an unreadable snapshot must not be read as 'nothing published yet'"
        );

        let version = sqlx::query_scalar::<_, i64>(
            "SELECT lifecycle_version FROM upload_missing_segment WHERE upload_session_id = ?1",
        )
        .bind(session)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(version, 1);
        let kind = sqlx::query_scalar::<_, String>(
            "SELECT kind FROM upload_lifecycle_backfill_event WHERE upload_session_id = ?1",
        )
        .bind(session)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(kind, "corrupt_videos_json");
        assert!(
            !session_completeness(&pool, session)
                .await
                .unwrap()
                .is_complete()
        );
    }

    #[tokio::test]
    async fn dry_run_reports_the_plan_without_writing() {
        let (_dir, pool) = seeded_pool().await;
        let videos = serde_json::to_string(&[video("part-0", "remote-0")]).unwrap();
        let session = legacy_session(&pool, "uploading", &videos).await;
        legacy_missing(&pool, session, "/media/part-0.flv", 0).await;

        let summary = run_lifecycle_backfill(&pool, true).await.unwrap();
        assert_eq!(summary.migrated_rows, 1);
        assert!(!summary.completed);

        let version = sqlx::query_scalar::<_, i64>(
            "SELECT lifecycle_version FROM upload_missing_segment WHERE upload_session_id = ?1",
        )
        .bind(session)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(version, 1, "a dry run must leave the database untouched");
    }
}
