use crate::server::api::bilibili_endpoints::{
    archive_pre_endpoint, get_myinfo_endpoint, get_proxy_endpoint, get_seasons_endpoint,
};
use crate::server::api::cover_background::cover_background_router;
use crate::server::api::cover_preview::cover_preview_router;
use crate::server::api::endpoints::{
    add_upload_streamer_endpoint, add_user_endpoint, delete_missing_upload,
    delete_streamers_endpoint, delete_template_endpoint, delete_user_endpoint, get_configuration,
    get_cookie_health, get_missing_uploads, get_qrcode, get_status, get_streamer_info,
    get_streamer_info_files, get_streamers_endpoint, get_upload_streamer_endpoint,
    get_upload_streamers_endpoint, get_users_endpoint, get_videos, login_by_qrcode,
    pause_streamers_endpoint, post_streamers_endpoint, post_uploads, put_configuration,
    put_streamers_endpoint, recover_missing_upload, retry_missing_upload,
};
use crate::server::common::path_safety::{PathRejection, resolve_within};
use crate::server::common::upload::BACKGROUND_DIR;
use crate::server::infrastructure::service_register::ServiceRegister;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use std::path::PathBuf;
use tower::ServiceExt;
use tower_http::services::ServeFile;
/// 创建应用程序路由
pub fn router(service_register: ServiceRegister) -> Router<()> {
    Router::new()
        // 主播管理相关路由
        .route(
            "/v1/streamers",
            get(get_streamers_endpoint) // 获取主播列表
                .post(post_streamers_endpoint) // 添加主播
                .put(put_streamers_endpoint), // 更新主播
        )
        .route("/v1/streamers/{id}", delete(delete_streamers_endpoint)) // 删除主播
        .route("/v1/streamers/{id}/pause", put(pause_streamers_endpoint))
        // 配置管理路由
        .route(
            "/v1/configuration",
            get(get_configuration).put(put_configuration), // 获取/更新配置
        )
        // 主播信息路由
        .route("/v1/streamer-info", get(get_streamer_info)) // 获取主播信息
        .route("/v1/streamer-info/files/{id}", get(get_streamer_info_files)) // 获取主播信息
        // 上传模板管理路由
        .route("/v1/upload/streamers", get(get_upload_streamers_endpoint)) // 获取上传模板列表
        .route(
            "/v1/upload/streamers/{id}",
            delete(delete_template_endpoint) // 删除上传模板
                .get(get_upload_streamer_endpoint), // 获取单个上传模板
        )
        .route("/v1/upload/streamers", post(add_upload_streamer_endpoint)) // 添加上传模板
        // 用户管理路由
        .route("/v1/users", get(get_users_endpoint).post(add_user_endpoint)) // 获取用户列表/添加用户
        .route("/v1/users/{id}", delete(delete_user_endpoint)) // 删除用户
        // B站API代理路由
        .route("/bili/archive/pre", get(archive_pre_endpoint)) // 投稿预处理
        .route("/bili/space/myinfo", get(get_myinfo_endpoint)) // 获取用户信息
        .route("/bili/seasons", get(get_seasons_endpoint)) // 列出视频合集（查 section_id）
        .route("/bili/proxy", get(get_proxy_endpoint)) // 代理请求
        // 认证相关路由
        .route("/v1/get_qrcode", get(get_qrcode)) // 获取二维码
        .route("/v1/login_by_qrcode", post(login_by_qrcode)) // 二维码登录
        // 视频文件管理路由
        .route("/v1/videos", get(get_videos)) // 获取视频列表
        .route("/v1/status", get(get_status))
        .route("/v1/health/cookie", get(get_cookie_health)) // cookie 健康状态（前端横幅轮询）
        .route("/v1/uploads/missing", get(get_missing_uploads))
        .route("/v1/uploads/missing/{id}", delete(delete_missing_upload))
        .route(
            "/v1/uploads/missing/{id}/recover",
            post(recover_missing_upload),
        )
        .route("/v1/uploads/missing/{id}/retry", post(retry_missing_upload))
        .route("/v1/uploads", post(post_uploads))
        .with_state(service_register) // 注入服务注册器状态
        .merge(static_file_router(default_static_root()))
        .merge(cover_background_router(PathBuf::from(BACKGROUND_DIR)))
        .merge(cover_preview_router(PathBuf::from(BACKGROUND_DIR)))
}

/// 静态文件子路由：服务工作目录下的日志与录播视频。
///
/// 单独拆出来有两个原因：一是它不依赖服务注册器（也就不依赖数据库），
/// 二是这样集成测试可以只挂载它、并指定自己的根目录，不必拖进整套数据库设施。
pub fn static_file_router(root: PathBuf) -> Router<()> {
    Router::new()
        .route("/static/{path}", get(serve_static_file))
        .with_state(root)
}

/// 生产环境的静态文件根目录：服务的工作目录（容器内即挂载卷 `/opt`）。
fn default_static_root() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|e| {
        tracing::error!(error = ?e, "无法获取工作目录，静态文件根目录回退为当前路径");
        PathBuf::from(".")
    })
}

/// 按用户提供的路径返回根目录内的文件。
///
/// 历史上这里直接把用户路径交给 `ServeFile`，等同于任意文件读取；
/// 现在一律先经 `resolve_within` 收口。注意根目录是工作目录而非某个图片目录——
/// 该接口同时承担日志下载与录播视频回放，收窄会直接破坏这两项功能。
async fn serve_static_file(
    State(root): State<PathBuf>,
    axum::extract::Path(path): axum::extract::Path<String>,
    request: Request<Body>,
) -> Response {
    match resolve_within(&root, &path) {
        // 目标不存在也会走到这里，由 ServeFile 自己回 404。
        Ok(resolved) => match ServeFile::new(resolved).oneshot(request).await {
            Ok(response) => response.into_response(),
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        // 根目录不可用是服务端配置问题，不是用户输入的错，据实返回 500 便于运维定位。
        Err(PathRejection::RootUnavailable) => {
            tracing::error!(root = ?root, "静态文件根目录不可用");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        // 越界一律 403 且不区分原因，避免用状态码泄漏根目录外的文件是否存在。
        Err(rejection) => {
            tracing::warn!(?rejection, path = %path, "拒绝越界的静态文件请求");
            StatusCode::FORBIDDEN.into_response()
        }
    }
}
