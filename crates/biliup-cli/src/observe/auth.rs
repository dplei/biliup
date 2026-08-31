//! Credential health, kept apart from the business result of any one call.
//!
//! A single failed operation is not proof of an expired credential: only the health state
//! machine in `cookie_health` decides that a platform is unhealthy or has recovered, and only
//! its two transitions produce `auth.health_changed`. Error text never travels with these
//! events — it may carry a signed URL or a cookie value, and the old sink already keeps it.

use super::EVENT_TARGET;
use crate::server::common::cookie_health::{HealthErrorKind, classify_error};
use tracing::{info, warn};

/// The typed failure kind, so "what kind of failure" is answerable without the message.
pub fn reason_of(kind: HealthErrorKind) -> &'static str {
    match kind {
        HealthErrorKind::Authentication => "authentication_failed",
        HealthErrorKind::Transport => "transport_error",
        HealthErrorKind::Server => "server_error",
        HealthErrorKind::InvalidResponse => "invalid_response",
    }
}

/// Classify without storing: the text is read to pick a reason code and then dropped.
pub fn reason_for_error(error: &str) -> &'static str {
    reason_of(classify_error(error))
}

/// A state transition of the health machine, not a single call's result.
pub fn health_changed(platform: &str, outcome: &str, reason_code: &str, consecutive_errors: u32) {
    if outcome == "recovered" {
        info!(
            target: EVENT_TARGET,
            event_name = "auth.health_changed",
            outcome,
            reason_code,
            platform,
            count = u64::from(consecutive_errors),
            "平台凭据健康已恢复"
        );
    } else {
        warn!(
            target: EVENT_TARGET,
            event_name = "auth.health_changed",
            outcome,
            reason_code,
            platform,
            count = u64::from(consecutive_errors),
            "平台凭据连续鉴权失败，已判定为异常"
        );
    }
}

/// One classified failure of an operation that uses credentials. `stage` says where it was
/// observed; it never changes the health state on its own.
pub fn operation_failed(platform: &str, stage: &str, reason_code: &str) {
    warn!(
        target: EVENT_TARGET,
        event_name = "auth.operation_failed",
        outcome = "failed",
        reason_code,
        platform,
        stage,
        "认证相关操作失败，凭据健康未因此改变"
    );
}

/// Report a failed result without changing it, classifying the error into a typed reason.
///
/// The error is rendered with `Debug`, not `Display`: an `error_stack::Report` shows only its
/// top context when displayed, so classifying the displayed text would type every wrapped
/// failure as `invalid_response`. The rendering is read and dropped, never stored.
pub fn observe<T, E: std::fmt::Debug>(
    platform: &str,
    stage: &str,
    result: Result<T, E>,
) -> Result<T, E> {
    if let Err(error) = &result {
        operation_failed(platform, stage, reason_for_error(&format!("{error:?}")));
    }
    result
}
