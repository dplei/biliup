//! Attempt lease phases, heartbeats and the stale-lease verdict.
//!
//! A claimed attempt does *not* start pushing bytes immediately. It first normalizes audio and
//! repairs timestamps locally, then waits behind the single global upload permit, and only then
//! opens a network transfer. Judging all three by one "no network progress for five minutes"
//! rule is what turned a healthy 3.32 GB normalization into a reaped lease, a ghost upload whose
//! result was thrown away at `persist_segment`, and a self-sustaining retry loop.
//!
//! So each phase gets its own deadline, and every phase writes a liveness heartbeat:
//!
//! | phase           | reaped when                                                       |
//! | --------------- | ----------------------------------------------------------------- |
//! | `preprocessing` | heartbeat lost, or size-derived hard cap exceeded                 |
//! | `queued`        | heartbeat lost, or two hours behind the global permit             |
//! | `transferring`  | five minutes without an acknowledged network chunk                |
//!
//! The heartbeat is what tells a stale *process* apart from slow *work*: only the process that
//! owns the lease writes it, so a crashed process stops it within `HEARTBEAT_STALE_AFTER` while
//! a busy ffmpeg keeps it fresh right up to the hard cap.

use crate::server::common::upload_line_health::sanitize_error;
use crate::server::errors::{AppError, AppResult};
use crate::server::infrastructure::connection_pool::ConnectionPool;
use chrono::{DateTime, Duration, Utc};
use error_stack::ResultExt;

/// Where an attempt currently is between claim and remote acknowledgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptPhase {
    /// Local ffmpeg work: audio normalization and timestamp repair.
    Preprocessing,
    /// Waiting for the process-wide upload permit (capacity 1).
    Queued,
    /// Actually pushing chunks to the upload line.
    Transferring,
}

impl AttemptPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preprocessing => "preprocessing",
            Self::Queued => "queued",
            Self::Transferring => "transferring",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "preprocessing" => Some(Self::Preprocessing),
            "queued" => Some(Self::Queued),
            "transferring" => Some(Self::Transferring),
            _ => None,
        }
    }
}

/// How often the owning task refreshes `last_heartbeat_at`.
pub const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
/// A lease whose heartbeat is older than this has no live owner in any process.
pub const HEARTBEAT_STALE_AFTER: Duration = Duration::minutes(3);
/// Transferring is the only phase where a silent network is itself the failure.
pub const NO_PROGRESS_TIMEOUT: Duration = Duration::minutes(5);
/// Queueing behind the global permit is normal for a long time — a 3 GB segment ahead of you
/// legitimately takes an hour — so this is aligned with the total upload timeout, not shortened.
pub const QUEUE_TIMEOUT: Duration = Duration::hours(2);
/// Preprocessing hard cap: ten minutes of fixed cost plus ten minutes per gigabyte.
pub const PREPROCESS_BASE: Duration = Duration::minutes(10);
pub const PREPROCESS_PER_GIB: Duration = Duration::minutes(10);
const BYTES_PER_GIB: i64 = 1024 * 1024 * 1024;

/// Upper bound for local preprocessing of a `total_bytes`-sized source.
///
/// Measured throughput during the incident was roughly 4 MB/s (1.2 GB in five minutes), so ten
/// minutes per gigabyte leaves about a threefold margin. Unknown sizes get the largest single
/// step rather than the base, because an unknown size is usually an old row, not a small file.
pub fn preprocess_deadline(total_bytes: Option<i64>) -> Duration {
    let gigabytes = match total_bytes {
        Some(bytes) if bytes > 0 => bytes.div_euclid(BYTES_PER_GIB) + 1,
        _ => 1,
    };
    PREPROCESS_BASE + PREPROCESS_PER_GIB * (gigabytes.min(64) as i32)
}

/// Why a lease is considered abandoned. The variant decides both the `last_error` text and
/// whether the upload line gets blamed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleReason {
    /// The owning process stopped writing heartbeats: it crashed, or was killed mid-attempt.
    HeartbeatLost,
    PreprocessTimeout,
    QueueTimeout,
    NoNetworkProgress,
}

