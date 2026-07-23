//! 封面预览接口。
//!
//! 改完封面文字模板点一下就能看到成品，不必等下一次下播走完整条上传流程才发现排版有问题。
//! 渲染走的是 `render_cover`——与投稿时**同一个函数**，因此预览好看就代表实际产出好看。
//!
//! 接口只吃两个显式参数（文字模板、背景文件名），**不读数据库**。这是刻意的：
//! 模板页和主播页各有各的那一级取值，不读库才能两边复用同一个接口，
//! 也不会出现「预览的是库里的旧值、用户改了还没保存」这种对不上的情况。

use crate::server::common::cover_generator::{
    Background, CoverOptions, render_cover, split_template_lines,
};
use crate::server::common::path_safety::{PathRejection, resolve_within, single_segment_name};
use crate::server::common::util::Recorder;
use crate::server::infrastructure::models::StreamerInfo;
use axum::Router;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::Utc;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tracing::{error, warn};

/// 预览参数。两项都来自表单当前的值，不查库。
#[derive(Debug, Deserialize)]
struct PreviewParams {
    /// 封面文字模板原文，占位符尚未展开。空串合法（只想看看背景铺上去什么样）；
    /// 整项缺失则是调用方写错了，交由 axum 的 Query 提取器回 400。
    template: String,
    /// 背景图文件名。缺省或空白 = 纯黑底，与投稿时未配置背景的产出一致。
    background: Option<String>,
}

/// 封面预览子路由。
///
/// 与上传、静态文件两个子路由同一形状：根目录做成参数而非写死，
/// 集成测试才能只挂它、指定临时目录，不必拖进整套数据库设施。
pub fn cover_preview_router(root: PathBuf) -> Router<()> {
    Router::new()
        .route("/v1/cover-preview", get(preview_cover))
        .with_state(root)
}

/// 渲染一张预览封面，直接返回 JPG 字节。
///
/// 做成 GET 而不是 POST：产出就是一张图，`<img src>` 能直接吃，
/// 前端不必绕 blob 与 object URL 那一圈。
async fn preview_cover(
    State(root): State<PathBuf>,
    Query(params): Query<PreviewParams>,
) -> Response {
    let background = match resolve_background(&root, params.background.as_deref()) {
        Ok(background) => background,
        Err(message) => return bad_request(message),
    };

    // 占位符展开必须能失败：模板是用户当场敲进输入框的，
    // chrono 遇上非法格式串是 panic 而不是报错（详见 `try_format`）。
    let Some(text) = sample_recorder().try_format(&params.template) else {
        return bad_request("时间占位符不合法，请检查 % 的写法");
    };

    let lines = split_template_lines(&text);
    let opts = CoverOptions {
        background,
        ..CoverOptions::default()
    };

    // 丢到阻塞线程池：解码 + Lanczos3 缩放 + JPEG 编码是实打实的 CPU 活儿，
    // 一张 10 MB 的大图能跑上几百毫秒。直接在 async 上下文里同步跑会占住一个
    // worker 线程，把同期的录播状态推送一起卡住。
    let rendered = tokio::task::spawn_blocking(move || render_cover(&lines, &opts)).await;

    match rendered {
        Ok(Ok(bytes)) => (
            [
                (header::CONTENT_TYPE, "image/jpeg"),
                // 参数相同但背景图可能刚被换掉，缓存会让用户看到上一张。
                (header::CACHE_CONTROL, "no-store"),
            ],
            bytes,
        )
            .into_response(),
        Ok(Err(e)) => {
            error!(error = ?e, "渲染预览封面失败");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        Err(e) => {
            error!(error = ?e, "预览封面渲染任务异常终止");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// 把「背景文件名」参数变成渲染器要的背景，或给出一句可直接显示的拒绝理由。
///
/// 两道关，顺序有讲究：
/// 1. **越界**——文件名是纯粹的客户端输入，先收口再谈其他。排在别的校验后面的话，
///    那些校验就成了事实上的第一道关，安全函数退化成走过场。
/// 2. **单段文件名**——与投稿侧对齐。带目录的值那边会被当成「没填」，
///    这里若放行，就成了「预览好看、投稿黑底」，「预览即产出」的承诺当场作废。
fn resolve_background(root: &Path, value: Option<&str>) -> Result<Background, &'static str> {
    // 空串等同没填：表单清空后提交的就是空串，不能当成「配了一张名为空的图」。
    let Some(name) = value.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(Background::Black);
    };

    let resolved = resolve_within(root, name);

    if let Err(
        rejection @ (PathRejection::Absolute | PathRejection::Traversal | PathRejection::Escapes),
    ) = &resolved
    {
        warn!(?rejection, file_name = %name, "拒绝越界的预览背景图文件名");
        return Err("背景图文件名不合法");
    }

    // 刻意不与上面那道合并：`resolve_within` 对「中间目录不存在」报的是
    // `RootUnavailable`，若在那里就返回，`nope/aurora.png` 这类值会绕过单段检查。
    if single_segment_name(name).is_none() {
        return Err("背景图必须是背景图目录下的单个文件名，不能带目录");
    }

    match resolved {
        Ok(path) => Ok(Background::Image(path)),
        // 走到这里只剩 `RootUnavailable`，且文件名已确认是单段的——
        // 成因是背景图目录还没建起来（一次都没上传过）。那张图必然读不出来，
        // 与投稿时一样回退纯黑：服务端的目录状态不该被报成用户的参数错误。
        //
        // 文件不存在则压根不会走到这里：`resolve_within` 放行尚不存在的目标，
        // 由渲染器自己回退纯黑——同样与投稿一致，用户当场就看见背景没生效。
        Err(_) => Ok(Background::Black),
    }
}

/// 预览用的示例主播信息。
///
/// 接口不读数据库，占位符只能拿一组示例值展开。这不是缺陷而是取舍：用户要确认的是
/// 排版——几行、多长、会不会顶到边——示例值撑起的字数与真实值同量级，足够看出来。
///
/// 时间取当前时刻：`%Y-%m-%d %H点场` 这类写法今天会渲染成什么样，正是用户想确认的。
fn sample_recorder() -> Recorder {
    Recorder::new(
        None,
        StreamerInfo::new(
            "示例主播",
            "https://live.bilibili.com/000000",
            "示例直播间标题",
            Utc::now(),
            "",
        ),
    )
}

/// 校验失败一律 400 + 一句人话，让表单能直接把原因显示出来。
fn bad_request(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, message.to_owned()).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_values_expand_placeholders() {
        let text = sample_recorder().try_format("{streamer}｜{title}").unwrap();

        assert_eq!(text, "示例主播｜示例直播间标题");
    }

    /// 示例值本身绝不能带 `%`，否则会被 chrono 当成时间占位符再解释一遍，
    /// 用户看到的预览就不是自己写的模板了。
    #[test]
    fn sample_values_contain_no_format_specifiers() {
        let text = sample_recorder()
            .try_format("{streamer} {title} {url}")
            .unwrap();

        assert!(!text.contains('%'), "示例值不该引入格式说明符，实际 {text}");
    }
}
