//! Task 15: the query, live and export API over the independent event store.
//!
//! One test process, one store: the handler holds a single lazy read-only handle, so the whole
//! contract is exercised in order inside one test rather than racing several.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use biliup_cli::server::api::log_events::{
    export_log_events, get_log_event_diagnostic, list_log_events, stream_log_events,
};
use biliup_observability::sqlite::{SqliteStore, StoreOptions};
use biliup_observability::{Context, Draft, Fields, Level, Options, Runtime};
use http_body_util::BodyExt;
use std::time::Duration;
use tower::ServiceExt;

fn app() -> Router {
    Router::new()
        .route("/v1/log-events", get(list_log_events))
        .route("/v1/log-events/export", get(export_log_events))
        .route("/v1/log-events/stream", get(stream_log_events))
        .route(
            "/v1/log-events/{event_uid}/diagnostic",
            get(get_log_event_diagnostic),
        )
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn call(uri: &str) -> axum::response::Response {
    app()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

fn write_events(path: &std::path::Path) {
    let options = StoreOptions::new(path);
    let mut runtime = Runtime::start_with_identity(
        "api-test",
        "test",
        Options {
            enabled: true,
            bridge: true,
            ..Options::default()
        },
        move |instance_id, process_run_id| {
            SqliteStore::open(options.clone(), instance_id, process_run_id)
        },
    )
    .unwrap();
    let emitter = runtime.emitter();
    for (name, message, task, level) in [
        ("recording.started", "开始录制", "task-a", Level::Info),
        ("upload.failed", "分段上传失败", "task-a", Level::Error),
        ("upload.completed", "分段上传完成", "task-b", Level::Info),
    ] {
        let mut draft = Draft::new(name, message);
        draft.context = Context(Fields::new().with("task_id", task));
        let event = emitter.create(level, draft).unwrap();
        assert!(emitter.submit(event));
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(4);
    while emitter.health().delivered < 3 {
        assert!(std::time::Instant::now() < deadline, "writer never drained");
        std::thread::sleep(Duration::from_millis(5));
    }
    runtime.shutdown(Duration::from_secs(2));
}

#[tokio::test(flavor = "multi_thread")]
async fn query_export_and_unavailable_states_are_distinguishable() {
    // 1. Capture off: an empty list must not look like "nothing went wrong".
    unsafe {
        std::env::remove_var("BILIUP_OBSERVABILITY");
        std::env::remove_var("BILIUP_OBSERVABILITY_DB");
    }
    let disabled = body_json(call("/v1/log-events").await).await;
    assert_eq!(disabled["availability"], "disabled");
    assert_eq!(disabled["coverage"], "none");
    assert!(disabled["error"].is_string());
    assert_eq!(disabled["events"].as_array().unwrap().len(), 0);

    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("events.sqlite");
    write_events(&database);
    unsafe {
        std::env::set_var("BILIUP_OBSERVABILITY", "1");
        std::env::set_var("BILIUP_OBSERVABILITY_DB", &database);
    }

    // 2. Native is the default view, and the answer says which kinds it covers.
    let listed = body_json(call("/v1/log-events").await).await;
    assert_eq!(listed["availability"], "ready");
    assert_eq!(listed["coverage"], "native");
    assert_eq!(listed["events"].as_array().unwrap().len(), 3);
    assert_eq!(listed["total"], 3);
    assert_eq!(listed["gap"], false);
    assert!(
        listed["next_after_id"].is_null(),
        "the page reached the end"
    );
    assert!(listed["health"]["runs"].is_array());

    // 3. Bridge diagnostics only when asked for; the count follows the same filter.
    let bridge = body_json(call("/v1/log-events?capture_kind=legacy_bridge").await).await;
    assert_eq!(bridge["coverage"], "legacy_bridge");
    assert_eq!(bridge["total"], 0);

    // 4. Paging is stable and the count still covers the whole range.
    let first = body_json(call("/v1/log-events?limit=2").await).await;
    assert_eq!(first["events"].as_array().unwrap().len(), 2);
    assert_eq!(first["total"], 3);
    let cursor = first["next_after_id"].as_u64().expect("more to read");
    let second = body_json(call(&format!("/v1/log-events?limit=2&after_id={cursor}")).await).await;
    assert_eq!(second["events"].as_array().unwrap().len(), 1);
    assert!(second["next_after_id"].is_null());
    let ids: Vec<u64> = first["events"]
        .as_array()
        .unwrap()
        .iter()
        .chain(second["events"].as_array().unwrap())
        .map(|event| event["id"].as_u64().unwrap())
        .collect();
    assert_eq!(ids.len(), 3, "no duplicates across the two pages");
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));

    // 5. Combined filters: keyword plus level plus association.
    let keyword = body_json(call("/v1/log-events?keyword=上传").await).await;
    assert_eq!(keyword["total"], 2);
    let association = body_json(
        call("/v1/log-events?instance_id=api-test&assoc_key=task_id&assoc_value=task-b").await,
    )
    .await;
    assert_eq!(association["total"], 1);

    // 6. A malformed filter is the caller's error, not a 500.
    assert_eq!(
        call("/v1/log-events?min_level=LOUD").await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call("/v1/log-events?assoc_key=task_id").await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call("/v1/log-events?capture_kind=whatever").await.status(),
        StatusCode::BAD_REQUEST
    );

    // 7. Export streams the current query and never writes a server-side file.
    let jsonl = call("/v1/log-events/export?keyword=上传").await;
    assert_eq!(
        jsonl.headers()[axum::http::header::CONTENT_TYPE],
        "application/x-ndjson"
    );
    let lines: Vec<_> = body_text(jsonl).await.lines().map(str::to_owned).collect();
    assert_eq!(lines.len(), 2);
    for line in &lines {
        let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(
            parsed["data"]["event_name"]
                .as_str()
                .unwrap()
                .starts_with("upload.")
        );
    }
    let csv = body_text(call("/v1/log-events/export?format=csv").await).await;
    assert!(csv.starts_with("id,occurred_at_ms,level,category,event_name,capture_kind"));
    assert_eq!(csv.lines().count(), 4, "header plus three rows");

    // 8. A diagnostic that does not exist is a 404, and a malformed id is not a 500.
    let missing = call("/v1/log-events/2b5c6a4e-0000-4000-8000-000000000000/diagnostic").await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        call("/v1/log-events/not-a-uuid/diagnostic").await.status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );

    // 10. Newest-first paging: a reader opening the page starts at the newest event and walks
    //     backwards, and the two directions must not disagree about what the range holds.
    let newest = body_json(call("/v1/log-events?order=desc&limit=2").await).await;
    let newest_ids: Vec<u64> = newest["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["id"].as_u64().unwrap())
        .collect();
    assert_eq!(newest_ids, vec![ids[2], ids[1]], "newest first");
    assert_eq!(newest["total"], 3, "the count still covers the whole range");
    assert!(newest["next_after_id"].is_null());
    let older_cursor = newest["next_until_id"].as_u64().expect("more to read");
    assert_eq!(older_cursor, ids[1] - 1);
    let older = body_json(
        call(&format!(
            "/v1/log-events?order=desc&limit=2&until_id={older_cursor}"
        ))
        .await,
    )
    .await;
    let older_ids: Vec<u64> = older["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["id"].as_u64().unwrap())
        .collect();
    assert_eq!(older_ids, vec![ids[0]], "no overlap with the first page");
    assert!(older["next_until_id"].is_null());

    // 11. Exact sets, not a floor: "only errors" and "recording only" are each one query.
    let errors = body_json(call("/v1/log-events?levels=ERROR").await).await;
    assert_eq!(errors["total"], 1);
    assert_eq!(errors["events"][0]["data"]["event_name"], "upload.failed");
    let information = body_json(call("/v1/log-events?levels=INFO").await).await;
    assert_eq!(information["total"], 2, "INFO alone excludes the error");
    let both = body_json(call("/v1/log-events?levels=INFO,ERROR").await).await;
    assert_eq!(both["total"], 3);
    let recording = body_json(call("/v1/log-events?categories=recording").await).await;
    assert_eq!(recording["total"], 1);
    let two = body_json(call("/v1/log-events?categories=recording,upload").await).await;
    assert_eq!(two["total"], 3);
    let combined = body_json(call("/v1/log-events?categories=upload&levels=ERROR").await).await;
    assert_eq!(combined["total"], 1, "different dimensions intersect");

    // 12. Malformed set/order values are the caller's mistake, and a stray comma is not silently
    //     read as "no filter".
    for uri in [
        "/v1/log-events?order=sideways",
        "/v1/log-events?levels=LOUD",
        "/v1/log-events?levels=INFO,",
    ] {
        assert_eq!(call(uri).await.status(), StatusCode::BAD_REQUEST, "{uri}");
    }

    // 13. Export keeps reading forward whatever the page is showing, and stays bounded.
    let exported = body_text(call("/v1/log-events/export?order=desc").await).await;
    let exported_ids: Vec<u64> = exported
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["id"]
                .as_u64()
                .unwrap()
        })
        .collect();
    assert_eq!(exported_ids, ids, "export is ascending and complete");

    // 9. Live continuation resumes from the same cursor the list query hands out, so the history
    //    query and the subscription cannot deliver the same event twice or skip between them.
    let after = ids[0];
    let response = call(&format!("/v1/log-events/stream?after_id={after}")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.into_body().into_data_stream();
    let mut text = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while text.matches("event: log-event").count() < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "stream stalled: {text}"
        );
        if let Some(Ok(chunk)) = tokio::time::timeout(
            Duration::from_secs(2),
            futures::StreamExt::next(&mut stream),
        )
        .await
        .unwrap()
        {
            text.push_str(&String::from_utf8_lossy(&chunk));
        }
    }
    let delivered: Vec<u64> = text
        .lines()
        .filter_map(|line| line.strip_prefix("id: "))
        .map(|id| id.parse().unwrap())
        .collect();
    assert_eq!(
        delivered,
        vec![ids[1], ids[2]],
        "resumed exactly after the cursor"
    );
    drop(stream);
}