impl StaleReason {
    pub fn as_error(self) -> &'static str {
        match self {
            Self::HeartbeatLost => {
                "stale_uploading_lease: attempt owner stopped heartbeating (process gone)"
            }
            Self::PreprocessTimeout => {
                "preprocess_timeout: local normalization/repair exceeded its size-derived limit"
            }
            Self::QueueTimeout => {
                "queue_timeout: attempt waited for the global upload permit past its limit"
            }
            Self::NoNetworkProgress => {
                "stale_uploading_lease: no acknowledged upload chunk within the transfer deadline"
            }
        }
    }

    /// Only a stalled *network transfer* is evidence about the upload line. Blaming the line for
    /// slow local ffmpeg is what cooled `bda2` down to the one-hour tier during the incident.
    pub fn blames_upload_line(self) -> bool {
        matches!(self, Self::NoNetworkProgress)
    }
}

/// The lease fields the verdict needs. Kept separate from the ORM row so the decision stays a
/// pure function that unit tests can drive without a database.
#[derive(Debug, Clone, Copy)]
pub struct LeaseSnapshot {
    pub phase: Option<AttemptPhase>,
    pub phase_started_at: Option<DateTime<Utc>>,
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    pub last_progress_at: Option<DateTime<Utc>>,
    pub upload_started_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub total_bytes: Option<i64>,
}

/// Decide whether an `uploading` row has been abandoned, and why.
///
/// Rows written before phases existed (and rows claimed by an older binary) carry no phase and no
/// heartbeat; they keep exactly the old behaviour — the transfer deadline against whatever
/// timestamp is available — so a rolling upgrade cannot leave leases unreapable.
pub fn classify_stale_lease(lease: &LeaseSnapshot, now: DateTime<Utc>) -> Option<StaleReason> {
    let phase = match lease.phase {
        Some(phase) => phase,
        None => {
            let since = lease
                .last_progress_at
                .or(lease.upload_started_at)
                .unwrap_or(lease.updated_at);
            return (now - since >= NO_PROGRESS_TIMEOUT).then_some(StaleReason::NoNetworkProgress);
        }
    };
    // A heartbeat is only evidence of death once one was ever written; a row mid-upgrade may
    // legitimately have a phase but no heartbeat yet, and must fall through to its phase deadline.
    if let Some(heartbeat) = lease.last_heartbeat_at
        && now - heartbeat >= HEARTBEAT_STALE_AFTER
    {
        return Some(StaleReason::HeartbeatLost);
    }
    let phase_since = lease
        .phase_started_at
        .or(lease.upload_started_at)
        .unwrap_or(lease.updated_at);
    match phase {
        AttemptPhase::Preprocessing => (now - phase_since
            >= preprocess_deadline(lease.total_bytes))
        .then_some(StaleReason::PreprocessTimeout),
        AttemptPhase::Queued => {
            (now - phase_since >= QUEUE_TIMEOUT).then_some(StaleReason::QueueTimeout)
        }
        AttemptPhase::Transferring => {
            let since = lease.last_progress_at.unwrap_or(phase_since);
            (now - since >= NO_PROGRESS_TIMEOUT).then_some(StaleReason::NoNetworkProgress)
        }
    }
}

/// Move a still-owned lease into `phase`, resetting its phase deadline and heartbeat.
///
/// Returns false when the CAS on `attempt_token` fails, which is the caller's signal that the
/// lease was revoked underneath it and the work in flight is now a ghost.
pub async fn record_phase(
    pool: &ConnectionPool,
    missing_id: i64,
    attempt_token: &str,
    phase: AttemptPhase,
    now: DateTime<Utc>,
) -> AppResult<bool> {
    let updated = sqlx::query(
        "UPDATE upload_missing_segment \
         SET attempt_phase = ?1, phase_started_at = ?2, last_heartbeat_at = ?2, updated_at = ?2 \
         WHERE id = ?3 AND lifecycle_version = 2 AND status = 'uploading' AND attempt_token = ?4",
    )
    .bind(phase.as_str())
    .bind(now)
    .bind(missing_id)
    .bind(attempt_token)
    .execute(pool)
    .await
    .change_context(AppError::Unknown)?;
    Ok(updated.rows_affected() == 1)
}

