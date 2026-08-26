//! Detached execution of recovery work, and the loop that starts it on its own.
//!
//! Two gaps this closes.
//!
//! First, recovery used to run inside the HTTP handler. A reverse-proxy read timeout dropped the
//! handler future, which dropped the upload *and* the watchdog that shared its `select!`, so the
//! lifecycle row was left at `uploading` with nobody to fail it. Everything here runs on a
//! detached task that owns its own terminal state.
//!
//! Second, due rows were only ever picked up from inside `process_with_upload`, i.e. only when a
//! new live segment arrived. After a restart, an already-due segment waited for the streamer to
//! go live again. [`start_due_recovery_scan`] takes that job.

use crate::server::common::recovery_eligibility::RecoveryEligibility;
use crate::server::common::upload::{
    ClaimedRecovery, RecoveryClaim, claim_manual_recovery, run_claimed_recovery,
};
use crate::server::config::Config;
use crate::server::errors::{AppError, AppResult};
use crate::server::infrastructure::connection_pool::ConnectionPool;
use chrono::{DateTime, Utc};
use error_stack::ResultExt;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use tracing::{error, info, warn};

/// How often the startup scanner looks for rows that have come due.
const SCAN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Sessions with a recovery run in flight.
///
/// Recovery is ordered by `segment_order` within a session, so a session is driven by exactly one
/// task at a time. Rows with no session get their own key, derived from the row id, so an unbound
/// row cannot block a real session (or be blocked by one).
static ACTIVE_GROUPS: OnceLock<Mutex<HashSet<i64>>> = OnceLock::new();

fn active_groups() -> &'static Mutex<HashSet<i64>> {
    ACTIVE_GROUPS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Releases its group key on drop, including on panic — otherwise one panicking run would wedge a
/// session's recovery until the process restarted.
struct GroupGuard(i64);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        active_groups()
            .lock()
            .expect("recovery group registry poisoned")
            .remove(&self.0);
    }
}

fn try_claim_group(key: i64) -> Option<GroupGuard> {
    let mut groups = active_groups()
        .lock()
        .expect("recovery group registry poisoned");
    groups.insert(key).then(|| GroupGuard(key))
}

fn group_key(upload_session_id: Option<i64>, missing_id: i64) -> i64 {
    // Session ids are positive, so negating a row id cannot collide with one.
    upload_session_id.unwrap_or(-missing_id)
}

/// Run a claimed recovery on a detached task.
///
/// The lease is already held by the time this is called, so the page shows the task as
/// `uploading` immediately and the caller can return within a second.
pub fn spawn_claimed_recovery(config: Config, pool: ConnectionPool, claim: Box<ClaimedRecovery>) {
    let missing_id = claim.missing_id();
    tokio::spawn(async move {
        match run_claimed_recovery(&config, &pool, *claim).await {
            Ok(decision) => info!(missing_id, ?decision, "background recovery task finished"),
            // `run_claimed_recovery` has already released the lease and written `last_error`; this
            // log is for correlation, not for recovery.
            Err(error) => error!(?error, missing_id, "background recovery task failed"),
        }
    });
}

/// One due row, in the order recovery must visit it.
#[derive(Debug, sqlx::FromRow)]
struct DueRow {
    id: i64,
    upload_session_id: Option<i64>,
    /// Selected and ordered by, not read: the SQL `ORDER BY` is what makes recovery visit a
    /// session's segments in enrollment order.
    #[allow(dead_code)]
    segment_order: i64,
}

/// Rows that are due now: `pending`/`failed`, past their retry time, in `segment_order`.
///
/// `finalized` sessions are excluded here as a cheap first pass; the authoritative check is
/// `check_recovery_eligibility`, which every claim goes through.
async fn due_rows(
    pool: &ConnectionPool,
    session_id: Option<i64>,
    now: DateTime<Utc>,
) -> AppResult<Vec<DueRow>> {
    let mut sql = String::from(
        "SELECT m.id, m.upload_session_id, m.segment_order FROM upload_missing_segment m \
         LEFT JOIN upload_session s ON s.id = m.upload_session_id \
         WHERE m.status IN ('pending', 'failed') AND m.next_retry_at <= ?1 \
           AND (s.id IS NULL OR s.status != 'finalized')",
    );
    if session_id.is_some() {
        sql.push_str(" AND m.upload_session_id = ?2");
    }
    sql.push_str(" ORDER BY m.upload_session_id ASC, m.segment_order ASC, m.id ASC");
    let mut query = sqlx::query_as::<_, DueRow>(&sql).bind(now);
    if let Some(session_id) = session_id {
        query = query.bind(session_id);
    }
    query
        .fetch_all(pool)
        .await
        .change_context(AppError::Unknown)
}

