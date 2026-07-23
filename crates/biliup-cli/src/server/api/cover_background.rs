//! 封面背景图上传接口。
//!
//! 在网页上选一张图直接传上来，省掉「登录服务器 + scp」那一趟。落盘位置就是
//! 投稿时解析背景用的那个目录，接口回一个文件名，表单把它存进背景字段即可。
//!
//! 路径安全一律走 `path_safety::resolve_within`，与静态文件接口同一个函数、
//! 只是根目录不同——两处规则共用一份实现才不会随时间漂移。

use crate::server::common::path_safety::{resolve_within, single_segment_name};
use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, Multipart, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::json;
use std::path::{Path, PathBuf};
use tracing::{error, warn};

/// 单个背景图的体积上限。背景图渲染出来是 1146×717，正常成品也就几百 KB；
/// 留到 10 MB 是给未压缩的原图一点余量，同时不至于让一次请求吃掉过多内存
/// （multipart 字段是整段读进内存再落盘的）。
const MAX_UPLOAD_BYTES: usize = 10 * 1024 * 1024;

/// multipart 里承载文件的字段名。与前端上传控件的 `name` 一致。
const FILE_FIELD: &str = "file";

/// 允许的扩展名，以及各自对应的真实图片格式。
///
/// 白名单而非黑名单：渲染侧用 `image` 解码，能解的格式远多于这几种，但背景图
/// 只需要常见位图；放开一堆用不上的格式只是白白扩大攻击面。
const ALLOWED_EXTENSIONS: &[(&str, image::ImageFormat)] = &[
    ("jpg", image::ImageFormat::Jpeg),
    ("jpeg", image::ImageFormat::Jpeg),
    ("png", image::ImageFormat::Png),
    ("webp", image::ImageFormat::WebP),
];

/// 背景图上传子路由。
///
/// 和静态文件子路由一样把根目录做成参数而非写死：它不依赖服务注册器，
/// 集成测试可以只挂载它、指定临时目录，不必拖进整套数据库设施。
pub fn cover_background_router(root: PathBuf) -> Router<()> {
    Router::new()
        .route("/v1/cover-backgrounds", post(upload_cover_background))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .with_state(root)
}

/// 接收单个图片文件，校验通过后落盘到背景图目录，返回文件名。
///
/// 校验分三道，顺序是有讲究的：
/// 1. **路径**经 `resolve_within` 收口——文件名是纯粹的客户端输入，先收口再谈其他。
///    放在最前面不只是习惯：越界判断一旦排在别的校验后面，那些校验就成了事实上的
///    第一道关，安全函数退化成走过场，测试也会跟着测不到它。
/// 2. **单段 + 扩展名**——与背景解析侧对齐，并决定渲染侧按什么格式解码。
/// 3. **文件头**与扩展名对得上——只看扩展名的话，一个改了名的脚本就能落进目录。
async fn upload_cover_background(
    State(root): State<PathBuf>,
    mut multipart: Multipart,
) -> Response {
    let Some(file) = next_file_field(&mut multipart).await else {
        return bad_request("请求里没有找到上传的文件");
    };

    // 首次上传时目录还不存在（数据卷里初始只有数据库）。必须建在路径校验之前——
    // `resolve_within` 要 canonicalize 根目录，目录不存在它只会报「根不可用」。
    if let Err(e) = tokio::fs::create_dir_all(&root).await {
        error!(error = ?e, root = ?root, "创建背景图目录失败");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // 根目录刚刚已确保存在，所以这里的 `RootUnavailable` 不再是服务端配置问题，
    // 只可能是文件名里带了不存在的中间目录——那是客户端的错，与越界一并归到 400。
    let target = match resolve_within(&root, &file.file_name) {
        Ok(target) => target,
        Err(rejection) => {
            warn!(?rejection, file_name = %file.file_name, "拒绝越界的背景图文件名");
            return bad_request("文件名不合法");
        }
    };

    let Some(expected) = expected_format(&file.file_name) else {
        return bad_request("只支持 jpg / jpeg / png / webp 格式的背景图，且不能带目录");
    };

    // 文件头必须与扩展名对得上。`guess_format` 只读魔数、不解码整张图——
    // 既够用来识破伪装，也不会让一张解压炸弹在这里就把内存吃光。
    match image::guess_format(&file.content) {
        Ok(actual) if actual == expected => {}
        _ => return bad_request("文件内容不是与扩展名相符的图片"),
    }

    // 同名即覆盖：换一张背景时用户多半就是想替换掉旧的那张，
    // 自动改名反而会在目录里堆出一串没人认得的 `aurora-1.jpg`。
    if let Err(e) = tokio::fs::write(&target, &file.content).await {
        error!(error = ?e, target = ?target, "写入背景图失败");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // 只回文件名：库里存的就是文件名，路径由运行时拼接，换挂载点时数据无需跟着改。
    Json(json!({ "file_name": file.file_name })).into_response()
}

/// multipart 里那个承载文件的字段。
struct UploadedFile {
    file_name: String,
    content: Vec<u8>,
}

/// 取出 multipart 里承载文件的那个字段。
///
/// 逐个字段找而不是只看第一个：不同上传控件塞进来的辅助字段顺序并不固定。
async fn next_file_field(multipart: &mut Multipart) -> Option<UploadedFile> {
    while let Ok(Some(field)) = multipart.next_field().await {
        if field.name() != Some(FILE_FIELD) {
            continue;
        }
        let file_name = field.file_name()?.to_owned();
        let content = field.bytes().await.ok()?.to_vec();
        return Some(UploadedFile { file_name, content });
    }
    None
}

/// 文件名必须是**单段**且扩展名在白名单里，返回它应有的图片格式。
///
/// 单段这条不承担安全职责——越界在此之前已由 `resolve_within` 挡掉了——它是与背景
/// 解析侧对齐：`upload::background_path` 只认单段文件名，带子目录的即使落了盘，
/// 投稿时也会被当成「没填」而悄悄回退纯黑。在入口就拒绝，把哑失败变成明确的报错。
fn expected_format(file_name: &str) -> Option<image::ImageFormat> {
    single_segment_name(file_name)?;

    let extension = Path::new(file_name)
        .extension()?
        .to_str()?
        .to_ascii_lowercase();
    ALLOWED_EXTENSIONS
        .iter()
        .find(|(name, _)| *name == extension)
        .map(|(_, format)| *format)
}

/// 校验失败一律 400 + 一句人话，让表单能直接把原因显示出来。
fn bad_request(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, message.to_owned()).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_allowed_extensions_case_insensitively() {
        assert_eq!(
            expected_format("Aurora.JPG"),
            Some(image::ImageFormat::Jpeg)
        );
        assert_eq!(expected_format("aurora.png"), Some(image::ImageFormat::Png));
        assert_eq!(
            expected_format("aurora.webp"),
            Some(image::ImageFormat::WebP)
        );
    }

    #[test]
    fn rejects_extension_outside_allow_list() {
        // svg 会被 image 拒绝解码，gif 能解但用不上——都不该进白名单。
        assert_eq!(expected_format("aurora.svg"), None);
        assert_eq!(expected_format("aurora.gif"), None);
        assert_eq!(expected_format("aurora"), None);
    }

    /// 带目录的文件名走不到背景解析侧，因此在这里就要拒掉。
    /// 注意这不是越界防线——那道在 `resolve_within`，且排在本函数之前。
    #[test]
    fn rejects_multi_segment_file_names() {
        assert_eq!(expected_format("nested/aurora.png"), None);
        assert_eq!(expected_format("../aurora.png"), None);
        assert_eq!(expected_format("/etc/aurora.png"), None);
    }
}
