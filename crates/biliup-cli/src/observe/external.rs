//! Bounded diagnostics for external commands. The attachment is written through this
//! invocation's collector only, never a globally discovered one, so overlapping embedded calls
//! cannot write into another entry's database.
use biliup_observability::{Context, Diagnostic, Draft, Fields, Level, shadow::current_emitter};

/// One external command did not succeed. `exit_code` is absent for a signal exit, and stays
/// absent rather than being invented; the bounded stderr tail travels as an attachment, so the
/// event list itself never carries third party output.
pub fn command_failed(
    stage: &str,
    reason_code: &str,
    context: Context,
    diagnostic: Option<Diagnostic>,
    exit_code: Option<i32>,
) {
    let Some(emitter) = current_emitter() else {
        return;
    };
    emitter.emit_with(Level::Warn, || {
        let mut draft = Draft::new(
            "processing.command_failed",
            "外部命令未成功，诊断详情已按限额采集",
        );
        draft.context = context;
        draft.fields = Fields::new()
            .with("stage", stage)
            .with("outcome", "failed")
            .with("reason_code", reason_code);
        if let Some(code) = exit_code {
            draft.fields.insert("exit_code", code.into());
        }
        if let Some(diagnostic) = diagnostic {
            draft
                .fields
                .insert("total_bytes", diagnostic.total_bytes().into());
            draft.diagnostic = Some(diagnostic);
        }
        draft
    });
}

/// Native marker for auxiliary subsystems which are not OS commands. Raw third-party errors stay
/// in the unchanged legacy output; the event only carries a stable stage and reason.
pub fn auxiliary_failed(
    event_name: &str,
    message: &str,
    stage: &str,
    reason_code: &str,
    context: Context,
) {
    let Some(emitter) = current_emitter() else {
        return;
    };
    emitter.emit_with(Level::Warn, || {
        let mut draft = Draft::new(event_name, message);
        draft.context = context;
        draft.fields = Fields::new()
            .with("stage", stage)
            .with("outcome", "failed")
            .with("reason_code", reason_code);
        draft
    });
}