/// Renew the lease without changing its phase deadline.
pub async fn record_heartbeat(
    pool: &ConnectionPool,
    missing_id: i64,
    attempt_token: &str,
    now: DateTime<Utc>,
) -> AppResult<bool> {
    let updated = sqlx::query(
        "UPDATE upload_missing_segment SET last_heartbeat_at = ?1 \
         WHERE id = ?2 AND lifecycle_version = 2 AND status = 'uploading' AND attempt_token = ?3",
    )
    .bind(now)
    .bind(missing_id)
    .bind(attempt_token)
    .execute(pool)
    .await
    .change_context(AppError::Unknown)?;
    Ok(updated.rows_affected() == 1)
}

/// How many attempt-history rows are kept per lifecycle row. Enough to read a line-switch story
/// off the page; bounded so a segment that retries for days cannot grow the table without limit.
pub const ATTEMPT_HISTORY_PER_SEGMENT: i64 = 20;

/// Open the history row for an attempt. Idempotent: a retried claim with the same token keeps its
/// original `started_at`.
pub async fn open_attempt_history(
    pool: &ConnectionPool,
    missing_id: i64,
    attempt_token: &str,
    line_key: &str,
    line_source: &str,
    now: DateTime<Utc>,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO upload_attempt \
             (missing_id, attempt_token, line_key, line_source, started_at, phase_reached) \
         VALUES (?1, ?2, ?3, ?4, ?5, 'preprocessing') \
         ON CONFLICT(missing_id, attempt_token) DO UPDATE SET \
             line_key = excluded.line_key, line_source = excluded.line_source",
    )
    .bind(missing_id)
    .bind(attempt_token)
    .bind(line_key)
    .bind(line_source)
    .bind(now)
    .execute(pool)
    .await
    .change_context(AppError::Unknown)?;
    prune_attempt_history(pool, missing_id).await
}

/// Record the furthest phase an attempt reached, so a failed attempt still shows whether it ever
/// got to the network.
pub async fn note_attempt_phase(
    pool: &ConnectionPool,
    missing_id: i64,
    attempt_token: &str,
    phase: AttemptPhase,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE upload_attempt SET phase_reached = ?1 \
         WHERE missing_id = ?2 AND attempt_token = ?3",
    )
    .bind(phase.as_str())
    .bind(missing_id)
    .bind(attempt_token)
    .execute(pool)
    .await
    .change_context(AppError::Unknown)?;
    Ok(())
}

/// Close the history row. `error` goes through the same redaction as `last_error`.
#[allow(clippy::too_many_arguments)]
pub async fn close_attempt_history(
    pool: &ConnectionPool,
    missing_id: i64,
    attempt_token: &str,
    outcome: &str,
    uploaded_bytes: i64,
    last_chunk_index: Option<i64>,
    error: Option<&str>,
    now: DateTime<Utc>,
) -> AppResult<()> {
    let error = error.map(sanitize_error);
    sqlx::query(
        "UPDATE upload_attempt \
         SET ended_at = ?1, outcome = ?2, uploaded_bytes = ?3, last_chunk_index = ?4, error = ?5 \
         WHERE missing_id = ?6 AND attempt_token = ?7 AND ended_at IS NULL",
    )
    .bind(now)
    .bind(outcome)
    .bind(uploaded_bytes)
    .bind(last_chunk_index)
    .bind(error)
    .bind(missing_id)
    .bind(attempt_token)
    .execute(pool)
    .await
    .change_context(AppError::Unknown)?;
    Ok(())
}