/// Which rows a scan started, grouped by session.
#[derive(Debug, Default, serde::Serialize)]
pub struct RecoveryScanResult {
    /// Rows whose lease this scan took (they are now `uploading`).
    pub started: Vec<i64>,
    /// Rows that were due but not startable, with the reason.
    pub skipped: Vec<(i64, String)>,
    /// Groups already being recovered by another run.
    pub busy_sessions: Vec<i64>,
}

/// Claim and start every due row, session by session, in `segment_order`.
///
/// Concurrency is handled at two levels: one task per session (so ordering holds), and the
/// per-row `attempt_token` CAS (so the scan loop and a manual click cannot both start the same
/// row). Passing `session_id` restricts the scan to one session.
pub async fn recover_due_segments(
    config: &Config,
    pool: &ConnectionPool,
    session_id: Option<i64>,
    now: DateTime<Utc>,
) -> AppResult<RecoveryScanResult> {
    let mut grouped: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut order = Vec::new();
    for row in due_rows(pool, session_id, now).await? {
        let key = group_key(row.upload_session_id, row.id);
        if !grouped.contains_key(&key) {
            order.push(key);
        }
        grouped.entry(key).or_default().push(row.id);
    }

    let mut result = RecoveryScanResult::default();
    for key in order {
        let rows = grouped.remove(&key).unwrap_or_default();
        let Some(guard) = try_claim_group(key) else {
            result.busy_sessions.push(key);
            continue;
        };
        let mut claims = Vec::new();
        for missing_id in rows {
            match claim_manual_recovery(config, pool, missing_id, None).await {
                Ok(RecoveryClaim::Claimed(claim)) => {
                    result.started.push(missing_id);
                    claims.push(claim);
                }
                Ok(RecoveryClaim::Rejected(decision)) => {
                    // `Eligible` never reaches here; every other verdict is a real reason not to
                    // run, and is worth showing rather than silently dropping.
                    result
                        .skipped
                        .push((missing_id, format!("{:?}", RejectedAs(decision))));
                }
                Err(error) => {
                    warn!(?error, missing_id, "领取到期补传任务失败，保留原状");
                    result
                        .skipped
                        .push((missing_id, "claim_failed".to_string()));
                }
            }
        }
        if claims.is_empty() {
            continue;
        }
        // One task per session drains its claims in order; the guard is released when it ends.
        let config = config.clone();
        let pool = pool.clone();
        tokio::spawn(async move {
            let _guard = guard;
            for claim in claims {
                let missing_id = claim.missing_id();
                match run_claimed_recovery(&config, &pool, *claim).await {
                    Ok(decision) => {
                        info!(missing_id, ?decision, "到期补传任务结束")
                    }
                    Err(error) => error!(?error, missing_id, "到期补传任务失败"),
                }
            }
        });
    }
    Ok(result)
}

/// Newtype so a rejected verdict renders as a stable snake_case reason.
struct RejectedAs(RecoveryEligibility);

impl std::fmt::Debug for RejectedAs {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = serde_json::to_string(&self.0).unwrap_or_else(|_| "\"unknown\"".to_string());
        formatter.write_str(text.trim_matches('"'))
    }
}

/// Start the periodic due-row scan.
///
/// Runs at the same level as the stale-lease reaper: without it, a restart left already-due
/// segments waiting for the next live event that might be days away.
pub fn start_due_recovery_scan(config: Arc<RwLock<Config>>, pool: ConnectionPool) {
    tokio::spawn(async move {
        loop {
            // The guard is dropped before the first await; holding a std RwLock across one
            // would make this future non-Send.
            let snapshot = {
                let Ok(config) = config.read() else {
                    error!("读取配置失败，跳过本轮到期补传扫描");
                    tokio::time::sleep(SCAN_INTERVAL).await;
                    continue;
                };
                config.clone()
            };
            match recover_due_segments(&snapshot, &pool, None, Utc::now()).await {
                Ok(result) if !result.started.is_empty() => info!(
                    started = ?result.started,
                    skipped = result.skipped.len(),
                    "主动补传扫描领取了到期分段"
                ),
                Ok(_) => {}
                Err(error) => error!(?error, "主动补传扫描失败"),
            }
            tokio::time::sleep(SCAN_INTERVAL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_group_is_driven_by_one_run_at_a_time() {
        let guard = try_claim_group(4242).expect("first run owns the group");
        assert!(
            try_claim_group(4242).is_none(),
            "a second run must not reorder a session that is already recovering"
        );
        drop(guard);
        assert!(
            try_claim_group(4242).is_some(),
            "the group must be released once its run ends"
        );
    }

    #[test]
    fn unbound_rows_never_collide_with_session_groups() {
        assert_eq!(group_key(Some(7), 7), 7);
        assert_eq!(group_key(None, 7), -7);
        assert_ne!(group_key(None, 7), group_key(Some(7), 99));
    }
}
