//! One run of an entry point: a whole process for the two CLIs, one embedded call for the
//! Python bindings. This is deliberately not a business identity — a recording or an upload
//! keeps its own task id, and the two are never used for each other. Arguments, account files
//! and error text stay out; only the typed result of the run is reported.

use super::EVENT_TARGET;
use crate::cli::Commands;
use std::future::Future;
use tracing::{info, warn};

/// The entry that owns the run. It is the shape of the process, not the work it does.
pub const RUST_CLI: &str = "rust_cli";
pub const WHEEL_CLI: &str = "wheel_cli";
pub const PYTHON_DOWNLOAD: &str = "python_download";
pub const PYTHON_UPLOAD: &str = "python_upload";

pub struct Invocation {
    stage: &'static str,
    command: &'static str,
    task: String,
    finished: bool,
}

impl Invocation {
    pub fn start(stage: &'static str, command: &'static str) -> Self {
        let task = biliup::downloader::util::allocate_id("entry");
        info!(
            target: EVENT_TARGET,
            event_name = "system.started",
            outcome = "executed",
            reason_code = "startup",
            stage,
            command,
            task_id = task,
            "入口开始执行"
        );
        Self {
            stage,
            command,
            task,
            finished: false,
        }
    }

    /// The run's own id, which identifies this invocation and nothing else. Overlapping embedded
    /// calls can share a process run, so the id is what tells two of them apart.
    pub fn task_id(&self) -> &str {
        &self.task
    }

    pub fn finish<T, E>(&mut self, result: &Result<T, E>) {
        self.finished = true;
        if result.is_ok() {
            info!(
                target: EVENT_TARGET,
                event_name = "system.stopped",
                outcome = "executed",
                reason_code = "shutdown",
                stage = self.stage,
                command = self.command,
                task_id = self.task,
                "入口正常结束"
            );
        } else {
            warn!(
                target: EVENT_TARGET,
                event_name = "system.stopped",
                outcome = "failed",
                reason_code = "entry_failed",
                stage = self.stage,
                command = self.command,
                task_id = self.task,
                "入口以错误结束"
            );
        }
    }
}

/// A run that unwinds or is cancelled still ends, and it ends without a known result. A killed
/// process runs no destructor at all, so a missing stop event stays missing — never invented.
impl Drop for Invocation {
    fn drop(&mut self) {
        if !self.finished {
            warn!(
                target: EVENT_TARGET,
                event_name = "system.stopped",
                outcome = "unknown",
                reason_code = "entry_interrupted",
                stage = self.stage,
                command = self.command,
                task_id = self.task,
                "入口未返回确定结果"
            );
        }
    }
}

pub async fn run<T, E>(
    stage: &'static str,
    command: &'static str,
    work: impl Future<Output = Result<T, E>>,
) -> Result<T, E> {
    let mut invocation = Invocation::start(stage, command);
    let result = work.await;
    invocation.finish(&result);
    result
}

/// The subcommand as a fixed word from the parsed enum, never the raw argument text.
pub fn command_name(command: &Commands) -> &'static str {
    match command {
        Commands::Login => "login",
        Commands::Renew => "renew",
        Commands::Upload { .. } => "upload",
        Commands::Append { .. } => "append",
        Commands::Show { .. } => "show",
        Commands::Comments { .. } => "comments",
        Commands::Reply { .. } => "reply",
        Commands::DumpFlv { .. } => "dump_flv",
        Commands::Download { .. } => "download",
        Commands::Server { .. } => "server",
        Commands::CoverPreview { .. } => "cover_preview",
        Commands::List { .. } => "list",
        Commands::BackfillLifecycle { .. } => "backfill_lifecycle",
    }
}
