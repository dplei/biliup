//! Exercise the real page handler and its detached task with a local subscriber.
//! Missing/malformed local credentials keep every scenario off the remote network.

use axum::Router;
use axum::body::Body;
use axum::extract::FromRef;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use biliup_cli::server::api::endpoints::post_uploads;
use biliup_cli::server::config::Config;
use biliup_cli::server::infrastructure::connection_pool::{ConnectionManager, ConnectionPool};
use biliup_observability::*;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tower::ServiceExt;
use tracing::instrument::WithSubscriber;
use tracing_subscriber::layer::SubscriberExt;

struct Memory(Arc<Mutex<Vec<Event>>>);
impl Consumer for Memory {
    fn write(&mut self, events: &[Event]) -> Result<Commit, StorageError> {
        self.0.lock().unwrap().extend_from_slice(events);
        Ok(Commit::default())
    }
}

#[derive(Clone, FromRef)]
struct State {
    pool: ConnectionPool,
    config: Arc<RwLock<Config>>,
}

async fn request(app: Router, payload: Value, dispatch: tracing::Dispatch) -> Value {
    let response = app
        .oneshot(
            Request::post("/v1/uploads")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .with_subscriber(dispatch)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_requests_correlate_detached_failures_without_inventing_recordings() {
    let dir = tempfile::tempdir().unwrap();
    let pool = ConnectionManager::new_pool(dir.path().join("business.sqlite").to_str().unwrap())
        .await
        .unwrap();
    sqlx::query("INSERT INTO streamerinfo (id, name, url, title, date, live_cover_path) VALUES (1, 'synthetic', 'https://example.invalid/live', 'synthetic', ?, '')")
        .bind(chrono::Utc::now()).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO filelist (file, streamer_info_id) VALUES ('known.flv', 1)")
        .execute(&pool)
        .await
        .unwrap();
    let app = Router::new()
        .route("/v1/uploads", post(post_uploads))
        .with_state(State {
            pool: pool.clone(),
            config: Arc::new(RwLock::new(Config::default())),
        });
    let events = Arc::new(Mutex::new(Vec::<Event>::new()));
    let sink = events.clone();
    let mut runtime = Runtime::start(
        "synthetic",
        "test",
        Options {
            enabled: true,
            ..Options::default()
        },
        move || Ok(Memory(sink.clone())),
    )
    .unwrap();
    let dispatch = tracing::Dispatch::new(
        tracing_subscriber::registry().with(CaptureLayer::new(runtime.emitter()).filtered()),
    );
    let payload = |files: Value, path: &str| {
        json!({
            "files": files,
            "params": {"id": 0, "template_name": "synthetic", "tags": [],
                       "user_cookie": dir.path().join(path), "is_only_self": 1}
        })
    };
    // Malformed JSON never enters the handler, so it must not fabricate an accepted task.
    let rejected = app
        .clone()
        .oneshot(
            Request::post("/v1/uploads")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .with_subscriber(dispatch.clone())
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(runtime.emitter().health().accepted, 0);

    // A local parse failure is safe too; the payload must never become a native error string.
    std::fs::write(
        dir.path().join("malformed.json"),
        "authorization=secret-sentinel",
    )
    .unwrap();
    let mut responses = Vec::new();
    for _ in 0..2 {
        let (matched, unknown) = tokio::join!(
            request(
                app.clone(),
                payload(json!(["known.flv", "other.flv"]), "absent.json"),
                dispatch.clone()
            ),
            request(
                app.clone(),
                payload(json!(["other.flv"]), "malformed.json"),
                dispatch.clone()
            )
        );
        assert_eq!(matched["matched"], true);
        assert_eq!(matched["streamer_name"], "synthetic");
        assert_eq!(unknown["matched"], false);
        assert!(unknown["streamer_name"].is_null());
        responses.extend([matched, unknown]);
    }
    // Preserve the existing empty-input and failed metadata-lookup behavior (accept, then
    // background auth failure); observation must not turn either into a synchronous error.
    responses.push(
        request(
            app.clone(),
            payload(json!([]), "absent.json"),
            dispatch.clone(),
        )
        .await,
    );
    pool.close().await;
    let fallback = request(app, payload(json!(["known.flv"]), "absent.json"), dispatch).await;
    assert_eq!(fallback["matched"], false);
    responses.push(fallback);
    let tasks: HashSet<_> = responses
        .iter()
        .map(|r| {
            let task = r["task_id"].as_str().unwrap();
            uuid::Uuid::parse_str(task).unwrap();
            task.to_owned()
        })
        .collect();
    assert_eq!(tasks.len(), 6);

    // Only the handler future has a local dispatch. Poll outside it, so losing the dispatch
    // across tokio::spawn would leave each task without its background failure event.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| {
                    event
                        .data()
                        .fields
                        .get("reason_code")
                        .is_some_and(|v| v == "authentication_failed")
                })
                .count()
                == 6
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("detached upload events were lost");
    assert!(runtime.shutdown(Duration::from_secs(2)).closed);
    let events = events.lock().unwrap();
    let native: Vec<_> = events
        .iter()
        .filter(|e| e.data().capture_kind == CaptureKind::Native)
        .collect();
    assert_eq!(native.len(), 12);
    for task in tasks {
        let chain: Vec<_> = native
            .iter()
            .filter(|e| e.data().fields.get("task_id") == Some(&json!(task)))
            .collect();
        assert_eq!(chain.len(), 2);
        assert_eq!(
            chain[0].data().fields.get("reason_code"),
            Some(&json!("preparing_upload"))
        );
        assert_eq!(
            chain[1].data().fields.get("reason_code"),
            Some(&json!("authentication_failed"))
        );
        assert_eq!(
            chain[1].data().fields.get("outcome"),
            Some(&json!("failed"))
        );
        for event in chain {
            assert_eq!(event.data().event_name, "submission.decided");
            for key in [
                "segment_id",
                "live_streamer_id",
                "streamer_info_id",
                "upload_session_id",
            ] {
                assert!(
                    event.data().fields.get(key).is_none(),
                    "template lookup must not assign {key}"
                );
            }
        }
    }
    let encoded =
        serde_json::to_string(&native.iter().map(|e| e.data()).collect::<Vec<_>>()).unwrap();
    assert!(!encoded.contains("secret-sentinel"));
    assert!(!encoded.contains("absent.json"));
}
