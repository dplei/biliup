use crate::server::errors::{AppError, AppResult};
use crate::server::infrastructure::connection_pool::ConnectionPool;
use biliup::error::Kind;
use chrono::{DateTime, Duration, Utc};
use error_stack::{Report, ResultExt};
use serde::{Deserialize, Serialize};
use std::error::Error;

const TLS_COOLDOWN: Duration = Duration::hours(24);
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

pub async fn record_success(pool: &ConnectionPool, line_key: &str) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO upload_line_health \
         (line_key, consecutive_failures, cooldown_until, last_failure_kind, last_error, updated_at) \
         VALUES (?1, 0, NULL, NULL, NULL, ?2) \
         ON CONFLICT(line_key) DO UPDATE SET consecutive_failures = 0, cooldown_until = NULL, \
             last_failure_kind = NULL, last_error = NULL, updated_at = excluded.updated_at",
    )
    .bind(line_key)
    .bind(Utc::now())
    .execute(pool)
    .await
    .change_context(AppError::Unknown)?;
    Ok(())
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
}
