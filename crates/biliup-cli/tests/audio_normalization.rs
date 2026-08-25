use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use biliup_cli::server::api::audio_normalization::audio_normalization_router;
use std::path::Path;
use tower::ServiceExt;

async fn request(root: &Path, method: &str, uri: &str) -> axum::response::Response {
    audio_normalization_router(root.to_path_buf())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn sample_lifecycle_reports_pending_and_serves_without_cache() {
    let root = tempfile::tempdir().unwrap();

    let response = request(root.path(), "GET", "/v1/audio-normalization/sample").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let response = request(
        root.path(),
        "POST",
        "/v1/audio-normalization/sample/capture",
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let response = request(root.path(), "GET", "/v1/audio-normalization/sample/status").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let status: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status["sample_ready"], false);
    assert_eq!(status["capture_pending"], true);

    let sample_dir = root.path().join("audio-normalization");
    tokio::fs::write(sample_dir.join("sample.m4a"), b"sample")
        .await
        .unwrap();
    let response = request(root.path(), "GET", "/v1/audio-normalization/sample").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "audio/mp4");
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");

    assert_eq!(
        request(
            root.path(),
            "DELETE",
            "/v1/audio-normalization/sample/capture"
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        request(root.path(), "DELETE", "/v1/audio-normalization/sample")
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    // 重复操作仍是幂等成功。
    assert_eq!(
        request(root.path(), "DELETE", "/v1/audio-normalization/sample")
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
}
