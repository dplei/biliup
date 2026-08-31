//! Entry runs and credential failures, observed through the real process and the real wrappers.
//!
//! A run is not a business task: the recording or upload inside it keeps its own identity, and
//! neither id is ever used for the other. A killed process leaves no stop event at all, which is
//! why an interrupted run is reported as unknown rather than as a failure or a success.
use biliup_cli::observe::lifecycle::{self, Invocation};
use biliup_observability::sqlite::{Query, Repository};
use biliup_observability::*;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing_subscriber::layer::SubscriberExt;

struct Memory(Arc<Mutex<Vec<Event>>>);
impl Consumer for Memory {
    fn write(&mut self, batch: &[Event]) -> Result<Commit, StorageError> {
        self.0.lock().unwrap().extend_from_slice(batch);
        Ok(Commit::default())
    }
}

fn runtime(sink: Arc<Mutex<Vec<Event>>>) -> Runtime {
    Runtime::start(
        "synthetic",
        "test",
        Options {
            enabled: true,
            ..Options::default()
        },
        move || Ok(Memory(sink.clone())),
    )
    .unwrap()
}

fn native(events: &[Event], name: &str) -> Vec<EventData> {
    events
        .iter()
        .map(Event::data)
        .filter(|data| data.event_name == name)
        .cloned()
        .collect()
}

fn field(data: &EventData, key: &str) -> String {
    data.fields
        .get(key)
        .map(|value| value.as_str().unwrap_or_default().to_owned())
        .unwrap_or_default()
}

#[tokio::test]
async fn a_run_reports_one_of_three_results_and_never_invents_the_missing_one() {
    let events = Arc::new(Mutex::new(Vec::<Event>::new()));
    let mut rt = runtime(events.clone());
    let subscriber =
        tracing_subscriber::registry().with(CaptureLayer::new(rt.emitter()).filtered());
    {
        let _guard = tracing::subscriber::set_default(subscriber);
        lifecycle::run("synthetic_entry", "upload", async { Ok::<(), ()>(()) })
            .await
            .unwrap();
        lifecycle::run("synthetic_entry", "login", async { Err::<(), ()>(()) })
            .await
            .unwrap_err();
        // A cancelled run still ends, and it ends without a known result.
        let pending = lifecycle::run("synthetic_entry", "server", async {
            std::future::pending::<Result<(), ()>>().await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), pending)
                .await
                .is_err()
        );
    };
    // Draining first: a batch that is still queued is not yet evidence of anything.
    rt.shutdown(Duration::from_secs(5));
    let collected = events.lock().unwrap().clone();

    let started = native(&collected, "system.started");
    let stopped = native(&collected, "system.stopped");
    assert_eq!(started.len(), 3);
    assert_eq!(stopped.len(), 3);

    // Every run reports its own id, and the start and the stop of one run share it.
    let ids: Vec<String> = started.iter().map(|data| field(data, "task_id")).collect();
    assert!(ids.iter().all(|id| !id.is_empty()));
    assert_ne!(ids[0], ids[1]);
    assert_ne!(ids[1], ids[2]);
    for (index, expected) in ids.iter().enumerate() {
        assert_eq!(&field(&stopped[index], "task_id"), expected);
    }

    for start in &started {
        assert_eq!(field(start, "outcome"), "executed");
        assert_eq!(field(start, "reason_code"), "startup");
        assert_eq!(field(start, "stage"), "synthetic_entry");
        assert_eq!(start.level, Level::Info);
    }
    let results: Vec<(String, String, String, Level)> = stopped
        .iter()
        .map(|data| {
            (
                field(data, "command"),
                field(data, "outcome"),
                field(data, "reason_code"),
                data.level,
            )
        })
        .collect();
    assert_eq!(
        results,
        [
            (
                "upload".into(),
                "executed".into(),
                "shutdown".into(),
                Level::Info
            ),
            (
                "login".into(),
                "failed".into(),
                "entry_failed".into(),
                Level::Warn
            ),
            (
                "server".into(),
                "unknown".into(),
                "entry_interrupted".into(),
                Level::Warn
            ),
        ]
    );
}

#[tokio::test]
async fn overlapping_runs_stay_separate() {
    let events = Arc::new(Mutex::new(Vec::<Event>::new()));
    let mut rt = runtime(events.clone());
    let subscriber =
        tracing_subscriber::registry().with(CaptureLayer::new(rt.emitter()).filtered());
    {
        let _guard = tracing::subscriber::set_default(subscriber);
        // Embedded calls can share a process run, so the run id is what tells two of them apart.
        let mut first = Invocation::start("python_upload", "upload");
        let mut second = Invocation::start("python_upload", "upload");
        assert_ne!(first.task_id(), second.task_id());
        second.finish(&Err::<(), ()>(()));
        first.finish(&Ok::<(), ()>(()));
    };
    // Draining first: a batch that is still queued is not yet evidence of anything.
    rt.shutdown(Duration::from_secs(5));
    let collected = events.lock().unwrap().clone();

    let stopped = native(&collected, "system.stopped");
    assert_eq!(stopped.len(), 2);
    assert_eq!(field(&stopped[0], "outcome"), "failed");
    assert_eq!(field(&stopped[1], "outcome"), "executed");
    assert_ne!(field(&stopped[0], "task_id"), field(&stopped[1], "task_id"));
}

