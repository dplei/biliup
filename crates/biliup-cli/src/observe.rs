//! Native structured events for recording, upload and submission chains.
//!
//! Every emitter here writes to the `biliup::event` target, which the old sinks filter out, so
//! old files and console output are unchanged. Identity is passed explicitly — no ambient span
//! context, no parsing of old message text.

pub mod auth;
pub mod external;
pub mod lifecycle;
pub mod standalone;

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

    /// Owned snapshot for emitters that write through the collector directly (attachments cannot
    /// travel as tracing fields). Same identity as the tracing path, no ambient context.
    pub fn context(&self, download_attempt_id: Option<&str>) -> biliup_observability::Context {
        biliup_observability::Context(
            biliup_observability::Fields::new()
                .with("live_streamer_id", self.live_streamer_id())
                .with("streamer_info_id", self.streamer_info_id())
                .with("task_id", self.task_id())
                .with("download_attempt_id", download_attempt_id.unwrap_or("")),
        )
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

/// Identity of one segment as it moves through preprocessing, upload, recovery and submission.
/// Every field is what the caller actually knows; nothing here is inferred from a file name.
#[derive(Debug, Clone, Default)]
pub struct UploadIdentity {
    pub task_id: Option<String>,
    pub live_streamer_id: Option<String>,
    pub streamer_info_id: Option<String>,
    pub upload_session_id: Option<String>,
    pub segment_id: Option<String>,
    pub missing_id: Option<String>,
    pub upload_attempt_id: Option<String>,
    pub original_file: Option<String>,
    pub segment_order: Option<i64>,
}

fn text(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("")
}

impl UploadIdentity {
    /// The durable ledger row is the authority: it already carries every id this segment owns.
    pub fn from_missing_row(
        row: &crate::server::infrastructure::models::UploadMissingSegment,
    ) -> Self {
        Self {
            task_id: None,
            live_streamer_id: Some(row.live_streamer_id.to_string()),
            streamer_info_id: Some(row.streamer_info_id.to_string()),
            upload_session_id: row.upload_session_id.map(|id| id.to_string()),
            segment_id: row.segment_id.clone(),
            missing_id: Some(row.id.to_string()),
            upload_attempt_id: row.attempt_token.clone(),
            original_file: Some(row.file_path.clone()),
            segment_order: Some(row.segment_order),
        }
    }

    pub fn from_enrollment(
        live_streamer_id: i64,
        streamer_info_id: i64,
        enrollment: &crate::server::core::downloader::SegmentEnrollment,
        original_file: &std::path::Path,
    ) -> Self {
        Self {
            task_id: None,
            live_streamer_id: Some(live_streamer_id.to_string()),
            streamer_info_id: Some(streamer_info_id.to_string()),
            upload_session_id: Some(enrollment.upload_session_id.to_string()),
            segment_id: enrollment.segment_id.clone(),
            missing_id: Some(enrollment.missing_id.to_string()),
            upload_attempt_id: None,
            original_file: Some(original_file.display().to_string()),
            segment_order: Some(enrollment.segment_order),
        }
    }

    /// Each attempt is its own identity; a retry never reuses the previous attempt's id.
    pub fn with_attempt(&self, attempt_token: &str) -> Self {
        Self {
            upload_attempt_id: Some(attempt_token.to_string()),
            ..self.clone()
        }
    }

    fn order(&self) -> u64 {
        self.segment_order.unwrap_or_default().max(0) as u64
    }
}

/// Preprocessing decided to run, skip or fall back. `stage` names the tool, so one segment can
/// carry several decisions without them overwriting each other.
pub fn processing_decided(
    identity: &UploadIdentity,
    stage: &str,
    outcome: &str,
    reason_code: &str,
) {
    info!(
        target: EVENT_TARGET,
        event_name = "processing.decided",
        outcome,
        reason_code,
        stage,
        segment_id = text(&identity.segment_id),
        original_file = text(&identity.original_file),
        upload_session_id = text(&identity.upload_session_id),
        missing_id = text(&identity.missing_id),
        upload_attempt_id = text(&identity.upload_attempt_id),
        task_id = text(&identity.task_id),
        live_streamer_id = text(&identity.live_streamer_id),
        streamer_info_id = text(&identity.streamer_info_id),
        "预处理决定"
    );
}

/// The result of a preprocessing stage that actually ran. A fallback is not an error by itself:
/// the level follows the business rule, the outcome carries the truth.
pub fn processing_completed(
    identity: &UploadIdentity,
    stage: &str,
    outcome: &str,
    reason_code: &str,
    artifact_file: Option<&str>,
    duration_ms: u64,
) {
    let artifact = artifact_file.unwrap_or("");
    if outcome == "failed" {
        warn!(
            target: EVENT_TARGET,
            event_name = "processing.completed",
            outcome,
            reason_code,
            stage,
            artifact_file = artifact,
            duration_ms,
            segment_id = text(&identity.segment_id),
            original_file = text(&identity.original_file),
            upload_session_id = text(&identity.upload_session_id),
            missing_id = text(&identity.missing_id),
            upload_attempt_id = text(&identity.upload_attempt_id),
            task_id = text(&identity.task_id),
            live_streamer_id = text(&identity.live_streamer_id),
            streamer_info_id = text(&identity.streamer_info_id),
            "预处理未完成"
        );
    } else {
        info!(
            target: EVENT_TARGET,
            event_name = "processing.completed",
            outcome,
            reason_code,
            stage,
            artifact_file = artifact,
            duration_ms,
            segment_id = text(&identity.segment_id),
            original_file = text(&identity.original_file),
            upload_session_id = text(&identity.upload_session_id),
            missing_id = text(&identity.missing_id),
            upload_attempt_id = text(&identity.upload_attempt_id),
            task_id = text(&identity.task_id),
            live_streamer_id = text(&identity.live_streamer_id),
            streamer_info_id = text(&identity.streamer_info_id),
            "预处理完成"
        );
    }
}

pub fn upload_queued(identity: &UploadIdentity, reason_code: &str) {
    info!(
        target: EVENT_TARGET,
        event_name = "upload.queued",
        outcome = "executed",
        reason_code,
        segment_id = text(&identity.segment_id),
        original_file = text(&identity.original_file),
        upload_session_id = text(&identity.upload_session_id),
        missing_id = text(&identity.missing_id),
        upload_attempt_id = text(&identity.upload_attempt_id),
        segment_order = identity.order(),
        task_id = text(&identity.task_id),
        live_streamer_id = text(&identity.live_streamer_id),
        streamer_info_id = text(&identity.streamer_info_id),
        "分段进入上传队列"
    );
}

/// Which line this attempt is on and why. `fallback` means the configured line was unusable.
pub fn upload_line_decided(
    identity: &UploadIdentity,
    line: &str,
    outcome: &str,
    reason_code: &str,
) {
    info!(
        target: EVENT_TARGET,
        event_name = "upload.line_decided",
        outcome,
        reason_code,
        line,
        segment_id = text(&identity.segment_id),
        original_file = text(&identity.original_file),
        upload_session_id = text(&identity.upload_session_id),
        missing_id = text(&identity.missing_id),
        upload_attempt_id = text(&identity.upload_attempt_id),
        task_id = text(&identity.task_id),
        segment_order = identity.order(),
        live_streamer_id = text(&identity.live_streamer_id),
        streamer_info_id = text(&identity.streamer_info_id),
        "上传线路已决定"
    );
}

pub fn upload_started(identity: &UploadIdentity, line: &str, total_bytes: u64) {
    info!(
        target: EVENT_TARGET,
        event_name = "upload.started",
        outcome = "executed",
        line,
        total_bytes,
        segment_id = text(&identity.segment_id),
        original_file = text(&identity.original_file),
        upload_session_id = text(&identity.upload_session_id),
        missing_id = text(&identity.missing_id),
        upload_attempt_id = text(&identity.upload_attempt_id),
        task_id = text(&identity.task_id),
        segment_order = identity.order(),
        live_streamer_id = text(&identity.live_streamer_id),
        streamer_info_id = text(&identity.streamer_info_id),
        "开始传输分段"
    );
}

/// A failed attempt stays failed. A later success is its own event and never rewrites this one.
pub fn upload_failed(identity: &UploadIdentity, reason_code: &str, error: &str) {
    warn!(
        target: EVENT_TARGET,
        event_name = "upload.failed",
        outcome = "failed",
        reason_code,
        error,
        segment_id = text(&identity.segment_id),
        original_file = text(&identity.original_file),
        upload_session_id = text(&identity.upload_session_id),
        missing_id = text(&identity.missing_id),
        upload_attempt_id = text(&identity.upload_attempt_id),
        task_id = text(&identity.task_id),
        segment_order = identity.order(),
        live_streamer_id = text(&identity.live_streamer_id),
        streamer_info_id = text(&identity.streamer_info_id),
        "分段上传失败"
    );
}

pub fn upload_completed(identity: &UploadIdentity, reason_code: &str, duration_ms: u64) {
    info!(
        target: EVENT_TARGET,
        event_name = "upload.completed",
        outcome = "succeeded",
        reason_code,
        duration_ms,
        segment_id = text(&identity.segment_id),
        original_file = text(&identity.original_file),
        upload_session_id = text(&identity.upload_session_id),
        missing_id = text(&identity.missing_id),
        upload_attempt_id = text(&identity.upload_attempt_id),
        segment_order = identity.order(),
        task_id = text(&identity.task_id),
        live_streamer_id = text(&identity.live_streamer_id),
        streamer_info_id = text(&identity.streamer_info_id),
        "分段上传完成"
    );
}

/// Why a recovery is allowed, refused or deferred. The original segment keeps its identity;
/// only the attempt is new.
pub fn recovery_decided(identity: &UploadIdentity, outcome: &str, reason_code: &str) {
    info!(
        target: EVENT_TARGET,
        event_name = "upload.recovery_decided",
        outcome,
        reason_code,
        segment_id = text(&identity.segment_id),
        original_file = text(&identity.original_file),
        upload_session_id = text(&identity.upload_session_id),
        missing_id = text(&identity.missing_id),
        task_id = text(&identity.task_id),
        segment_order = identity.order(),
        live_streamer_id = text(&identity.live_streamer_id),
        streamer_info_id = text(&identity.streamer_info_id),
        "补传资格判定"
    );
}

pub fn recovery_started(identity: &UploadIdentity, reason_code: &str) {
    info!(
        target: EVENT_TARGET,
        event_name = "upload.recovery_started",
        outcome = "executed",
        reason_code,
        segment_id = text(&identity.segment_id),
        original_file = text(&identity.original_file),
        upload_session_id = text(&identity.upload_session_id),
        missing_id = text(&identity.missing_id),
        upload_attempt_id = text(&identity.upload_attempt_id),
        task_id = text(&identity.task_id),
        segment_order = identity.order(),
        live_streamer_id = text(&identity.live_streamer_id),
        streamer_info_id = text(&identity.streamer_info_id),
        "开始补传"
    );
}

/// Submission identity is the upload session; the room may be unknown for a session recovered
/// by a background scan, and stays unknown rather than being guessed.
#[derive(Debug, Clone, Default)]
pub struct SubmissionIdentity {
    pub task_id: Option<String>,
    pub upload_session_id: Option<String>,
    pub live_streamer_id: Option<String>,
    pub streamer_info_id: Option<String>,
}

impl SubmissionIdentity {
    pub fn session(upload_session_id: i64) -> Self {
        Self {
            upload_session_id: Some(upload_session_id.to_string()),
            ..Default::default()
        }
    }
}

/// Waiting, refused or cleared to submit. `pending_count` is how many segments still block it.
pub fn submission_decided(
    identity: &SubmissionIdentity,
    outcome: &str,
    reason_code: &str,
    pending_count: u64,
) {
    info!(
        target: EVENT_TARGET,
        event_name = "submission.decided",
        outcome,
        reason_code,
        pending_count,
        upload_session_id = text(&identity.upload_session_id),
        task_id = text(&identity.task_id),
        live_streamer_id = text(&identity.live_streamer_id),
        streamer_info_id = text(&identity.streamer_info_id),
        "投稿判定"
    );
}

pub fn submission_started(identity: &SubmissionIdentity, reason_code: &str) {
    info!(
        target: EVENT_TARGET,
        event_name = "submission.started",
        outcome = "executed",
        reason_code,
        upload_session_id = text(&identity.upload_session_id),
        task_id = text(&identity.task_id),
        live_streamer_id = text(&identity.live_streamer_id),
        streamer_info_id = text(&identity.streamer_info_id),
        "开始提交投稿"
    );
}

/// A submission whose remote result is not known must stay `unknown`: no error line is not proof
/// of success, and the claim is deliberately kept so nothing else resubmits it.
pub fn submission_completed(identity: &SubmissionIdentity, outcome: &str, reason_code: &str) {
    if outcome == "succeeded" {
        info!(
            target: EVENT_TARGET,
            event_name = "submission.completed",
            outcome,
            reason_code,
            upload_session_id = text(&identity.upload_session_id),
            task_id = text(&identity.task_id),
            live_streamer_id = text(&identity.live_streamer_id),
            streamer_info_id = text(&identity.streamer_info_id),
            "投稿完成"
        );
    } else {
        warn!(
            target: EVENT_TARGET,
            event_name = "submission.completed",
            outcome,
            reason_code,
            upload_session_id = text(&identity.upload_session_id),
            task_id = text(&identity.task_id),
            live_streamer_id = text(&identity.live_streamer_id),
            streamer_info_id = text(&identity.streamer_info_id),
            "投稿未成功"
        );
    }
}
