//! Task 15 under load: the query API must stay inside the frozen P0 budget while the writer is
//! still committing, and a long export must not pin the database.
//!
//! Its own test binary on purpose: the API holds one process-wide read handle, so this file gets
//! its own process and cannot race the functional test's store.

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use axum::routing::get;
use biliup_cli::server::api::log_events::{export_log_events, list_log_events};
use biliup_observability::sqlite::{SqliteStore, StoreOptions};
use biliup_observability::{Context, Draft, Fields, Level, Options, Runtime};
use http_body_util::BodyExt;
use std::time::{Duration, Instant};
use tower::ServiceExt;

const EVENTS: usize = 20_000;
/// From the frozen budget: a single query is bounded at 250ms, including at 20,000 rows.
const QUERY_BUDGET: Duration = Duration::from_millis(250);

fn app() -> Router {
    Router::new()
        .route("/v1/log-events", get(list_log_events))
        .route("/v1/log-events/export", get(export_log_events))
}

async fn call(uri: &str) -> axum::response::Response {
    app()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn queries_stay_inside_budget_while_the_writer_is_running() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("events.sqlite");
    let options = StoreOptions::new(&database);
    let mut runtime = Runtime::start(
        "scale-test",
        "test",
        Options {
            enabled: true,
            ..Options::default()
        },
        move || SqliteStore::open(options.clone()),
    )
    .unwrap();
    let emitter = runtime.emitter();
    // The frozen budget is 2,000 events/second sustained, not an unbounded burst: an unpaced
    // loop is expected to hit the bounded queue and shed INFO, which measures nothing.
    let writer = std::thread::spawn(move || {
        for index in 0..EVENTS {
            if index % 200 == 0 && index > 0 {
                std::thread::sleep(Duration::from_millis(100));
            }
            let mut draft = Draft::new("upload.completed", "分段上传完成");
            draft.context = Context(
                Fields::new()
                    .with("task_id", format!("task-{}", index % 4))
                    .with("segment_id", format!("seg-{index}")),
            );
            let event = emitter.create(Level::Info, draft).unwrap();
            emitter.submit(event);
        }
        emitter
    });
    let emitter = writer.join().unwrap();
    // Query while the tail is still being committed: this is the concurrent case the budget is
    // about, not a quiet database.
    unsafe {
        std::env::set_var("BILIUP_OBSERVABILITY", "1");
        std::env::set_var("BILIUP_OBSERVABILITY_DB", &database);
    }
    let mut worst = Duration::ZERO;
    for uri in [
        "/v1/log-events?limit=200",
        "/v1/log-events?limit=200&min_level=WARN",
        "/v1/log-events?limit=200&keyword=上传",
        "/v1/log-events?limit=200&instance_id=scale-test&assoc_key=task_id&assoc_value=task-2",
        "/v1/log-events?limit=200&after_id=10000",
    ] {
        let started = Instant::now();
        let response = call(uri).await;
        let elapsed = started.elapsed();
        worst = worst.max(elapsed);
        assert!(response.status().is_success(), "{uri} failed");
        assert!(elapsed < QUERY_BUDGET, "{uri} took {elapsed:?}");
    }

    let deadline = Instant::now() + Duration::from_secs(30);
    while emitter.health().delivered < EVENTS as u64 {
        assert!(Instant::now() < deadline, "{:?}", emitter.health());
        std::thread::sleep(Duration::from_millis(20));
    }
    let health = emitter.health();
    assert_eq!(health.dropped.iter().sum::<u64>(), 0, "no drops under load");
    runtime.shutdown(Duration::from_secs(5));

    // A full export is bounded and says so; it must not try to stream the whole table forever.
    let started = Instant::now();
    let body = call("/v1/log-events/export").await;
    let text = String::from_utf8(
        body.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    let lines = text.lines().count();
    assert!(lines <= 20_001, "export must stay bounded, got {lines}");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "export took {:?}",
        started.elapsed()
    );
    // Reading everything must not leave the database unusable for the next reader.
    assert!(call("/v1/log-events?limit=1").await.status().is_success());
}