/// The subcommand travels as a fixed word from the parsed enum, never as raw argument text.
#[test]
fn command_names_are_a_frozen_vocabulary() {
    use biliup_cli::cli::Commands;
    assert_eq!(lifecycle::command_name(&Commands::Login), "login");
    assert_eq!(lifecycle::command_name(&Commands::Renew), "renew");
    assert_eq!(
        lifecycle::command_name(&Commands::Server {
            bind: "0.0.0.0".into(),
            port: 0,
            auth: false,
            config: None,
        }),
        "server"
    );
}

/// A credential failure is typed without keeping the message that produced the type.
#[tokio::test]
async fn a_failed_credential_operation_reports_a_type_and_no_secret() {
    let events = Arc::new(Mutex::new(Vec::<Event>::new()));
    let mut rt = runtime(events.clone());
    let subscriber =
        tracing_subscriber::registry().with(CaptureLayer::new(rt.emitter()).filtered());
    let dir = tempfile::tempdir().unwrap();
    let cookie = dir.path().join("cookies.json");
    std::fs::write(
        &cookie,
        serde_json::json!({
            "cookie_info": {"cookies": []},
            "sso": [],
            "platform": "Android",
            "token_info": {
                "access_token": "synthetic-access",
                "expires_in": 0,
                "mid": 0,
                "refresh_token": "synthetic-refresh"
            }
        })
        .to_string(),
    )
    .unwrap();
    {
        let _guard = tracing::subscriber::set_default(subscriber);
        // A proxy pointing at a closed local port fails the request without leaving the machine,
        // so this exercises the real renew path with no account and no network.
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            biliup_cli::uploader::renew(cookie.clone(), Some("http://127.0.0.1:1")),
        )
        .await
        .expect("renew returns without hanging");
        assert!(result.is_err());
    };
    // Draining first: a batch that is still queued is not yet evidence of anything.
    rt.shutdown(Duration::from_secs(5));
    let collected = events.lock().unwrap().clone();

    let failures = native(&collected, "auth.operation_failed");
    assert_eq!(failures.len(), 1);
    assert_eq!(field(&failures[0], "stage"), "renew");
    assert_eq!(field(&failures[0], "platform"), "bilibili");
    assert_eq!(field(&failures[0], "outcome"), "failed");
    // A refused proxy connection: the request never left the machine, and the type says so.
    assert_eq!(field(&failures[0], "reason_code"), "transport_error");
    // One failed operation is not proof of an expired credential.
    assert!(native(&collected, "auth.health_changed").is_empty());
    for (_, value) in failures[0].fields.iter() {
        let rendered = value.to_string();
        assert!(!rendered.contains("synthetic-refresh"), "{rendered}");
        assert!(!rendered.contains("cookies.json"), "{rendered}");
    }
}

/// The real binary, its real global subscriber and its real exit path — not a wrapper in a test.
#[tokio::test]
async fn the_installed_cli_records_both_of_its_own_exits() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("events.sqlite3");
    let run = |args: Vec<String>| {
        std::process::Command::new(env!("CARGO_BIN_EXE_biliup"))
            .args(args)
            .env("BILIUP_OBSERVABILITY", "1")
            .env("BILIUP_OBSERVABILITY_DB", &db)
            .env("BILIUP_OBSERVABILITY_INSTANCE", "entry-lifecycle-test")
            .current_dir(dir.path())
            .output()
            .unwrap()
    };
    assert!(
        run(vec![
            "cover-preview".into(),
            "--text".into(),
            "synthetic".into(),
            "--output".into(),
            "cover.jpg".into(),
        ])
        .status
        .success()
    );
    assert!(
        !run(vec!["dump-flv".into(), "missing-input.flv".into()])
            .status
            .success()
    );

    let repository = Repository::open(&db).await.unwrap();
    let page = repository
        .query(&Query {
            capture_kind: Some(CaptureKind::Native),
            category: Some("system".into()),
            limit: 64,
            ..Query::default()
        })
        .await
        .unwrap();
    repository.close().await;

    let seen: Vec<(String, String, String)> = page
        .events
        .iter()
        .map(|stored| {
            (
                stored.data.event_name.clone(),
                field(&stored.data, "command"),
                field(&stored.data, "outcome"),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            (
                "system.started".into(),
                "cover_preview".into(),
                "executed".into()
            ),
            (
                "system.stopped".into(),
                "cover_preview".into(),
                "executed".into()
            ),
            (
                "system.started".into(),
                "dump_flv".into(),
                "executed".into()
            ),
            ("system.stopped".into(), "dump_flv".into(), "failed".into()),
        ]
    );
    // Two separate processes, so two separate runs — the version travels with each of them.
    let runs: Vec<&str> = page
        .events
        .iter()
        .map(|stored| stored.data.process_run_id.as_str())
        .collect();
    assert_eq!(runs[0], runs[1]);
    assert_eq!(runs[2], runs[3]);
    assert_ne!(runs[0], runs[2]);
    assert!(
        page.events
            .iter()
            .all(|stored| !stored.data.app_version.is_empty())
    );
}
