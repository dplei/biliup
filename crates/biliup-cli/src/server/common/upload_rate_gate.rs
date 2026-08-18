use crate::server::config::Config;
use crate::server::errors::{AppError, AppResult};
use crate::server::infrastructure::connection_pool::ConnectionPool;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use error_stack::ResultExt;
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify};
use tracing::{info, warn};

#[derive(Debug, Clone, Copy)]
pub struct UploadRateGateSettings {
    pub enabled: bool,
    pub min_request_interval: Duration,
    pub initial_cooldown: Duration,
    pub max_cooldown: Duration,
}

impl From<&Config> for UploadRateGateSettings {
    fn from(config: &Config) -> Self {
        Self {
            enabled: config.upload_rate_gate_enabled,
            min_request_interval: Duration::from_secs(config.upload_min_request_interval_secs),
            initial_cooldown: Duration::from_secs(config.upload_601_initial_cooldown_secs),
            max_cooldown: Duration::from_secs(config.upload_601_max_cooldown_secs),
        }
    }
}

#[derive(Debug)]
enum UploadGateState {
    Ready,
    CoolingDown { until: DateTime<Utc>, strikes: u32 },
    Probing { strikes: u32 },
}

#[derive(Debug)]
struct Runtime {
    loaded: bool,
    state: UploadGateState,
    last_request_at: Option<Instant>,
    waiting: u64,
    rate_limit_count: u64,
    pre_upload_count: u64,
    pre_upload_times: VecDeque<Instant>,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            loaded: false,
            state: UploadGateState::Ready,
            last_request_at: None,
            waiting: 0,
            rate_limit_count: 0,
            pre_upload_count: 0,
            pre_upload_times: VecDeque::new(),
        }
    }
}

static RUNTIME: LazyLock<Mutex<Runtime>> = LazyLock::new(|| Mutex::new(Runtime::default()));
static CHANGED: LazyLock<Notify> = LazyLock::new(Notify::new);

async fn load_once(pool: &ConnectionPool) -> AppResult<()> {
    let mut runtime = RUNTIME.lock().await;
    if runtime.loaded {
        return Ok(());
    }
    let row = sqlx::query_as::<_, (Option<DateTime<Utc>>, i64)>(
        "SELECT cooldown_until, strikes FROM upload_rate_gate WHERE id = 1",
    )
    .fetch_optional(pool)
    .await
    .change_context(AppError::Unknown)?;
    if let Some((Some(until), strikes)) = row
        && until > Utc::now()
    {
        runtime.state = UploadGateState::CoolingDown {
            until,
            strikes: u32::try_from(strikes).unwrap_or(u32::MAX),
        };
        info!(%until, strikes, "restored Bilibili upload cooldown from database");
    }
    runtime.loaded = true;
    Ok(())
}

