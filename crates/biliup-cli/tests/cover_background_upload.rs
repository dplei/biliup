//! 封面背景图上传接口的 HTTP 层集成测试。
//!
//! 沿用静态文件接口的测试方式：`oneshot` 直接驱动路由、不起真实端口，
//! 且只挂载上传子路由——它不依赖服务注册器，测试无需拖进数据库设施。
//!
//! 这里的重点是「传上来的东西真的落到了该落的地方，且只落在那里」：
//! 落盘位置、非图片内容、恶意文件名三件事各自锁一条。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use biliup_cli::server::api::cover_background::cover_background_router;
use std::fs;
use std::path::{Path, PathBuf};
use tower::ServiceExt;

/// 一张真实可解码的 1×1 PNG。用真图而非随便几个字节，是为了让「合法图片」
/// 这一侧的测试不依赖校验实现的宽严程度——无论校验只看文件头还是整张解码，它都该通过。
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0x00,
    0x00, 0x03, 0x01, 0x01, 0x00, 0xc9, 0xfe, 0x92, 0xef, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e,
    0x44, 0xae, 0x42, 0x60, 0x82,
];

const BOUNDARY: &str = "----biliuptestboundary";

/// 造一个背景图目录，外面另放一个哨兵文件用于探测越界写入。
///
/// 哨兵刻意用 `.png` 而非 `.txt`：扩展名白名单会先拦下 `.txt`，用它做穿越测试
/// 就变成了在测扩展名校验，路径处理其实一步都没跑到——那是条会一直通过的假测试。
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("data/cover-backgrounds");
    fs::create_dir_all(&root).unwrap();
    fs::write(tmp.path().join("outside.png"), b"original").unwrap();
    (tmp, root)
}

/// 手工拼一个单文件的 multipart 请求体。
fn multipart_body(file_name: &str, content: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(content);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

async fn upload(root: &Path, file_name: &str, content: &[u8]) -> (StatusCode, String) {
    let response = cover_background_router(root.to_path_buf())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/cover-backgrounds")
                .header(
                    "Content-Type",
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                )
                .body(Body::from(multipart_body(file_name, content)))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();

    (status, String::from_utf8_lossy(&body).into_owned())
}

// 合法图片上传成功，且文件确实落到了背景图目录里。
#[tokio::test]
async fn stores_uploaded_image_and_returns_file_name() {
    let (_tmp, root) = fixture();

    let (status, body) = upload(&root, "aurora.png", TINY_PNG).await;

    assert_eq!(status, StatusCode::OK, "body={body}");
    assert!(
        body.contains("aurora.png"),
        "接口应返回文件名供表单保存，实际返回 {body}"
    );
    assert_eq!(
        fs::read(root.join("aurora.png")).unwrap(),
        TINY_PNG,
        "文件应原样落在背景图目录内"
    );
}

// 扩展名装成图片、内容却不是——只认扩展名的实现会在这里放行。
#[tokio::test]
async fn rejects_non_image_content_disguised_by_extension() {
    let (_tmp, root) = fixture();

    let (status, _) = upload(&root, "payload.png", b"#!/bin/sh\nrm -rf /\n").await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        !root.join("payload.png").exists(),
        "被拒绝的内容不该留下文件"
    );
}

// 内容是真图片，但扩展名不在白名单里。
#[tokio::test]
async fn rejects_disallowed_extension() {
    let (_tmp, root) = fixture();

    let (status, _) = upload(&root, "aurora.svg", TINY_PNG).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!root.join("aurora.svg").exists());
}

// 恶意文件名不能在背景图目录之外产生文件，也不能改写已有的外部文件。
//
// 内容是真图片、扩展名也合法，问题只出在路径，因此校验的第一道关（`resolve_within`）
// 确实会跑到。但要说清楚：单段文件名检查同样拦得下它，两道关是冗余的。这条锁的是
// **行为**——外面不多出文件——而不是某一道关；独占锁住 `resolve_within` 的是下面
// 那条符号链接测试，只有它是别的关卡看不出问题的。
#[tokio::test]
async fn traversing_file_name_writes_nothing_outside_root() {
    let (tmp, root) = fixture();

    let (status, _) = upload(&root, "../../outside.png", TINY_PNG).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        fs::read(tmp.path().join("outside.png")).unwrap(),
        b"original",
        "根目录外的既有文件不该被改写"
    );
    assert!(
        !tmp.path().join("data/outside.png").exists(),
        "也不该在根目录的上级留下文件"
    );
}

// 绝对路径文件名同样要被拦下：`Path::join` 碰上绝对路径会把基路径整段丢掉，
// `join("/tmp/x.png")` 的结果就是 `/tmp/x.png`，一个文件名就能写到任意位置。
#[tokio::test]
async fn absolute_file_name_writes_nothing_outside_root() {
    let (tmp, root) = fixture();
    let victim = tmp.path().join("outside.png");

    let (status, _) = upload(&root, victim.to_str().unwrap(), TINY_PNG).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(fs::read(&victim).unwrap(), b"original");
}

// 背景解析侧（`background_path`）只认单段文件名，带子目录的传上来即使落了盘也用不了，
// 因此在入口就拒绝，避免出现「上传成功但投稿时背景不生效」的哑失败。
//
// 子目录真实存在，`resolve_within` 会放行（它没越界），所以这条独占锁住单段检查。
#[tokio::test]
async fn rejects_file_name_with_subdirectory() {
    let (_tmp, root) = fixture();
    fs::create_dir(root.join("nested")).unwrap();

    let (status, _) = upload(&root, "nested/aurora.png", TINY_PNG).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!root.join("nested/aurora.png").exists());
}

// 中间目录不存在时，`resolve_within` 报的是「根不可用」——但根目录明明刚建好，
// 成因只能是客户端给的文件名。这类请求必须回 400，不能因为错误名字里带 Root 就甩成 500。
//
// 这条同时钉住校验顺序：`nope/aurora.png` 两道关都会拒，但拒它的理由不同。
// 断言的是路径那一关给的话——路径校验一旦被排到后面，这里就会变成扩展名的说辞。
#[tokio::test]
async fn reports_bad_request_for_missing_intermediate_directory() {
    let (_tmp, root) = fixture();

    let (status, body) = upload(&root, "nope/aurora.png", TINY_PNG).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body.contains("文件名不合法"),
        "路径校验应排在扩展名校验之前，实际返回 {body}"
    );
}

// 同名再传一次即覆盖：换背景图时用户多半就是想替换掉旧的那张。
#[tokio::test]
async fn overwrites_existing_background_with_same_name() {
    let (_tmp, root) = fixture();
    fs::write(root.join("aurora.png"), b"stale").unwrap();

    let (status, _) = upload(&root, "aurora.png", TINY_PNG).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(fs::read(root.join("aurora.png")).unwrap(), TINY_PNG);
}

// 覆盖背景图目录里指向外部的符号链接——路径全由普通片段构成，
// 只有解析后复查才拦得下，这正是复用 `resolve_within` 的价值。
#[cfg(unix)]
#[tokio::test]
async fn rejects_overwriting_symlink_escaping_root() {
    let (tmp, root) = fixture();
    std::os::unix::fs::symlink(tmp.path().join("outside.png"), root.join("escape.png")).unwrap();

    let (status, _) = upload(&root, "escape.png", TINY_PNG).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        fs::read(tmp.path().join("outside.png")).unwrap(),
        b"original",
        "符号链接指向的外部文件不该被改写"
    );
}
