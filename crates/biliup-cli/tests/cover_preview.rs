//! 封面预览接口的 HTTP 层集成测试。
//!
//! 沿用前两个子路由的测试方式：`oneshot` 直接驱动路由、不起真实端口，
//! 且只挂载预览子路由——它不读数据库，测试无需拖进服务注册器。
//!
//! 这里锁的是「预览出来的就是投稿时会产出的那张图」：合法 JPEG、背景确实被用上、
//! 没配背景时是纯黑，以及参数不合法时据实报错而不是悄悄出一张图。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use biliup_cli::server::api::cover_preview::cover_preview_router;
use image::{ImageReader, Rgb, RgbImage};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use tower::ServiceExt;

/// 造一个背景图目录，外面另放一张图用于探测越界读取。
///
/// 外面那张刻意也是合法图片：若路径校验失效，接口会真把它渲染出来，
/// 测试就能从像素上看出越界——用一个坏文件的话，渲染器会回退纯黑，
/// 越界与「读不到」两种结果长得一模一样，测了等于没测。
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("data/cover-backgrounds");
    std::fs::create_dir_all(&root).unwrap();
    write_image(&tmp.path().join("outside.png"), Rgb([200, 30, 30]));
    (tmp, root)
}

fn write_image(path: &Path, color: Rgb<u8>) {
    RgbImage::from_pixel(1146, 717, color).save(path).unwrap();
}

async fn preview(root: &Path, query: &str) -> (StatusCode, Vec<u8>) {
    let response = cover_preview_router(root.to_path_buf())
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/cover-preview?{query}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();

    (status, body.to_vec())
}

fn decode(bytes: &[u8]) -> RgbImage {
    ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .unwrap()
        .decode()
        .unwrap()
        .to_rgb8()
}

/// 左上角：文字居中，绝不会画到角落，因此角落颜色只反映背景。
fn corner(bytes: &[u8]) -> Rgb<u8> {
    *decode(bytes).get_pixel(0, 0)
}

// 给定文字与背景，返回一张合法 JPEG，且背景确实是给的那张图。
#[tokio::test]
async fn renders_jpeg_from_template_and_background() {
    let (_tmp, root) = fixture();
    write_image(&root.join("aurora.png"), Rgb([10, 120, 200]));

    let (status, bytes) = preview(&root, "template=%E4%B8%BB%E6%92%AD&background=aurora.png").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        image::guess_format(&bytes).unwrap(),
        image::ImageFormat::Jpeg,
        "接口应直接返回 JPG 字节"
    );
    assert_eq!(decode(&bytes).dimensions(), (1146, 717));

    let px = corner(&bytes);
    assert!(px[2] > px[0], "角落应取自蓝色背景图，实际 {px:?}");
}

// 背景参数缺省 → 纯黑底，与投稿时未配置背景的产出一致。
#[tokio::test]
async fn renders_black_cover_when_background_omitted() {
    let (_tmp, root) = fixture();

    let (status, bytes) = preview(&root, "template=%E4%B8%BB%E6%92%AD").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(decode(&bytes).dimensions(), (1146, 717));
    assert_eq!(corner(&bytes), Rgb([0, 0, 0]), "未配背景时应为纯黑底");
}

// 空字符串等同于没填——前端表单清空后提交的就是空串。
#[tokio::test]
async fn treats_blank_background_as_omitted() {
    let (_tmp, root) = fixture();

    let (status, bytes) = preview(&root, "template=%E4%B8%BB%E6%92%AD&background=%20").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(corner(&bytes), Rgb([0, 0, 0]));
}

// 穿越写法必须被拒。断言状态码之外还断言没渲染出外面那张红图——
// 只看状态码的话，一个「凑巧也回 400」的实现会掩盖真正的越界。
#[tokio::test]
async fn rejects_traversing_background_name() {
    let (_tmp, root) = fixture();

    let (status, bytes) = preview(&root, "template=x&background=..%2F..%2Foutside.png").await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        image::guess_format(&bytes).is_err(),
        "被拒的请求不该返回图片"
    );
}

// 绝对路径同样要拦：`Path::join` 碰上绝对路径会把基路径整段丢掉。
#[tokio::test]
async fn rejects_absolute_background_name() {
    let (tmp, root) = fixture();
    let victim = urlencoding::encode(tmp.path().join("outside.png").to_str().unwrap()).into_owned();

    let (status, _) = preview(&root, &format!("template=x&background={victim}")).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// 带子目录的文件名：投稿侧只认单段文件名，这里预览得出图、投稿却回退纯黑的话，
// 「预览即产出」的承诺就是假的。所以入口就拒绝，即便子目录真实存在。
#[tokio::test]
async fn rejects_background_with_subdirectory() {
    let (_tmp, root) = fixture();
    std::fs::create_dir(root.join("nested")).unwrap();
    write_image(&root.join("nested/aurora.png"), Rgb([10, 120, 200]));

    let (status, _) = preview(&root, "template=x&background=nested%2Faurora.png").await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// 文件名合法但图不存在：与投稿时一致地回退纯黑，而不是报错。
// 预览的价值正在于此——用户当场就看见背景没生效。
#[tokio::test]
async fn falls_back_to_black_when_background_file_missing() {
    let (_tmp, root) = fixture();

    let (status, bytes) = preview(&root, "template=x&background=nope.png").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(corner(&bytes), Rgb([0, 0, 0]));
}

// 时间占位符写错（例如落单的 `%`）必须回 400。
// 这条不只是参数校验：chrono 对非法格式串是 panic 而非报错，
// 没有这道校验的话，用户在输入框里手滑一个 `%` 就能把请求打崩。
#[tokio::test]
async fn rejects_invalid_time_placeholder() {
    let (_tmp, root) = fixture();

    let (status, _) = preview(&root, "template=%E4%B8%BB%E6%92%AD%20%25").await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// 文字模板整项缺失 → 400。空模板是合法的（只看背景），缺项则是调用方写错了。
#[tokio::test]
async fn rejects_missing_template_parameter() {
    let (_tmp, root) = fixture();

    let (status, _) = preview(&root, "background=aurora.png").await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// 空模板合法：只想看看背景图铺上去是什么样，不必先编一段文字。
#[tokio::test]
async fn accepts_empty_template() {
    let (_tmp, root) = fixture();
    write_image(&root.join("aurora.png"), Rgb([10, 120, 200]));

    let (status, bytes) = preview(&root, "template=&background=aurora.png").await;

    assert_eq!(status, StatusCode::OK);
    let px = corner(&bytes);
    assert!(px[2] > px[0], "空文字也该照常铺背景，实际 {px:?}");
}

// 换行标记要按投稿时同一套规则切行：字面 `\n` 切成两行。
// 两行文字比一行矮、字号也不同，因此中心行的像素分布必然不同——
// 用「两种模板渲染结果不相等」来锁住切行确实发生了。
#[tokio::test]
async fn splits_lines_on_literal_backslash_n() {
    let (_tmp, root) = fixture();

    let (_, one_line) = preview(&root, "template=AAAA%20BBBB").await;
    let (_, two_lines) = preview(&root, "template=AAAA%5CnBBBB").await;

    assert_ne!(one_line, two_lines, "字面 \\n 应当切成两行，排版必然不同");
}
