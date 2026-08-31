use biliup_observability::{
    shadow::{self, Config, Shadow},
    sqlite::{Query, Repository},
    *,
};
use std::{
    io::Write,
    sync::{Arc, Mutex},
};
use tracing_subscriber::{Layer, layer::SubscriberExt};

#[derive(Clone)]
struct Output(Arc<Mutex<Vec<u8>>>);
impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn config(path: &std::path::Path) -> Config {
    Config {
        path: path.into(),
        instance: "synthetic".into(),
        level: Level::Info,
    }
}
fn rows(path: &std::path::Path) -> Vec<biliup_observability::sqlite::StoredEvent> {
    shadow::block_on(
        tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default()),
        true,
        async {
            let repo = Repository::open(path).await.unwrap();
            let rows = repo
                .query(&Query {
                    limit: 200,
                    ..Query::default()
                })
                .await
                .unwrap()
                .events;
            repo.close().await;
            rows
        },
    )
    .unwrap()
}

#[test]
fn reload_does_not_filter_shadow_and_workers_keep_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.sqlite");
    let shadow = Shadow::start(config(&path), "test").unwrap();
    let output = Output(Arc::default());
    let writer = output.clone();
    let (filter, reload) =
        tracing_subscriber::reload::Layer::new(tracing_subscriber::EnvFilter::new("error"));
    let subscriber = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .without_time()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .with_filter(tracing_subscriber::filter::filter_fn(legacy_output))
                .with_filter(filter),
        )
        .with(shadow.layer().map(|l| l.filtered()));
    shadow::block_on(tracing::Dispatch::new(subscriber), false, async {
        tracing::info!("new-only");
        reload
            .reload(tracing_subscriber::EnvFilter::new("info"))
            .unwrap();
        tokio::spawn(async {
            tracing::warn!("worker-message");
        })
        .await
        .unwrap();
        tokio::task::spawn_blocking(|| tracing::error!("blocking-message"))
            .await
            .unwrap();
        tracing::info!(target:"biliup::event",event_name="system.started","native-not-old");
    })
    .unwrap();
    drop(shadow);
    let old = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
    assert!(!old.contains("new-only"));
    assert!(!old.contains("native-not-old"));
    assert!(old.contains("worker-message"));
    assert!(old.contains("blocking-message"));
    let events = rows(&path);
    assert_eq!(events.len(), 4);
    assert_eq!(
        events
            .iter()
            .filter(|r| r.data.capture_kind == CaptureKind::LegacyBridge)
            .count(),
        3
    );
}

#[test]
fn repeated_calls_shared_worker_and_inherited_host_do_not_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.sqlite");
    let output = Output(Arc::default());
    let writer = output.clone();
    let host = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .without_time()
            .with_writer(move || writer.clone()),
    );
    tracing::subscriber::with_default(host, || {
        for _ in 0..2 {
            let shadow = Shadow::start(config(&path), "test").unwrap();
            let shared = Shadow::start(config(&path), "test").unwrap();
            shadow::block_on(shadow.inherited_dispatch(), false, async {
                // Nested helper inherits the existing bridge rather than forwarding into it twice.
                tracing::dispatcher::with_default(&shared.inherited_dispatch(), || {
                    tracing::warn!("helper-message")
                });
                tokio::spawn(async {
                    tracing::warn!("helper-worker");
                })
                .await
                .unwrap();
            })
            .unwrap();
            drop(shared);
            drop(shadow);
        }
        tracing::warn!("host-still-active");
    });
    let events = rows(&path);
    assert_eq!(events.len(), 4);
    let old = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
    assert_eq!(old.matches("helper-message").count(), 2);
    assert_eq!(old.matches("helper-worker").count(), 2);
    assert!(old.contains("host-still-active"));
    assert!(
        events
            .iter()
            .all(|r| r.data.fields.get("streamer_info_id").is_none())
    );
}

#[test]
fn failed_database_keeps_legacy_and_reports_loss() {
    let dir = tempfile::tempdir().unwrap();
    let shadow = Shadow::start(config(&dir.path().join("absent/events.sqlite")), "test").unwrap();
    let output = Output(Arc::default());
    let writer = output.clone();
    let subscriber = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .without_time()
                .with_writer(move || writer.clone()),
        )
        .with(shadow.layer().map(|l| l.filtered()));
    tracing::subscriber::with_default(subscriber, || tracing::error!("old-survives"));
    drop(shadow);
    assert!(
        String::from_utf8(output.0.lock().unwrap().clone())
            .unwrap()
            .contains("old-survives")
    );
    let health = shadow::health_snapshot();
    assert_eq!(health["legacy_file_health"], "unknown");
    assert!(
        health["runs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["storage_failures"].as_u64().unwrap() > 0)
    );
}
