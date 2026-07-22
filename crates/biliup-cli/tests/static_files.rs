//! 静态文件接口的 HTTP 层集成测试。
//!
//! 用 `oneshot` 直接驱动路由，不起真实端口。只挂载静态文件子路由——它不依赖
//! 服务注册器，因此测试无需拖进数据库设施。
//!
//! 这里同时承担回归职责：该接口服务的是日志下载与录播视频回放，安全收紧
//! 不能以牺牲这两项功能为代价。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use biliup_cli::server::router::static_file_router;
use std::fs;
use std::path::{Path, PathBuf};
use tower::ServiceExt;

/// 造一个「工作目录」，放进日志与录播视频各一份；根目录之外另放一个文件用于越界测试。
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("opt");
    fs::create_dir(&root).unwrap();

    fs::write(root.join("ds_update.log"), b"log contents").unwrap();
    fs::write(root.join("2026-07-23 12点场.mp4"), b"fake video").unwrap();
    fs::write(tmp.path().join("secret.txt"), b"should never be served").unwrap();

    (tmp, root)
}

async fn fetch(root: &Path, uri: &str) -> (StatusCode, Vec<u8>) {
    let response = static_file_router(root.to_path_buf())
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();

    (status, body)
}

// 回归：日志下载必须照常工作。
#[tokio::test]
async fn serves_log_file() {
    let (_tmp, root) = fixture();
    let (status, body) = fetch(&root, "/static/ds_update.log").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"log contents");
}

// 回归：录播视频回放必须照常工作。历史记录页把文件名直接拼进播放器地址，
// 文件名里带空格与中文，因此这里用真实形态的文件名。
#[tokio::test]
async fn serves_recorded_video_with_spaces_and_cjk_name() {
    let (_tmp, root) = fixture();
    let (status, body) = fetch(&root, "/static/2026-07-23%2012%E7%82%B9%E5%9C%BA.mp4").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"fake video");
}

// 回归：拖动播放进度条依赖 HTTP Range。请求原样透传给 ServeFile 才有这个能力，
// 若日后有人改成「先把文件读进内存再返回」，这条会立刻失败。
#[tokio::test]
async fn serves_byte_range_for_video_seeking() {
    let (_tmp, root) = fixture();
    let response = static_file_router(root.clone())
        .oneshot(
            Request::builder()
                .uri("/static/2026-07-23%2012%E7%82%B9%E5%9C%BA.mp4")
                .header("Range", "bytes=5-8")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(&body[..], b"vide", "应只返回请求的字节区间");
}

#[tokio::test]
async fn rejects_parent_dir_traversal() {
    let (_tmp, root) = fixture();
    let (status, _) = fetch(&root, "/static/..%2Fsecret.txt").await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

// 大小写混用的百分号编码同样要被拦下。
#[tokio::test]
async fn rejects_traversal_with_mixed_case_encoding() {
    let (_tmp, root) = fixture();
    let (status, _) = fetch(&root, "/static/%2e%2e%2Fsecret.txt").await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn rejects_deep_traversal_to_system_file() {
    let (_tmp, root) = fixture();
    let (status, _) = fetch(&root, "/static/..%2F..%2F..%2F..%2Fetc%2Fpasswd").await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn rejects_absolute_path() {
    let (_tmp, root) = fixture();
    let (status, _) = fetch(&root, "/static/%2Fetc%2Fpasswd").await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

// 越界返回 403、不存在返回 404，两者分开，避免用状态码泄漏根目录外的文件是否存在。
#[tokio::test]
async fn reports_not_found_for_missing_file_inside_root() {
    let (_tmp, root) = fixture();
    let (status, _) = fetch(&root, "/static/nope.log").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

// 指向根目录外的符号链接：路径全由普通片段构成，只能靠解析后复查拦下。
#[cfg(unix)]
#[tokio::test]
async fn rejects_symlink_escaping_root() {
    let (tmp, root) = fixture();
    std::os::unix::fs::symlink(tmp.path().join("secret.txt"), root.join("escape.txt")).unwrap();

    let (status, _) = fetch(&root, "/static/escape.txt").await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}
