use crate::server::common::upload_line_selection::RECOVERABLE_LINES;
use crate::server::errors::{AppError, AppResult};
use crate::server::infrastructure::connection_pool::ConnectionPool;
use biliup::error::Kind;
use chrono::{DateTime, Duration, Utc};
use error_stack::{Report, ResultExt};
use serde::{Deserialize, Serialize};
use std::error::Error;
use tracing::warn;

const TLS_COOLDOWN: Duration = Duration::hours(24);
/// 本次实测吞吐低于「本机见过的最好线路」的这一分之一即判劣化。分母而非绝对值，是为了让
/// 千兆机器和家宽机器共用一套阈值。
pub(crate) const SLOW_RATIO: f64 = 4.0;
const SLOW_COOLDOWN: Duration = Duration::minutes(30);
pub const SLOW_THROUGHPUT: &str = "slow_throughput";
const PROBE_LEASE: Duration = Duration::minutes(5);
const MAX_ERROR_LEN: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadFailureKind {
    CertificateExpired,
    CertificateInvalid,
    ConnectTimeout,
    RequestTimeout,
    HttpStatus,
    RateLimit601,
    Transport,
}

impl UploadFailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CertificateExpired => "certificate_expired",
            Self::CertificateInvalid => "certificate_invalid",
            Self::ConnectTimeout => "connect_timeout",
            Self::RequestTimeout => "request_timeout",
            Self::HttpStatus => "http_status",
            Self::RateLimit601 => "rate_limit_601",
            Self::Transport => "transport",
        }
    }

    fn is_tls(self) -> bool {
        matches!(self, Self::CertificateExpired | Self::CertificateInvalid)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct UploadLineHealth {
    pub line_key: String,
    pub consecutive_failures: i64,
    pub cooldown_until: Option<DateTime<Utc>>,
    pub last_failure_kind: Option<String>,
    pub last_error: Option<String>,
    pub avg_mbps: Option<f64>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineAvailability {
    Available,
    Cooling {
        until: DateTime<Utc>,
        reason: Option<String>,
    },
}

fn classify_text(text: &str) -> Option<UploadFailureKind> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("certificate has expired")
        || lower.contains("certificate expired")
        || lower.contains("cert has expired")
    {
        Some(UploadFailureKind::CertificateExpired)
    } else if lower.contains("certificate")
        || lower.contains("unknown issuer")
        || lower.contains("invalid peer certificate")
        || lower.contains("certvalid")
    {
        Some(UploadFailureKind::CertificateInvalid)
    } else if lower.contains("connect") && lower.contains("timed out") {
        Some(UploadFailureKind::ConnectTimeout)
    } else if lower.contains("timed out") || lower.contains("timeout") {
        Some(UploadFailureKind::RequestTimeout)
    } else if lower.contains("http status") || lower.contains("http 4") || lower.contains("http 5")
    {
        Some(UploadFailureKind::HttpStatus)
    } else {
        None
    }
}

fn classify_error(error: &(dyn Error + 'static)) -> UploadFailureKind {
    let mut current = Some(error);
    while let Some(source) = current {
        if let Some(reqwest) = source.downcast_ref::<reqwest::Error>() {
            if reqwest.is_connect() && reqwest.is_timeout() {
                return UploadFailureKind::ConnectTimeout;
            }
            if reqwest.is_timeout() {
                return UploadFailureKind::RequestTimeout;
            }
            if reqwest.status().is_some() {
                return UploadFailureKind::HttpStatus;
            }
        }
        if let Some(kind) = classify_text(&source.to_string()) {
            return kind;
        }
        current = source.source();
    }
    UploadFailureKind::Transport
}

pub fn classify_kind(error: &Kind) -> UploadFailureKind {
    match error {
        Kind::RateLimit { code: 601, .. } => UploadFailureKind::RateLimit601,
        Kind::Reqwest(error) => classify_error(error),
        Kind::ReqwestMiddleware(error) => classify_error(error),
        Kind::Custom(message) => classify_text(message).unwrap_or(UploadFailureKind::Transport),
        _ => classify_error(error),
    }
}

pub fn classify_report(error: &Report<AppError>) -> UploadFailureKind {
    if let Some(kind) = error.downcast_ref::<Kind>() {
        return classify_kind(kind);
    }
    classify_text(&format!("{error:?}")).unwrap_or(UploadFailureKind::Transport)
}

pub fn sanitized_error_summary(error: &Report<AppError>) -> String {
    sanitize_error(&format!("{error:?}"))
}

pub fn sanitize_error(raw: &str) -> String {
    let compact = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut words = Vec::new();
    let mut redact_next = false;
    for word in compact.split(' ') {
        if redact_next {
            words.push("[redacted]");
            redact_next = false;
            continue;
        }
        let lower = word.to_ascii_lowercase();
        if lower.starts_with("cookie") || lower.contains("x-upos-auth") {
            words.push("[redacted]");
            redact_next = true;
            continue;
        }
        if let Some((base, _)) = word.split_once('?') {
            words.push(base);
        } else {
            words.push(word);
        }
    }
    words.join(" ").chars().take(MAX_ERROR_LEN).collect()
}

fn ordinary_cooldown(failures: i64) -> Duration {
    match failures {
        0 | 1 => Duration::minutes(1),
        2 => Duration::minutes(5),
        3 => Duration::minutes(15),
        _ => Duration::hours(1),
    }
}

/// Atomically reserves a line whose cooldown has elapsed. The short lease makes the first
/// post-cooldown request the only probe across concurrent workers and processes.
pub async fn acquire_line(
    pool: &ConnectionPool,
    line_key: &str,
    now: DateTime<Utc>,
) -> AppResult<LineAvailability> {
    let existing = sqlx::query_as::<_, UploadLineHealth>(
        "SELECT * FROM upload_line_health WHERE line_key = ?",
    )
    .bind(line_key)
    .fetch_optional(pool)
    .await
    .change_context(AppError::Unknown)?;
    let Some(existing) = existing else {
        return Ok(LineAvailability::Available);
    };
    if let Some(until) = existing.cooldown_until {
        if until > now {
            return Ok(LineAvailability::Cooling {
                until,
                reason: existing.last_failure_kind,
            });
        }
        let reserved_until = now + PROBE_LEASE;
        let updated = sqlx::query(
            "UPDATE upload_line_health SET cooldown_until = ?1, updated_at = ?2 \
             WHERE line_key = ?3 AND cooldown_until = ?4",
        )
        .bind(reserved_until)
        .bind(now)
        .bind(line_key)
        .bind(until)
        .execute(pool)
        .await
        .change_context(AppError::Unknown)?;
        if updated.rows_affected() == 0 {
            return Ok(LineAvailability::Cooling {
                until: reserved_until,
                reason: Some("probe_in_progress".to_string()),
            });
        }
    }
    Ok(LineAvailability::Available)
}

pub async fn active_cooldowns(
    pool: &ConnectionPool,
    now: DateTime<Utc>,
) -> AppResult<Vec<UploadLineHealth>> {
    sqlx::query_as::<_, UploadLineHealth>(
        "SELECT * FROM upload_line_health WHERE cooldown_until > ? ORDER BY line_key",
    )
    .bind(now)
    .fetch_all(pool)
    .await
    .change_context(AppError::Unknown)
}

pub async fn all_health(pool: &ConnectionPool) -> AppResult<Vec<UploadLineHealth>> {
    sqlx::query_as::<_, UploadLineHealth>("SELECT * FROM upload_line_health ORDER BY line_key")
        .fetch_all(pool)
        .await
        .change_context(AppError::Unknown)
}

/// 本机见过的最好线路吞吐，是所有「慢」判据的分母。空库返回 `None`，此时判据整个关闭。
pub async fn baseline_mbps(pool: &ConnectionPool) -> AppResult<Option<f64>> {
    sqlx::query_scalar("SELECT MAX(avg_mbps) FROM upload_line_health")
        .fetch_one(pool)
        .await
        .change_context(AppError::Unknown)
}

/// 记录一次成功传输。`mbps` 是纯网络阶段的实测吞吐；非正数或非有限值视为「没测到」，
/// 此时行为与旧版逐字段一致（清零、清冷却、保留既有 EWMA）。
///
/// 传完了但很慢同样是线路问题，所以这里会把慢线路按短冷却挂起——复用 `cooldown_until`，
/// 选路侧不需要第二套排除逻辑。慢不是失败，`consecutive_failures` 保持 0，不去污染
/// `ordinary_cooldown` 的失败梯度。
pub async fn record_success(
    pool: &ConnectionPool,
    line_key: &str,
    mbps: f64,
    now: DateTime<Utc>,
) -> AppResult<()> {
    let measured = (mbps.is_finite() && mbps > 0.0).then_some(mbps);
    let mut avg_mbps = None;
    let mut cooldown_until = None;
    let mut failure_kind = None;
    let mut last_error = None;
    if let Some(mbps) = measured {
        let previous: Option<f64> =
            sqlx::query_scalar("SELECT avg_mbps FROM upload_line_health WHERE line_key = ?")
                .bind(line_key)
                .fetch_optional(pool)
                .await
                .change_context(AppError::Unknown)?
                .flatten();
        // 判慢用本次实测值而不是 EWMA：一次劣化不该被历史稀释掉。
        avg_mbps = Some(previous.map_or(mbps, |old| old * 0.7 + mbps * 0.3));
        let baseline = baseline_mbps(pool).await?;
        // 基线为空是冷启动：一条样本都没有时任何判据都是瞎猜，整个关掉。
        if let Some(baseline) = baseline.filter(|value| mbps < value / SLOW_RATIO) {
            if strands_recoverable_lines(pool, line_key, now).await? {
                warn!(
                    line = line_key,
                    mbps, baseline, "线路吞吐劣化，但冷却它会让可取回线路全部挂起，本次只更新均值"
                );
            } else {
                cooldown_until = Some(now + SLOW_COOLDOWN);
                failure_kind = Some(SLOW_THROUGHPUT);
                last_error = Some(format!(
                    "throughput {mbps:.2} MB/s < baseline {baseline:.2}/{SLOW_RATIO:.0} MB/s"
                ));
            }
        }
    }
    sqlx::query(
        "INSERT INTO upload_line_health \
         (line_key, consecutive_failures, cooldown_until, last_failure_kind, last_error, avg_mbps, updated_at) \
         VALUES (?1, 0, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT(line_key) DO UPDATE SET consecutive_failures = 0, \
             cooldown_until = excluded.cooldown_until, last_failure_kind = excluded.last_failure_kind, \
             last_error = excluded.last_error, \
             avg_mbps = coalesce(excluded.avg_mbps, upload_line_health.avg_mbps), \
             updated_at = excluded.updated_at",
    )
    .bind(line_key)
    .bind(cooldown_until)
    .bind(failure_kind)
    .bind(last_error)
    .bind(avg_mbps)
    .bind(now)
    .execute(pool)
    .await
    .change_context(AppError::Unknown)?;
    Ok(())
}

/// 冷却一条慢线路的代价是它退出候选集；把可取回线路全冷却掉，选路会退回不受限探测，
/// 落到没有灾后取回通道的线路上。传得慢比丢失取回通道轻，所以这里让路。
async fn strands_recoverable_lines(
    pool: &ConnectionPool,
    line_key: &str,
    now: DateTime<Utc>,
) -> AppResult<bool> {
    if !RECOVERABLE_LINES.contains(&line_key) {
        return Ok(false);
    }
    let cooling = active_cooldowns(pool, now).await?;
    Ok(RECOVERABLE_LINES
        .iter()
        .all(|key| *key == line_key || cooling.iter().any(|row| row.line_key == *key)))
}

/// Returns true only when this failure opened a fresh TLS breaker (used to de-duplicate alerts).
pub async fn record_failure(
    pool: &ConnectionPool,
    line_key: &str,
    kind: UploadFailureKind,
    summary: &str,
    now: DateTime<Utc>,
) -> AppResult<bool> {
    if kind == UploadFailureKind::RateLimit601 {
        return Ok(false);
    }
    let previous = sqlx::query_as::<_, UploadLineHealth>(
        "SELECT * FROM upload_line_health WHERE line_key = ?",
    )
    .bind(line_key)
    .fetch_optional(pool)
    .await
    .change_context(AppError::Unknown)?;
    let failures = previous
        .as_ref()
        .map_or(1, |row| row.consecutive_failures + 1);
    let already_open = previous
        .as_ref()
        .and_then(|row| row.cooldown_until)
        .is_some_and(|until| until > now);
    let cooldown_until = now
        + if kind.is_tls() {
            TLS_COOLDOWN
        } else {
            ordinary_cooldown(failures)
        };
    let summary: String = summary.chars().take(MAX_ERROR_LEN).collect();
    sqlx::query(
        "INSERT INTO upload_line_health \
         (line_key, consecutive_failures, cooldown_until, last_failure_kind, last_error, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT(line_key) DO UPDATE SET consecutive_failures = excluded.consecutive_failures, \
             cooldown_until = excluded.cooldown_until, last_failure_kind = excluded.last_failure_kind, \
             last_error = excluded.last_error, updated_at = excluded.updated_at",
    )
    .bind(line_key)
    .bind(failures)
    .bind(cooldown_until)
    .bind(kind.as_str())
    .bind(summary)
    .bind(now)
    .execute(pool)
    .await
    .change_context(AppError::Unknown)?;
    Ok(kind.is_tls() && !already_open)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::infrastructure::connection_pool::test_support::migrated_pool;
    use chrono::TimeZone;

    #[test]
    fn classifies_nested_certificate_and_redacts_query() {
        let error = Kind::Custom("request failed: certificate has expired".to_string());
        assert_eq!(classify_kind(&error), UploadFailureKind::CertificateExpired);
        let clean =
            sanitize_error("https://host/upload?upload_id=secret X-Upos-Auth: token Cookie: sid");
        assert!(!clean.contains("secret"));
        assert!(!clean.to_ascii_lowercase().contains("token"));
        assert!(!clean.to_ascii_lowercase().contains("sid"));
    }

    #[tokio::test]
    async fn tls_breaker_survives_pool_reopen_and_only_one_probe_is_reserved() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("line-health.db");
        let pool = crate::server::infrastructure::connection_pool::ConnectionManager::new_pool(
            path.to_str().unwrap(),
        )
        .await
        .unwrap();
        let now = Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 0).unwrap();
        assert!(
            record_failure(
                &pool,
                "bldsa",
                UploadFailureKind::CertificateExpired,
                "expired",
                now
            )
            .await
            .unwrap()
        );
        assert!(matches!(
            acquire_line(&pool, "bldsa", now).await.unwrap(),
            LineAvailability::Cooling { .. }
        ));
        pool.close().await;
        let pool = crate::server::infrastructure::connection_pool::ConnectionManager::new_pool(
            path.to_str().unwrap(),
        )
        .await
        .unwrap();
        assert!(matches!(
            acquire_line(&pool, "bldsa", now).await.unwrap(),
            LineAvailability::Cooling { .. }
        ));
        let after = now + Duration::hours(24);
        assert_eq!(
            acquire_line(&pool, "bldsa", after).await.unwrap(),
            LineAvailability::Available
        );
        assert!(matches!(
            acquire_line(&pool, "bldsa", after).await.unwrap(),
            LineAvailability::Cooling { .. }
        ));
    }

    #[tokio::test]
    async fn rate_limit_does_not_open_line_breaker() {
        let (_dir, pool) = migrated_pool().await;
        record_failure(
            &pool,
            "bda2",
            UploadFailureKind::RateLimit601,
            "601",
            Utc::now(),
        )
        .await
        .unwrap();
        assert!(all_health(&pool).await.unwrap().is_empty());
    }

    async fn row(pool: &ConnectionPool, line_key: &str) -> UploadLineHealth {
        all_health(pool)
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.line_key == line_key)
            .expect("线路缺行")
    }

    #[tokio::test]
    async fn cold_start_records_average_without_judging() {
        let (_dir, pool) = migrated_pool().await;
        let now = Utc.with_ymd_and_hms(2026, 9, 4, 12, 0, 0).unwrap();
        record_success(&pool, "tx", 2.3, now).await.unwrap();
        let row = row(&pool, "tx").await;
        assert_eq!(row.avg_mbps, Some(2.3));
        assert!(row.cooldown_until.is_none());
        assert!(row.last_failure_kind.is_none());
    }

    #[tokio::test]
    async fn slow_transfer_cools_the_line_without_counting_as_failure() {
        let (_dir, pool) = migrated_pool().await;
        let now = Utc.with_ymd_and_hms(2026, 9, 4, 12, 0, 0).unwrap();
        record_success(&pool, "tx", 26.0, now).await.unwrap();
        record_success(&pool, "bda2", 2.3, now).await.unwrap();
        let row = row(&pool, "bda2").await;
        assert_eq!(row.cooldown_until, Some(now + SLOW_COOLDOWN));
        assert_eq!(row.last_failure_kind.as_deref(), Some(SLOW_THROUGHPUT));
        assert_eq!(row.consecutive_failures, 0);
        assert!(row.last_error.unwrap().contains("2.30 MB/s"));
    }

    /// 8.55 对 26 的基线不判慢（26/4 = 6.5）。这是刻意的：先按 1/4 上线保不误伤，
    /// 要不要收紧到 1/3 等生产数据说话。
    #[tokio::test]
    async fn moderate_slowdown_is_left_alone() {
        let (_dir, pool) = migrated_pool().await;
        let now = Utc.with_ymd_and_hms(2026, 9, 4, 12, 0, 0).unwrap();
        record_success(&pool, "tx", 26.0, now).await.unwrap();
        record_success(&pool, "bda2", 8.55, now).await.unwrap();
        let row = row(&pool, "bda2").await;
        assert!(row.cooldown_until.is_none());
        assert_eq!(row.avg_mbps, Some(8.55));
    }

    #[tokio::test]
    async fn last_recoverable_line_is_not_cooled_for_being_slow() {
        let (_dir, pool) = migrated_pool().await;
        let now = Utc.with_ymd_and_hms(2026, 9, 4, 12, 0, 0).unwrap();
        record_success(&pool, "tx", 26.0, now).await.unwrap();
        for line in ["bda2", "alia"] {
            record_failure(&pool, line, UploadFailureKind::Transport, "boom", now)
                .await
                .unwrap();
        }
        record_success(&pool, "tx", 2.3, now).await.unwrap();
        let row = row(&pool, "tx").await;
        assert!(row.cooldown_until.is_none());
        assert!(row.avg_mbps.unwrap() < 26.0);
    }

    #[tokio::test]
    async fn unmeasured_success_keeps_the_old_clearing_semantics() {
        let (_dir, pool) = migrated_pool().await;
        let now = Utc.with_ymd_and_hms(2026, 9, 4, 12, 0, 0).unwrap();
        record_success(&pool, "tx", 26.0, now).await.unwrap();
        record_failure(&pool, "tx", UploadFailureKind::Transport, "boom", now)
            .await
            .unwrap();
        record_success(&pool, "tx", f64::NAN, now).await.unwrap();
        let row = row(&pool, "tx").await;
        assert!(row.cooldown_until.is_none());
        assert!(row.last_failure_kind.is_none());
        assert!(row.last_error.is_none());
        assert_eq!(row.consecutive_failures, 0);
        assert_eq!(row.avg_mbps, Some(26.0));
    }
}
