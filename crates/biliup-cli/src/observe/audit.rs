//! Best-effort views of durable business audit rows.
//!
//! The business database remains authoritative.  A persisted UUID lets startup and recovery
//! replay the same event without creating duplicate rows in the independent event database.

use biliup_observability::{Context, Draft, Fields, Level, shadow::current_emitter};
use std::path::Path;

fn result(reason: &str) -> (&'static str, &'static str) {
    match reason {
        "source_missing_before_enrollment" => ("failed", "source_missing"),
        "late_validated_segment_for_finalized_session"
        | "late_outbox_manifest_for_finalized_session"
        | "rescan_skipped_finalized_session" => ("skipped", "session_finalized"),
        _ => ("unknown", "audit_reason_unknown"),
    }
}

/// Project one already-durable recovery audit row.  `false` means the current invocation has no
/// enabled collector, rejected the event, or could not accept it; it never changes business state.
pub fn operation_projected(
    event_uid: uuid::Uuid,
    occurred_at_ms: i64,
    live_streamer_id: i64,
    streamer_info_id: i64,
    file_path: &Path,
    reason: &str,
) -> bool {
    let Some(emitter) = current_emitter() else {
        return false;
    };
    let (outcome, reason_code) = result(reason);
    let mut draft = Draft::new(
        "audit.operation_projected",
        "持久恢复审计已投影；业务审计表仍是权威来源",
    );
    draft.context = Context(
        Fields::new()
            .with("live_streamer_id", live_streamer_id)
            .with("streamer_info_id", streamer_info_id)
            .with("original_file", file_path.display().to_string()),
    );
    draft.fields = Fields::new()
        .with("stage", reason)
        .with("outcome", outcome)
        .with("reason_code", reason_code);
    emitter
        .project(event_uid, occurred_at_ms, Level::Warn, draft)
        .is_ok_and(|event| emitter.submit(event))
}

#[cfg(test)]
mod tests {
    use super::result;

    #[test]
    fn durable_reasons_have_a_closed_projection_vocabulary() {
        assert_eq!(
            result("source_missing_before_enrollment"),
            ("failed", "source_missing")
        );
        for reason in [
            "late_validated_segment_for_finalized_session",
            "late_outbox_manifest_for_finalized_session",
            "rescan_skipped_finalized_session",
        ] {
            assert_eq!(result(reason), ("skipped", "session_finalized"));
        }
        assert_eq!(result("future_reason"), ("unknown", "audit_reason_unknown"));
    }
}
