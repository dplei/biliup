//! 直播间录制租约。
//!
//! 数据库状态是权限边界；Worker 状态只负责让当前进程及时停止轮询。所有状态推进都使用带
//! `state`/`id` 条件的更新，重复扫描与迟到的旧任务不会关闭后来创建的租约。

use crate::server::common::cookie_health;
use crate::server::core::download_manager::DownloadManager;
use crate::server::errors::{AppError, AppResult};
use crate::server::infrastructure::connection_pool::ConnectionPool;
use crate::server::infrastructure::context::{
    ActiveRecordingSnapshot, Stage, Worker, WorkerStatus,
};
use chrono::{DateTime, Duration, FixedOffset, SecondsFormat, Utc};
use error_stack::{ResultExt, bail};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use std::sync::{Arc, RwLock};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};
use uuid::Uuid;

pub const ACTIVE_STATES_SQL: &str = "('scheduled', 'grace_current_session', 'expired_paused')";
const CLAIM_TIMEOUT_MINUTES: i64 = 5;

fn concurrent_write_error(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .is_some_and(|database| database.is_unique_violation())
        || error
            .to_string()
            .to_ascii_lowercase()
            .contains("database is locked")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingLeaseState {
    Scheduled,
    GraceCurrentSession,
    ExpiredPaused,
    Superseded,
    Cancelled,
}

impl RecordingLeaseState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::GraceCurrentSession => "grace_current_session",
            Self::ExpiredPaused => "expired_paused",
            Self::Superseded => "superseded",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(value: &str) -> AppResult<Self> {
        match value {
            "scheduled" => Ok(Self::Scheduled),
            "grace_current_session" => Ok(Self::GraceCurrentSession),
            "expired_paused" => Ok(Self::ExpiredPaused),
            "superseded" => Ok(Self::Superseded),
            "cancelled" => Ok(Self::Cancelled),
            other => bail!(AppError::Custom(format!("未知录制租约状态：{other}"))),
        }
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct RecordingLease {
    pub id: i64,
    pub live_streamer_id: i64,
    pub expires_at: DateTime<Utc>,
    pub customer_note: String,
    pub state: String,
    pub grace_streamer_info_id: Option<i64>,
    pub grace_live_session_key: Option<String>,
    pub pause_owned_by_lease: bool,
    pub effective_paused_at: Option<DateTime<Utc>>,
    pub notification_status: String,
    pub notification_claim_token: Option<String>,
    pub notification_claimed_at: Option<DateTime<Utc>>,
    pub notification_attempts: i64,
    pub next_notification_at: Option<DateTime<Utc>>,
    pub last_notification_error: Option<String>,
    pub notified_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl RecordingLease {
    pub fn parsed_state(&self) -> AppResult<RecordingLeaseState> {
        RecordingLeaseState::parse(&self.state)
    }

    pub fn projection(&self) -> RecordingLeaseProjection {
        RecordingLeaseProjection {
            id: self.id,
            expires_at: self.expires_at,
            customer_note: self.customer_note.clone(),
            state: self.state.clone(),
            effective_paused_at: self.effective_paused_at,
            notification_status: self.notification_status.clone(),
            last_notification_error: self.last_notification_error.clone(),
            notified_at: self.notified_at,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RecordingLeaseProjection {
    pub id: i64,
    pub expires_at: DateTime<Utc>,
    pub customer_note: String,
    pub state: String,
    pub effective_paused_at: Option<DateTime<Utc>>,
    pub notification_status: String,
    pub last_notification_error: Option<String>,
    pub notified_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DueAction {
    NotDue,
    GraceCurrentSession,
    Pause,
}

/// 到期扫描的纯决策。`now == expires_at` 明确定义为到期。
pub fn due_action(
    expires_at: DateTime<Utc>,
    now: DateTime<Utc>,
    active: Option<&ActiveRecordingSnapshot>,
) -> DueAction {
    if now < expires_at {
        return DueAction::NotDue;
    }
    if active.is_some_and(|session| session.recording_started_at <= expires_at) {
        DueAction::GraceCurrentSession
    } else {
        DueAction::Pause
    }
}

fn same_session(
    expected_id: Option<i64>,
    expected_key: Option<&str>,
    actual_id: Option<i64>,
    actual_key: Option<&str>,
) -> bool {
    expected_id.zip(actual_id).is_some_and(|(a, b)| a == b)
        || expected_key
            .filter(|key| !key.is_empty())
            .zip(actual_key.filter(|key| !key.is_empty()))
            .is_some_and(|(a, b)| a == b)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionDecision {
    Allow,
    AllowAndStartGrace,
    Block,
}

/// 录制启动前的纯准入判断。到期后的 scheduled 只接受可证明是在到期前开始的复用场次。
pub fn admission_decision(
    lease: &RecordingLease,
    now: DateTime<Utc>,
    candidate_id: Option<i64>,
    candidate_key: Option<&str>,
    candidate_started_at: DateTime<Utc>,
    is_reused: bool,
) -> AppResult<AdmissionDecision> {
    Ok(match lease.parsed_state()? {
        RecordingLeaseState::Scheduled if now < lease.expires_at => AdmissionDecision::Allow,
        RecordingLeaseState::Scheduled if is_reused && candidate_started_at <= lease.expires_at => {
            AdmissionDecision::AllowAndStartGrace
        }
        RecordingLeaseState::Scheduled => AdmissionDecision::Block,
        RecordingLeaseState::GraceCurrentSession
            if same_session(
                lease.grace_streamer_info_id,
                lease.grace_live_session_key.as_deref(),
                candidate_id,
                candidate_key,
            ) =>
        {
            AdmissionDecision::Allow
        }
        RecordingLeaseState::GraceCurrentSession | RecordingLeaseState::ExpiredPaused => {
            AdmissionDecision::Block
        }
        RecordingLeaseState::Superseded | RecordingLeaseState::Cancelled => {
            AdmissionDecision::Allow
        }
    })
}

pub async fn current_lease(
    pool: &ConnectionPool,
    live_streamer_id: i64,
) -> AppResult<Option<RecordingLease>> {
    sqlx::query_as::<_, RecordingLease>(&format!(
        "SELECT * FROM recording_lease WHERE live_streamer_id = ?1 AND state IN {ACTIVE_STATES_SQL} LIMIT 1"
    ))
    .bind(live_streamer_id)
    .fetch_optional(pool)
    .await
    .change_context(AppError::Unknown)
}

pub async fn current_lease_projections(
    pool: &ConnectionPool,
) -> AppResult<std::collections::HashMap<i64, RecordingLeaseProjection>> {
    let rows = sqlx::query_as::<_, RecordingLease>(&format!(
        "SELECT * FROM recording_lease WHERE state IN {ACTIVE_STATES_SQL}"
    ))
    .fetch_all(pool)
    .await
    .change_context(AppError::Unknown)?;
    Ok(rows
        .into_iter()
        .map(|lease| (lease.live_streamer_id, lease.projection()))
        .collect())
}

#[derive(Debug)]
pub struct ReplaceLeaseOutcome {
    pub lease: RecordingLease,
    pub resume_lease_owned_pause: bool,
}

/// 创建或替换当前租约。expected id 与替换、插入在同一个写事务中完成。
pub async fn replace_lease(
    pool: &ConnectionPool,
    live_streamer_id: i64,
    expires_at: DateTime<Utc>,
    customer_note: &str,
    expected_lease_id: Option<i64>,
    now: DateTime<Utc>,
) -> AppResult<ReplaceLeaseOutcome> {
    let mut tx = pool.begin().await.change_context(AppError::Unknown)?;
    let current = sqlx::query_as::<_, RecordingLease>(&format!(
        "SELECT * FROM recording_lease WHERE live_streamer_id = ?1 AND state IN {ACTIVE_STATES_SQL} LIMIT 1"
    ))
    .bind(live_streamer_id)
    .fetch_optional(&mut *tx)
    .await
    .change_context(AppError::Unknown)?;

    if current.as_ref().map(|lease| lease.id) != expected_lease_id {
        bail!(AppError::Custom(
            "录制期限已被其他页面更新，请刷新后重试".into()
        ));
    }

    let resume_lease_owned_pause = current
        .as_ref()
        .is_some_and(|lease| lease.state == "expired_paused" && lease.pause_owned_by_lease);
    if let Some(current) = &current {
        let changed = match sqlx::query(
            "UPDATE recording_lease SET state = 'superseded', updated_at = ?1 \
             WHERE id = ?2 AND state IN ('scheduled', 'grace_current_session', 'expired_paused')",
        )
        .bind(now)
        .bind(current.id)
        .execute(&mut *tx)
        .await
        {
            Ok(result) => result.rows_affected(),
            Err(error) if concurrent_write_error(&error) => bail!(AppError::Custom(
                "录制期限已被其他页面更新，请刷新后重试".into()
            )),
            Err(error) => return Err(error).change_context(AppError::Unknown),
        };
        if changed != 1 {
            bail!(AppError::Custom(
                "录制期限已被其他页面更新，请刷新后重试".into()
            ));
        }
    }

    match sqlx::query(
        "INSERT INTO recording_lease \
         (live_streamer_id, expires_at, customer_note, state, created_at, updated_at) \
         VALUES (?1, ?2, ?3, 'scheduled', ?4, ?4)",
    )
    .bind(live_streamer_id)
    .bind(expires_at)
    .bind(customer_note)
    .bind(now)
    .execute(&mut *tx)
    .await
    {
        Ok(_) => {}
        Err(error) if concurrent_write_error(&error) => bail!(AppError::Custom(
            "录制期限已被其他页面更新，请刷新后重试".into()
        )),
        Err(error) => return Err(error).change_context(AppError::Unknown),
    }
    let id: i64 = sqlx::query_scalar("SELECT last_insert_rowid()")
        .fetch_one(&mut *tx)
        .await
        .change_context(AppError::Unknown)?;
    let lease = sqlx::query_as::<_, RecordingLease>("SELECT * FROM recording_lease WHERE id = ?1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .change_context(AppError::Unknown)?;
    tx.commit().await.change_context(AppError::Unknown)?;
    info!(
        event = "recording_lease_created",
        live_streamer_id,
        lease_id = id,
        state = "scheduled"
    );
    if current.is_some() {
        info!(
            event = "recording_lease_superseded",
            live_streamer_id,
            lease_id = id
        );
    }
    Ok(ReplaceLeaseOutcome {
        lease,
        resume_lease_owned_pause,
    })
}

#[derive(Debug)]
pub struct CancelLeaseOutcome {
    pub resume_lease_owned_pause: bool,
    pub already_cancelled: bool,
}

pub async fn cancel_lease(
    pool: &ConnectionPool,
    live_streamer_id: i64,
    lease_id: i64,
    now: DateTime<Utc>,
) -> AppResult<CancelLeaseOutcome> {
    let mut tx = pool.begin().await.change_context(AppError::Unknown)?;
    let target = sqlx::query_as::<_, RecordingLease>(
        "SELECT * FROM recording_lease WHERE id = ?1 AND live_streamer_id = ?2",
    )
    .bind(lease_id)
    .bind(live_streamer_id)
    .fetch_optional(&mut *tx)
    .await
    .change_context(AppError::Unknown)?
    .ok_or_else(|| error_stack::Report::new(AppError::Custom("录制期限不存在".into())))?;

    if target.state == "cancelled" {
        tx.commit().await.change_context(AppError::Unknown)?;
        return Ok(CancelLeaseOutcome {
            resume_lease_owned_pause: false,
            already_cancelled: true,
        });
    }
    if !matches!(
        target.state.as_str(),
        "scheduled" | "grace_current_session" | "expired_paused"
    ) {
        bail!(AppError::Custom("该录制期限已被替换，请刷新后重试".into()));
    }
    let changed = sqlx::query(
        "UPDATE recording_lease SET state = 'cancelled', updated_at = ?1 \
         WHERE id = ?2 AND live_streamer_id = ?3 \
           AND state IN ('scheduled', 'grace_current_session', 'expired_paused')",
    )
    .bind(now)
    .bind(lease_id)
    .bind(live_streamer_id)
    .execute(&mut *tx)
    .await
    .change_context(AppError::Unknown)?
    .rows_affected();
    if changed != 1 {
        bail!(AppError::Custom(
            "录制期限已被其他页面更新，请刷新后重试".into()
        ));
    }
    tx.commit().await.change_context(AppError::Unknown)?;
    info!(
        event = "recording_lease_cancelled",
        live_streamer_id, lease_id
    );
    Ok(CancelLeaseOutcome {
        resume_lease_owned_pause: target.state == "expired_paused" && target.pause_owned_by_lease,
        already_cancelled: false,
    })
}

async fn start_grace(
    pool: &ConnectionPool,
    lease_id: i64,
    session: &ActiveRecordingSnapshot,
    now: DateTime<Utc>,
) -> AppResult<bool> {
    let changed = sqlx::query(
        "UPDATE recording_lease SET state = 'grace_current_session', \
         grace_streamer_info_id = ?1, grace_live_session_key = ?2, updated_at = ?3 \
         WHERE id = ?4 AND state = 'scheduled' AND expires_at <= ?3",
    )
    .bind(session.streamer_info_id)
    .bind(&session.live_session_key)
    .bind(now)
    .bind(lease_id)
    .execute(pool)
    .await
    .change_context(AppError::Unknown)?
    .rows_affected()
        == 1;
    Ok(changed)
}

async fn transition_to_expired_paused(
    pool: &ConnectionPool,
    lease_id: i64,
    from_state: &str,
    pause_owned_by_lease: bool,
    now: DateTime<Utc>,
) -> AppResult<bool> {
    let changed = sqlx::query(
        "UPDATE recording_lease SET state = 'expired_paused', pause_owned_by_lease = ?1, \
         effective_paused_at = ?2, notification_status = 'pending', next_notification_at = ?2, \
         notification_claim_token = NULL, notification_claimed_at = NULL, updated_at = ?2 \
         WHERE id = ?3 AND state = ?4",
    )
    .bind(pause_owned_by_lease)
    .bind(now)
    .bind(lease_id)
    .bind(from_state)
    .execute(pool)
    .await
    .change_context(AppError::Unknown)?
    .rows_affected()
        == 1;
    Ok(changed)
}

/// 单轮到期扫描，显式接收时钟以便测试。
pub async fn scan_due_recording_leases(
    pool: &ConnectionPool,
    managers: &Arc<DownloadManager>,
    now: DateTime<Utc>,
) -> AppResult<usize> {
    let due = sqlx::query_as::<_, RecordingLease>(
        "SELECT * FROM recording_lease WHERE state = 'scheduled' AND expires_at <= ?1 ORDER BY expires_at, id",
    )
    .bind(now)
    .fetch_all(pool)
    .await
    .change_context(AppError::Unknown)?;
    let mut changed = 0;
    for lease in due {
        info!(
            event = "recording_lease_due",
            live_streamer_id = lease.live_streamer_id,
            lease_id = lease.id,
            state = lease.state
        );
        let Some(worker) = managers.get_room_by_id(lease.live_streamer_id).await else {
            error!(
                event = "recording_lease_due",
                live_streamer_id = lease.live_streamer_id,
                lease_id = lease.id,
                "租约到期但 Worker 不可用，保留 scheduled 等待重试"
            );
            continue;
        };
        let active = worker.active_recording();
        match due_action(lease.expires_at, now, active.as_ref()) {
            DueAction::NotDue => {}
            DueAction::GraceCurrentSession => {
                let active = active.expect("due_action required active session");
                if start_grace(pool, lease.id, &active, now).await? {
                    changed += 1;
                    info!(
                        event = "recording_lease_grace_started",
                        live_streamer_id = lease.live_streamer_id,
                        streamer_info_id = active.streamer_info_id,
                        lease_id = lease.id,
                        state = "grace_current_session"
                    );
                }
            }
            DueAction::Pause => {
                let already_manual_pause = matches!(
                    *worker.downloader_status.read().unwrap(),
                    WorkerStatus::Pause
                );
                if transition_to_expired_paused(
                    pool,
                    lease.id,
                    "scheduled",
                    !already_manual_pause,
                    now,
                )
                .await?
                {
                    worker
                        .change_status(Stage::Download, WorkerStatus::Pause)
                        .await;
                    managers.make_waker(lease.live_streamer_id).await;
                    changed += 1;
                    info!(
                        event = "recording_lease_paused",
                        live_streamer_id = lease.live_streamer_id,
                        lease_id = lease.id,
                        state = "expired_paused"
                    );
                }
            }
        }
    }
    Ok(changed)
}

/// 服务启动时应用持久的 Pause。grace/scheduled 仍入队，但最终准入会再次查库。
pub async fn apply_initial_state(pool: &ConnectionPool, worker: &Worker) -> AppResult<()> {
    if current_lease(pool, worker.id())
        .await?
        .is_some_and(|lease| lease.state == "expired_paused")
    {
        worker
            .change_status(Stage::Download, WorkerStatus::Pause)
            .await;
    }
    Ok(())
}

/// Monitor 在写入新 `streamerinfo` 之前调用。被阻断时同步收敛持久状态。
pub async fn admit_detected_session(
    pool: &ConnectionPool,
    worker: &Arc<Worker>,
    candidate_id: Option<i64>,
    candidate_key: Option<&str>,
    candidate_started_at: DateTime<Utc>,
    is_reused: bool,
    now: DateTime<Utc>,
) -> AppResult<bool> {
    let Some(lease) = current_lease(pool, worker.id()).await? else {
        return Ok(true);
    };
    match admission_decision(
        &lease,
        now,
        candidate_id,
        candidate_key,
        candidate_started_at,
        is_reused,
    )? {
        AdmissionDecision::Allow => Ok(true),
        AdmissionDecision::AllowAndStartGrace => {
            let session = ActiveRecordingSnapshot {
                streamer_info_id: candidate_id.expect("reused session has id"),
                live_session_key: candidate_key.map(ToOwned::to_owned),
                recording_started_at: candidate_started_at,
            };
            let _ = start_grace(pool, lease.id, &session, now).await?;
            info!(
                event = "recording_lease_same_session_resumed",
                live_streamer_id = worker.id(),
                streamer_info_id = session.streamer_info_id,
                lease_id = lease.id
            );
            Ok(true)
        }
        AdmissionDecision::Block => {
            if lease.state != "expired_paused" {
                let _ =
                    transition_to_expired_paused(pool, lease.id, &lease.state, true, now).await?;
            }
            worker
                .change_status(Stage::Download, WorkerStatus::Pause)
                .await;
            warn!(
                event = "recording_lease_new_session_blocked",
                live_streamer_id = worker.id(),
                lease_id = lease.id,
                candidate_streamer_info_id = candidate_id,
                "录制期限不允许开始该场直播"
            );
            Ok(false)
        }
    }
}

/// 确认下播后的原子边界。返回 true 时调用方不得再把 Worker 放回轮询队列。
pub async fn complete_grace_session(
    pool: &ConnectionPool,
    worker: &Arc<Worker>,
    streamer_info_id: i64,
    live_session_key: Option<&str>,
    now: DateTime<Utc>,
) -> AppResult<bool> {
    let Some(lease) = current_lease(pool, worker.id()).await? else {
        return Ok(false);
    };
    if lease.state != "grace_current_session"
        || !same_session(
            lease.grace_streamer_info_id,
            lease.grace_live_session_key.as_deref(),
            Some(streamer_info_id),
            live_session_key,
        )
    {
        return Ok(false);
    }
    let already_manual_pause = matches!(
        *worker.downloader_status.read().unwrap(),
        WorkerStatus::Pause
    );
    let changed = transition_to_expired_paused(
        pool,
        lease.id,
        "grace_current_session",
        !already_manual_pause,
        now,
    )
    .await?;
    if changed {
        worker.finish_download_status(WorkerStatus::Pause);
        info!(
            event = "recording_lease_paused",
            live_streamer_id = worker.id(),
            streamer_info_id,
            lease_id = lease.id,
            state = "expired_paused"
        );
    }
    Ok(changed)
}

pub async fn resume_worker_if_owned(managers: &Arc<DownloadManager>, id: i64) {
    if let Some(worker) = managers.get_room_by_id(id).await {
        worker
            .change_status(Stage::Download, WorkerStatus::Idle)
            .await;
        managers.wake_waker(id).await;
    }
}

#[derive(Debug, FromRow)]
struct NotificationLease {
    #[sqlx(flatten)]
    lease: RecordingLease,
    remark: String,
}

fn notification_retry_delay(attempts: i64) -> Duration {
    match attempts {
        0 | 1 => Duration::minutes(1),
        2 => Duration::minutes(5),
        3 => Duration::minutes(15),
        _ => Duration::hours(1),
    }
}

fn format_shanghai(value: DateTime<Utc>) -> String {
    let offset = FixedOffset::east_opt(8 * 3600).expect("valid UTC+8 offset");
    value
        .with_timezone(&offset)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

fn notification_message(row: &NotificationLease) -> (String, String) {
    let lease = &row.lease;
    let title = "⏰ 客户录制到期，已暂停后续录制".to_string();
    let pause_time = lease
        .effective_paused_at
        .map(format_shanghai)
        .unwrap_or_else(|| "未知".into());
    let session_end =
        if lease.grace_streamer_info_id.is_some() || lease.grace_live_session_key.is_some() {
            format!("\n本场结束：{pause_time}")
        } else {
            String::new()
        };
    // 备注选填，留空时给占位，别推送出一行光秃秃的「客户/需求：」。
    let customer_note = if lease.customer_note.trim().is_empty() {
        "（未填写）"
    } else {
        lease.customer_note.trim()
    };
    let content = format!(
        "客户/需求：{}\n主播：{}\n约定到期：{} (Asia/Shanghai){}\n实际暂停：{}\n租约事件：#{}",
        customer_note,
        row.remark,
        format_shanghai(lease.expires_at),
        session_end,
        pause_time,
        lease.id,
    );
    (title, content)
}

/// 单轮可靠通知扫描。claim、发送结果和退避均持久化。
pub async fn scan_recording_lease_notifications(
    pool: &ConnectionPool,
    webhook: Option<&str>,
    now: DateTime<Utc>,
) -> AppResult<usize> {
    let configured = webhook.is_some_and(|url| !url.trim().is_empty());
    if !configured {
        sqlx::query(
            "UPDATE recording_lease SET notification_status = 'not_configured', updated_at = ?1 \
             WHERE state = 'expired_paused' AND notification_status IN ('pending', 'failed')",
        )
        .bind(now)
        .execute(pool)
        .await
        .change_context(AppError::Unknown)?;
        return Ok(0);
    }

    let stale_before = now - Duration::minutes(CLAIM_TIMEOUT_MINUTES);
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM recording_lease WHERE state = 'expired_paused' AND (\
           (notification_status IN ('pending', 'failed', 'not_configured') AND (next_notification_at IS NULL OR next_notification_at <= ?1)) \
           OR (notification_status = 'sending' AND notification_claimed_at <= ?2)) \
         ORDER BY id LIMIT 20",
    )
    .bind(now)
    .bind(stale_before)
    .fetch_all(pool)
    .await
    .change_context(AppError::Unknown)?;

    let mut sent = 0;
    for id in ids {
        let token = Uuid::new_v4().to_string();
        let claimed = sqlx::query(
            "UPDATE recording_lease SET notification_status = 'sending', notification_claim_token = ?1, \
             notification_claimed_at = ?2, notification_attempts = notification_attempts + 1, updated_at = ?2 \
             WHERE id = ?3 AND state = 'expired_paused' AND (\
               (notification_status IN ('pending', 'failed', 'not_configured') AND (next_notification_at IS NULL OR next_notification_at <= ?2)) \
               OR (notification_status = 'sending' AND notification_claimed_at <= ?4))",
        )
        .bind(&token)
        .bind(now)
        .bind(id)
        .bind(stale_before)
        .execute(pool)
        .await
        .change_context(AppError::Unknown)?
        .rows_affected();
        if claimed != 1 {
            continue;
        }
        let row = sqlx::query_as::<_, NotificationLease>(
            "SELECT r.*, l.remark FROM recording_lease r \
             JOIN livestreamers l ON l.id = r.live_streamer_id WHERE r.id = ?1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .change_context(AppError::Unknown)?;
        let (title, content) = notification_message(&row);
        match cookie_health::send_webhook(webhook.expect("configured"), &title, &content).await {
            Ok(()) => {
                let changed = sqlx::query(
                    "UPDATE recording_lease SET notification_status = 'sent', notified_at = ?1, \
                     next_notification_at = NULL, last_notification_error = NULL, updated_at = ?1 \
                     WHERE id = ?2 AND notification_status = 'sending' AND notification_claim_token = ?3",
                )
                .bind(Utc::now())
                .bind(id)
                .bind(&token)
                .execute(pool)
                .await
                .change_context(AppError::Unknown)?
                .rows_affected();
                if changed == 1 {
                    sent += 1;
                    info!(
                        event = "recording_lease_notification_sent",
                        live_streamer_id = row.lease.live_streamer_id,
                        lease_id = id
                    );
                }
            }
            Err(message) => {
                let summary: String = message.chars().take(300).collect();
                let retry_at = now + notification_retry_delay(row.lease.notification_attempts);
                sqlx::query(
                    "UPDATE recording_lease SET notification_status = 'failed', next_notification_at = ?1, \
                     last_notification_error = ?2, notification_claim_token = NULL, notification_claimed_at = NULL, updated_at = ?3 \
                     WHERE id = ?4 AND notification_status = 'sending' AND notification_claim_token = ?5",
                )
                .bind(retry_at)
                .bind(&summary)
                .bind(now)
                .bind(id)
                .bind(&token)
                .execute(pool)
                .await
                .change_context(AppError::Unknown)?;
                warn!(event = "recording_lease_notification_retry", live_streamer_id = row.lease.live_streamer_id, lease_id = id, next_notification_at = %retry_at.to_rfc3339_opts(SecondsFormat::Secs, true), error = summary);
            }
        }
    }
    Ok(sent)
}

pub struct RecordingLeaseTaskHandles {
    due: JoinHandle<()>,
    notifications: JoinHandle<()>,
}

impl RecordingLeaseTaskHandles {
    pub fn abort(&self) {
        self.due.abort();
        self.notifications.abort();
    }
}

pub fn start_recording_lease_tasks(
    pool: ConnectionPool,
    managers: Arc<DownloadManager>,
    config: Arc<RwLock<crate::server::config::Config>>,
) -> RecordingLeaseTaskHandles {
    let due_pool = pool.clone();
    let due = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            if let Err(report) = scan_due_recording_leases(&due_pool, &managers, Utc::now()).await {
                error!(error = ?report, "录制租约到期扫描失败");
            }
        }
    });
    let notifications = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            interval.tick().await;
            let webhook = config.read().unwrap().cookie_health_webhook.clone();
            if let Err(report) =
                scan_recording_lease_notifications(&pool, webhook.as_deref(), Utc::now()).await
            {
                error!(error = ?report, "录制租约通知扫描失败");
            }
        }
    });
    RecordingLeaseTaskHandles { due, notifications }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::config::Config;
    use crate::server::infrastructure::connection_pool::test_support::migrated_pool;
    use crate::server::infrastructure::context::Worker;
    use crate::server::infrastructure::models::live_streamer::LiveStreamer;
    use axum::{Router, extract::State, http::StatusCode, routing::post};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn at(second: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(second, 0).unwrap()
    }

    fn snapshot(started: i64) -> ActiveRecordingSnapshot {
        ActiveRecordingSnapshot {
            streamer_info_id: 7,
            live_session_key: Some("session-7".into()),
            recording_started_at: at(started),
        }
    }

    fn worker(id: i64) -> Arc<Worker> {
        Arc::new(Worker::new(
            LiveStreamer {
                id,
                url: format!("https://example/{id}"),
                remark: format!("主播{id}"),
                filename_prefix: None,
                time_range: None,
                upload_streamers_id: None,
                format: None,
                override_cfg: None,
                preprocessor: None,
                segment_processor: None,
                downloaded_processor: None,
                postprocessor: None,
                opt_args: None,
                excluded_keywords: None,
                cover_background: None,
            },
            None,
            Arc::new(RwLock::new(Config::default())),
            biliup::client::StatelessClient::default(),
        ))
    }

    #[test]
    fn expiry_boundary_is_inclusive() {
        assert_eq!(due_action(at(10), at(9), None), DueAction::NotDue);
        assert_eq!(due_action(at(10), at(10), None), DueAction::Pause);
        assert_eq!(
            due_action(at(10), at(10), Some(&snapshot(10))),
            DueAction::GraceCurrentSession
        );
        assert_eq!(
            due_action(at(10), at(11), Some(&snapshot(11))),
            DueAction::Pause
        );
    }

    #[tokio::test]
    async fn replace_and_cancel_keep_a_single_active_lease() {
        let (_dir, pool) = migrated_pool().await;
        sqlx::query("INSERT INTO livestreamers (url, remark) VALUES ('https://example/1', '甲')")
            .execute(&pool)
            .await
            .unwrap();
        let first = replace_lease(&pool, 1, at(100), "需求甲", None, at(1))
            .await
            .unwrap();
        let second = replace_lease(&pool, 1, at(200), "需求乙", Some(first.lease.id), at(2))
            .await
            .unwrap();
        assert_ne!(first.lease.id, second.lease.id);
        assert_eq!(
            current_lease(&pool, 1).await.unwrap().unwrap().id,
            second.lease.id
        );
        cancel_lease(&pool, 1, second.lease.id, at(3))
            .await
            .unwrap();
        assert!(current_lease(&pool, 1).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn stale_expected_id_cannot_replace_newer_lease() {
        let (_dir, pool) = migrated_pool().await;
        sqlx::query("INSERT INTO livestreamers (url, remark) VALUES ('https://example/1', '甲')")
            .execute(&pool)
            .await
            .unwrap();
        let first = replace_lease(&pool, 1, at(100), "需求甲", None, at(1))
            .await
            .unwrap();
        let second = replace_lease(&pool, 1, at(200), "需求乙", Some(first.lease.id), at(2))
            .await
            .unwrap();
        assert!(
            replace_lease(&pool, 1, at(300), "旧页面", Some(first.lease.id), at(3))
                .await
                .is_err()
        );
        assert_eq!(
            current_lease(&pool, 1).await.unwrap().unwrap().id,
            second.lease.id
        );
    }

    #[tokio::test]
    async fn concurrent_creates_leave_exactly_one_active_lease() {
        let (_dir, pool) = migrated_pool().await;
        sqlx::query("INSERT INTO livestreamers (url, remark) VALUES ('https://example/1', '甲')")
            .execute(&pool)
            .await
            .unwrap();
        let (left, right) = tokio::join!(
            replace_lease(&pool, 1, at(100), "需求甲", None, at(1)),
            replace_lease(&pool, 1, at(200), "需求乙", None, at(1)),
        );
        assert_ne!(left.is_ok(), right.is_ok());
        let active: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM recording_lease WHERE live_streamer_id = 1 \
             AND state IN ('scheduled', 'grace_current_session', 'expired_paused')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(active, 1);
    }

    #[tokio::test]
    async fn deleting_streamer_cascades_lease_history() {
        let (_dir, pool) = migrated_pool().await;
        sqlx::query("INSERT INTO livestreamers (url, remark) VALUES ('https://example/1', '甲')")
            .execute(&pool)
            .await
            .unwrap();
        replace_lease(&pool, 1, at(100), "需求甲", None, at(1))
            .await
            .unwrap();
        sqlx::query("DELETE FROM livestreamers WHERE id = 1")
            .execute(&pool)
            .await
            .unwrap();
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM recording_lease")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[tokio::test]
    async fn matching_grace_session_pauses_once_before_requeue_boundary() {
        let (_dir, pool) = migrated_pool().await;
        sqlx::query("INSERT INTO livestreamers (url, remark) VALUES ('https://example/1', '甲')")
            .execute(&pool)
            .await
            .unwrap();
        let lease = replace_lease(&pool, 1, at(100), "需求甲", None, at(1))
            .await
            .unwrap()
            .lease;
        let session = snapshot(90);
        assert!(
            start_grace(&pool, lease.id, &session, at(100))
                .await
                .unwrap()
        );
        let worker = worker(1);
        assert!(
            complete_grace_session(
                &pool,
                &worker,
                session.streamer_info_id,
                session.live_session_key.as_deref(),
                at(120),
            )
            .await
            .unwrap()
        );
        assert!(matches!(
            *worker.downloader_status.read().unwrap(),
            WorkerStatus::Pause
        ));
        let expired = current_lease(&pool, 1).await.unwrap().unwrap();
        assert_eq!(expired.state, "expired_paused");
        assert_eq!(expired.notification_status, "pending");
        assert!(
            !complete_grace_session(&pool, &worker, 7, Some("session-7"), at(121))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn old_session_completion_cannot_close_replacement_lease() {
        let (_dir, pool) = migrated_pool().await;
        sqlx::query("INSERT INTO livestreamers (url, remark) VALUES ('https://example/1', '甲')")
            .execute(&pool)
            .await
            .unwrap();
        let first = replace_lease(&pool, 1, at(100), "需求甲", None, at(1))
            .await
            .unwrap()
            .lease;
        let session = snapshot(90);
        start_grace(&pool, first.id, &session, at(100))
            .await
            .unwrap();
        let second = replace_lease(&pool, 1, at(300), "已延期", Some(first.id), at(110))
            .await
            .unwrap()
            .lease;
        let worker = worker(1);
        assert!(
            !complete_grace_session(&pool, &worker, 7, Some("session-7"), at(120))
                .await
                .unwrap()
        );
        assert_eq!(
            current_lease(&pool, 1).await.unwrap().unwrap().id,
            second.id
        );
        assert!(!matches!(
            *worker.downloader_status.read().unwrap(),
            WorkerStatus::Pause
        ));
    }

    async fn fake_webhook(State(calls): State<Arc<AtomicUsize>>) -> StatusCode {
        calls.fetch_add(1, Ordering::SeqCst);
        StatusCode::OK
    }

    #[tokio::test]
    async fn successful_notification_is_persisted_and_not_sent_twice() {
        let (_dir, pool) = migrated_pool().await;
        sqlx::query("INSERT INTO livestreamers (url, remark) VALUES ('https://example/1', '甲')")
            .execute(&pool)
            .await
            .unwrap();
        let lease = replace_lease(&pool, 1, at(100), "需求甲", None, at(1))
            .await
            .unwrap()
            .lease;
        sqlx::query(
            "UPDATE recording_lease SET state = 'expired_paused', effective_paused_at = ?1, \
             notification_status = 'pending', next_notification_at = ?1 WHERE id = ?2",
        )
        .bind(at(101))
        .bind(lease.id)
        .execute(&pool)
        .await
        .unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/", post(fake_webhook))
            .with_state(calls.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let webhook = format!("http://{address}/");

        assert_eq!(
            scan_recording_lease_notifications(&pool, Some(&webhook), at(101))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            scan_recording_lease_notifications(&pool, Some(&webhook), at(200))
                .await
                .unwrap(),
            0
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            current_lease(&pool, 1)
                .await
                .unwrap()
                .unwrap()
                .notification_status,
            "sent"
        );
        server.abort();
    }

    async fn failing_webhook(State(calls): State<Arc<AtomicUsize>>) -> StatusCode {
        calls.fetch_add(1, Ordering::SeqCst);
        StatusCode::INTERNAL_SERVER_ERROR
    }

    #[tokio::test]
    async fn failed_notification_uses_first_retry_delay() {
        let (_dir, pool) = migrated_pool().await;
        sqlx::query("INSERT INTO livestreamers (url, remark) VALUES ('https://example/1', '甲')")
            .execute(&pool)
            .await
            .unwrap();
        let lease = replace_lease(&pool, 1, at(100), "需求甲", None, at(1))
            .await
            .unwrap()
            .lease;
        sqlx::query(
            "UPDATE recording_lease SET state = 'expired_paused', effective_paused_at = ?1, \
             notification_status = 'pending', next_notification_at = ?1 WHERE id = ?2",
        )
        .bind(at(101))
        .bind(lease.id)
        .execute(&pool)
        .await
        .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route("/", post(failing_webhook))
            .with_state(calls.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let webhook = format!("http://{address}/");

        scan_recording_lease_notifications(&pool, Some(&webhook), at(101))
            .await
            .unwrap();
        let failed = current_lease(&pool, 1).await.unwrap().unwrap();
        assert_eq!(failed.notification_status, "failed");
        assert_eq!(failed.next_notification_at, Some(at(161)));
        assert_eq!(failed.notification_attempts, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        server.abort();
    }
}
