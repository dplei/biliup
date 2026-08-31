//! Native structured events for the recording chain.
//!
//! Every emitter here writes to the `biliup::event` target, which the old sinks filter out, so
//! old files and console output are unchanged. Identity is passed explicitly — no ambient span
//! context, no parsing of old message text.

use biliup::downloader::util::RecordingOwner;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// The routing target that separates native events from the old sinks.
pub const EVENT_TARGET: &str = "biliup::event";

/// Business identity of one recording session, built once and cloned into every callback,
/// channel message and blocking worker that needs it.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RecordingIdentity {
    pub live_streamer_id: Option<String>,
    pub streamer_info_id: Option<String>,
    pub task_id: Option<String>,
    pub streamer_name: Option<String>,
}

impl RecordingIdentity {
    pub fn server(live_streamer_id: i64, streamer_info_id: i64, streamer_name: &str) -> Self {
        Self {
            live_streamer_id: Some(live_streamer_id.to_string()),
            streamer_info_id: Some(streamer_info_id.to_string()),
            task_id: None,
            streamer_name: Some(streamer_name.to_owned()),
        }
    }

    /// Standalone commands have no room or session row; they identify themselves by task id and
    /// must not borrow a streamer identity they do not have.
    pub fn task(task_id: &str) -> Self {
        Self {
            live_streamer_id: None,
            streamer_info_id: None,
            task_id: Some(task_id.to_owned()),
            streamer_name: None,
        }
    }

    pub fn owner(&self, download_attempt_id: Option<&str>) -> RecordingOwner {
        RecordingOwner {
            live_streamer_id: self.live_streamer_id.clone(),
            streamer_info_id: self.streamer_info_id.clone(),
            task_id: self.task_id.clone(),
            download_attempt_id: download_attempt_id.map(ToOwned::to_owned),
        }
    }

    fn live_streamer_id(&self) -> &str {
        self.live_streamer_id.as_deref().unwrap_or("")
    }
    fn streamer_info_id(&self) -> &str {
        self.streamer_info_id.as_deref().unwrap_or("")
    }
    fn task_id(&self) -> &str {
        self.task_id.as_deref().unwrap_or("")
    }
    fn streamer_name(&self) -> &str {
        self.streamer_name.as_deref().unwrap_or("")
    }
}

pub fn recording_started(identity: &RecordingIdentity, reason_code: &str, attempt: Option<&str>) {
    info!(
        target: EVENT_TARGET,
        event_name = "recording.started",
        outcome = "executed",
        reason_code,
        live_streamer_id = identity.live_streamer_id(),
        streamer_info_id = identity.streamer_info_id(),
        task_id = identity.task_id(),
        streamer_name = identity.streamer_name(),
        download_attempt_id = attempt.unwrap_or(""),
        "开始录制本场"
    );
}

/// `outcome` is the recording result, not the level: a cancelled recording is not an error.
pub fn recording_stopped(identity: &RecordingIdentity, outcome: &str, reason_code: &str) {
    if outcome == "failed" {
        warn!(
            target: EVENT_TARGET,
            event_name = "recording.stopped",
            outcome,
            reason_code,
            live_streamer_id = identity.live_streamer_id(),
            streamer_info_id = identity.streamer_info_id(),
            task_id = identity.task_id(),
            streamer_name = identity.streamer_name(),
            "本场录制异常结束"
        );
    } else {
        info!(
            target: EVENT_TARGET,
            event_name = "recording.stopped",
            outcome,
            reason_code,
            live_streamer_id = identity.live_streamer_id(),
            streamer_info_id = identity.streamer_info_id(),
            task_id = identity.task_id(),
            streamer_name = identity.streamer_name(),
            "本场录制结束"
        );
    }
}

/// Scheduled, not yet performed: the outcome stays `waiting` until a later connection succeeds.
pub fn retry_scheduled(
    identity: &RecordingIdentity,
    reason_code: &str,
    delay_ms: u64,
    attempt: Option<&str>,
) {
    warn!(
        target: EVENT_TARGET,
        event_name = "recording.retry_scheduled",
        outcome = "waiting",
        reason_code,
        delay_ms,
        live_streamer_id = identity.live_streamer_id(),
        streamer_info_id = identity.streamer_info_id(),
        task_id = identity.task_id(),
        download_attempt_id = attempt.unwrap_or(""),
        "等待重连"
    );
}

/// `gap_ms` is the three part estimate from the reconnect loop; `silent_measured` says whether the
/// silent half was actually measured, so an estimate is never read as a measurement.
pub fn reconnected(
    identity: &RecordingIdentity,
    gap_ms: u64,
    silent_ms: u64,
    silent_measured: bool,
    attempt: Option<&str>,
) {
    info!(
        target: EVENT_TARGET,
        event_name = "recording.reconnected",
        outcome = "recovered",
        reason_code = if silent_measured { "measured_gap" } else { "estimated_gap" },
        gap_ms,
        silent_ms,
        live_streamer_id = identity.live_streamer_id(),
        streamer_info_id = identity.streamer_info_id(),
        task_id = identity.task_id(),
        download_attempt_id = attempt.unwrap_or(""),
        "重连成功，已记录缺口"
    );
}

/// Enrollment is the ledger boundary: the segment identity assigned at file creation is the same
/// one reported here, so a segment can be followed before it has any missing/session id.
#[allow(clippy::too_many_arguments)]
pub fn segment_enrolled(
    identity: &RecordingIdentity,
    segment_id: &str,
    original_file: &str,
    outcome: &str,
    reason_code: &str,
    upload_session_id: Option<i64>,
    missing_id: Option<i64>,
    segment_order: Option<i64>,
) {
    let session = upload_session_id
        .map(|id| id.to_string())
        .unwrap_or_default();
    let missing = missing_id.map(|id| id.to_string()).unwrap_or_default();
    let order = segment_order.unwrap_or_default().max(0) as u64;
    if outcome == "executed" {
        info!(
            target: EVENT_TARGET,
            event_name = "recording.segment_enrolled",
            outcome,
            reason_code,
            segment_id,
            original_file,
            upload_session_id = session,
            missing_id = missing,
            segment_order = order,
            live_streamer_id = identity.live_streamer_id(),
            streamer_info_id = identity.streamer_info_id(),
            task_id = identity.task_id(),
            "分段已登记进上传账本"
        );
    } else {
        warn!(
            target: EVENT_TARGET,
            event_name = "recording.segment_enrolled",
            outcome,
            reason_code,
            segment_id,
            original_file,
            upload_session_id = session,
            live_streamer_id = identity.live_streamer_id(),
            streamer_info_id = identity.streamer_info_id(),
            task_id = identity.task_id(),
            "分段未进入上传账本"
        );
    }
}