/// Wait until a single process-wide pre-upload attempt is allowed.
///
/// Existing upload call sites also hold the global upload semaphore, so the `Probing` state
/// reserves the first attempt after a cooldown without allowing a second task through.
pub async fn before_pre_upload(
    settings: UploadRateGateSettings,
    pool: &ConnectionPool,
) -> AppResult<()> {
    if !settings.enabled {
        return Ok(());
    }
    load_once(pool).await?;
    let mut counted_waiter = false;
    loop {
        let mut runtime = RUNTIME.lock().await;
        match runtime.state {
            UploadGateState::Ready => {
                let wait = runtime
                    .last_request_at
                    .map(|last| settings.min_request_interval.saturating_sub(last.elapsed()))
                    .unwrap_or_default();
                if wait.is_zero() {
                    runtime.last_request_at = Some(Instant::now());
                    runtime.pre_upload_count = runtime.pre_upload_count.saturating_add(1);
                    prune_pre_upload_times(&mut runtime);
                    runtime.pre_upload_times.push_back(Instant::now());
                    if counted_waiter {
                        runtime.waiting = runtime.waiting.saturating_sub(1);
                    }
                    return Ok(());
                }
                if !counted_waiter {
                    runtime.waiting = runtime.waiting.saturating_add(1);
                    counted_waiter = true;
                }
                drop(runtime);
                tokio::time::sleep(wait).await;
            }
            UploadGateState::CoolingDown { until, strikes } => {
                let now = Utc::now();
                if until <= now {
                    runtime.state = UploadGateState::Probing { strikes };
                    runtime.last_request_at = Some(Instant::now());
                    runtime.pre_upload_count = runtime.pre_upload_count.saturating_add(1);
                    prune_pre_upload_times(&mut runtime);
                    runtime.pre_upload_times.push_back(Instant::now());
                    if counted_waiter {
                        runtime.waiting = runtime.waiting.saturating_sub(1);
                    }
                    info!(
                        strikes,
                        "upload cooldown ended; allowing one probe pre-upload"
                    );
                    return Ok(());
                }
                if !counted_waiter {
                    runtime.waiting = runtime.waiting.saturating_add(1);
                    counted_waiter = true;
                }
                let wait = (until - now)
                    .to_std()
                    .unwrap_or_else(|_| Duration::from_secs(1));
                drop(runtime);
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {},
                    _ = CHANGED.notified() => {},
                }
            }
            UploadGateState::Probing { .. } => {
                if !counted_waiter {
                    runtime.waiting = runtime.waiting.saturating_add(1);
                    counted_waiter = true;
                }
                drop(runtime);
                // `Notify::notify_waiters` does not retain a permit. A bounded poll avoids a
                // lost-wakeup race between dropping the mutex and registering the waiter,
                // while still keeping the queue asleep rather than spinning.
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

pub async fn record_success(settings: UploadRateGateSettings, pool: &ConnectionPool) {
    if !settings.enabled {
        return;
    }
    let was_probing = {
        let mut runtime = RUNTIME.lock().await;
        let probing = matches!(runtime.state, UploadGateState::Probing { .. });
        if probing {
            runtime.state = UploadGateState::Ready;
        }
        probing
    };
    if was_probing {
        if let Err(error) = sqlx::query(
            "UPDATE upload_rate_gate SET cooldown_until = NULL, strikes = 0, updated_at = ?1 WHERE id = 1",
        )
        .bind(Utc::now())
        .execute(pool)
        .await
        {
            warn!(?error, "failed to persist upload gate recovery");
        }
        info!("Bilibili upload probe succeeded; global gate is ready");
        CHANGED.notify_waiters();
    }
}

pub async fn record_non_rate_limit_failure(settings: UploadRateGateSettings) {
    if !settings.enabled {
        return;
    }
    let changed = {
        let mut runtime = RUNTIME.lock().await;
        if matches!(runtime.state, UploadGateState::Probing { .. }) {
            runtime.state = UploadGateState::Ready;
            true
        } else {
            false
        }
    };
    if changed {
        CHANGED.notify_waiters();
    }
}

pub async fn record_rate_limited(
    settings: UploadRateGateSettings,
    pool: &ConnectionPool,
) -> AppResult<DateTime<Utc>> {
    if !settings.enabled {
        return Ok(Utc::now());
    }
    let (until, strikes) = {
        let mut runtime = RUNTIME.lock().await;
        let previous = match runtime.state {
            UploadGateState::CoolingDown { strikes, .. } | UploadGateState::Probing { strikes } => {
                strikes
            }
            UploadGateState::Ready => 0,
        };
        let strikes = previous.saturating_add(1);
        let multiplier = 2_u32.saturating_pow(strikes.saturating_sub(1).min(20));
        let cooldown = settings
            .initial_cooldown
            .saturating_mul(multiplier)
            .min(settings.max_cooldown);
        let until = Utc::now()
            + ChronoDuration::from_std(cooldown)
                .unwrap_or_else(|_| ChronoDuration::seconds(i64::MAX / 4));
        runtime.state = UploadGateState::CoolingDown { until, strikes };
        runtime.rate_limit_count = runtime.rate_limit_count.saturating_add(1);
        (until, strikes)
    };
    sqlx::query(
        "INSERT INTO upload_rate_gate (id, last_601_at, cooldown_until, strikes, updated_at) \
         VALUES (1, ?1, ?2, ?3, ?1) \
         ON CONFLICT(id) DO UPDATE SET last_601_at = excluded.last_601_at, \
         cooldown_until = excluded.cooldown_until, strikes = excluded.strikes, updated_at = excluded.updated_at",
    )
    .bind(Utc::now())
    .bind(until)
    .bind(i64::from(strikes))
    .execute(pool)
    .await
    .change_context(AppError::Unknown)?;
    warn!(%until, strikes, "Bilibili returned 601; global upload cooldown started");
    CHANGED.notify_waiters();
    Ok(until)
}

#[derive(Serialize)]
pub struct UploadGateSnapshot {
    pub state: &'static str,
    pub strikes: u32,
    pub cooldown_remaining_secs: u64,
    pub waiting: u64,
    pub rate_limit_count: u64,
    pub pre_upload_count: u64,
    pub pre_upload_last_minute: u64,
}

pub async fn snapshot() -> UploadGateSnapshot {
    let mut runtime = RUNTIME.lock().await;
    prune_pre_upload_times(&mut runtime);
    let (state, strikes, remaining) = match runtime.state {
        UploadGateState::Ready => ("ready", 0, 0),
        UploadGateState::Probing { strikes } => ("probing", strikes, 0),
        UploadGateState::CoolingDown { until, strikes } => (
            "cooling_down",
            strikes,
            (until - Utc::now()).num_seconds().max(0) as u64,
        ),
    };
    UploadGateSnapshot {
        state,
        strikes,
        cooldown_remaining_secs: remaining,
        waiting: runtime.waiting,
        rate_limit_count: runtime.rate_limit_count,
        pre_upload_count: runtime.pre_upload_count,
        pre_upload_last_minute: runtime.pre_upload_times.len() as u64,
    }
}

fn prune_pre_upload_times(runtime: &mut Runtime) {
    while runtime
        .pre_upload_times
        .front()
        .is_some_and(|instant| instant.elapsed() >= Duration::from_secs(60))
    {
        runtime.pre_upload_times.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cooldown_grows_exponentially_and_is_capped() {
        let initial = Duration::from_secs(60);
        let max = Duration::from_secs(300);
        let values: Vec<_> = (1_u32..=5)
            .map(|strike| {
                initial
                    .saturating_mul(2_u32.saturating_pow(strike - 1))
                    .min(max)
            })
            .collect();
        assert_eq!(values, [60, 120, 240, 300, 300].map(Duration::from_secs));
    }
}