async fn prune_attempt_history(pool: &ConnectionPool, missing_id: i64) -> AppResult<()> {
    sqlx::query(
        "DELETE FROM upload_attempt WHERE missing_id = ?1 AND id NOT IN \
         (SELECT id FROM upload_attempt WHERE missing_id = ?1 ORDER BY id DESC LIMIT ?2)",
    )
    .bind(missing_id)
    .bind(ATTEMPT_HISTORY_PER_SEGMENT)
    .execute(pool)
    .await
    .change_context(AppError::Unknown)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn base(now: DateTime<Utc>) -> LeaseSnapshot {
        LeaseSnapshot {
            phase: None,
            phase_started_at: None,
            last_heartbeat_at: None,
            last_progress_at: None,
            upload_started_at: None,
            updated_at: now,
            total_bytes: None,
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 27, 12, 0, 0).unwrap()
    }

    #[test]
    fn preprocessing_survives_far_beyond_the_transfer_deadline() {
        let now = now();
        let lease = LeaseSnapshot {
            phase: Some(AttemptPhase::Preprocessing),
            phase_started_at: Some(now - Duration::minutes(20)),
            last_heartbeat_at: Some(now - Duration::seconds(10)),
            total_bytes: Some(3 * BYTES_PER_GIB + 300 * 1024 * 1024),
            ..base(now)
        };

        assert_eq!(classify_stale_lease(&lease, now), None);
    }

    #[test]
    fn preprocessing_is_reaped_once_its_size_derived_cap_is_passed() {
        let now = now();
        // 3.32 GB → 10 + 4 * 10 = 50 minutes.
        let lease = LeaseSnapshot {
            phase: Some(AttemptPhase::Preprocessing),
            phase_started_at: Some(now - Duration::minutes(51)),
            last_heartbeat_at: Some(now - Duration::seconds(10)),
            total_bytes: Some(3 * BYTES_PER_GIB + 300 * 1024 * 1024),
            ..base(now)
        };

        assert_eq!(
            classify_stale_lease(&lease, now),
            Some(StaleReason::PreprocessTimeout)
        );
        assert!(!StaleReason::PreprocessTimeout.blames_upload_line());
    }

    #[test]
    fn queueing_behind_the_global_permit_is_not_a_line_failure() {
        let now = now();
        let waiting = LeaseSnapshot {
            phase: Some(AttemptPhase::Queued),
            phase_started_at: Some(now - Duration::minutes(75)),
            last_heartbeat_at: Some(now - Duration::seconds(5)),
            ..base(now)
        };
        assert_eq!(classify_stale_lease(&waiting, now), None);

        let expired = LeaseSnapshot {
            phase_started_at: Some(now - Duration::hours(2) - Duration::minutes(1)),
            ..waiting
        };
        assert_eq!(
            classify_stale_lease(&expired, now),
            Some(StaleReason::QueueTimeout)
        );
        assert!(!StaleReason::QueueTimeout.blames_upload_line());
    }

    #[test]
    fn transferring_keeps_the_five_minute_network_rule() {
        let now = now();
        let lease = LeaseSnapshot {
            phase: Some(AttemptPhase::Transferring),
            phase_started_at: Some(now - Duration::minutes(30)),
            last_heartbeat_at: Some(now - Duration::seconds(5)),
            last_progress_at: Some(now - Duration::minutes(6)),
            ..base(now)
        };

        assert_eq!(
            classify_stale_lease(&lease, now),
            Some(StaleReason::NoNetworkProgress)
        );
        assert!(StaleReason::NoNetworkProgress.blames_upload_line());
    }

    #[test]
    fn a_dead_owner_is_reaped_in_every_phase() {
        let now = now();
        for phase in [
            AttemptPhase::Preprocessing,
            AttemptPhase::Queued,
            AttemptPhase::Transferring,
        ] {
            let lease = LeaseSnapshot {
                phase: Some(phase),
                phase_started_at: Some(now - Duration::minutes(4)),
                last_heartbeat_at: Some(now - Duration::minutes(4)),
                total_bytes: Some(8 * BYTES_PER_GIB),
                ..base(now)
            };
            assert_eq!(
                classify_stale_lease(&lease, now),
                Some(StaleReason::HeartbeatLost),
                "{phase:?} must not outlive its owner"
            );
        }
    }

    #[test]
    fn rows_without_a_phase_keep_the_pre_upgrade_behaviour() {
        let now = now();
        let fresh = LeaseSnapshot {
            upload_started_at: Some(now - Duration::minutes(4)),
            ..base(now)
        };
        assert_eq!(classify_stale_lease(&fresh, now), None);

        let stale = LeaseSnapshot {
            upload_started_at: Some(now - Duration::minutes(6)),
            ..base(now)
        };
        assert_eq!(
            classify_stale_lease(&stale, now),
            Some(StaleReason::NoNetworkProgress)
        );
    }
}
