use crate::server::errors::{AppError, report_to_response};
use crate::server::infrastructure::connection_pool::ConnectionPool;
use crate::server::infrastructure::models::Configuration;
use axum::Json;
use axum::extract::{Query, State};
use axum::response::Response;
use biliup::client::StatelessClient;
use biliup::uploader::credential::login_by_cookies;
use bytes::Bytes;
use error_stack::{Report, ResultExt};
use ormlite::Model;
use std::collections::HashMap;

/// B站投稿预处理端点
pub async fn archive_pre_endpoint(
    Query(_params): Query<HashMap<String, String>>,
    State(pool): State<ConnectionPool>,
) -> Result<Json<serde_json::Value>, Response> {
    // 获取所有B站Cookie配置
    let configurations = Configuration::select()
        .where_("key = 'bilibili-cookies'")
        .fetch_all(&pool)
        .await
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;

    // 尝试使用每个Cookie进行登录
    for cookies in configurations {
        if let Ok(bili) = login_by_cookies(cookies.value, None).await {
            return Ok(Json(
                bili.archive_pre()
                    .await
                    .change_context(AppError::Unknown)
                    .map_err(report_to_response)?,
            ));
        }
    }

    // 没有可用的Cookie
    Err(report_to_response(Report::from(AppError::Custom(
        "无可用 cookie 文件".to_string(),
    ))))
}

/// 获取B站用户信息端点
pub async fn get_myinfo_endpoint(
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, Response> {
    // 使用指定用户的Cookie登录
    let bili = login_by_cookies(&params["user"], None)
        .await
        .change_context(AppError::Custom(params["user"].to_string()))
        .map_err(report_to_response)?;

    // 获取用户信息
    Ok(Json(
        bili.my_info()
            .await
            .change_context(AppError::Unknown)
            .map_err(report_to_response)?,
    ))
}

/// 列出当前账号的视频合集（season）及分区，用于在「录播管理」里查到要填的 section_id。
/// 参数 user=cookie文件路径（如 data/16416555.json）。返回 data.seasons[i].sections.sections[j].id 即 section_id。
pub async fn get_seasons_endpoint(
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, Response> {
    let user = params
        .get("user")
        .map(|s| s.as_str())
        .unwrap_or("cookies.json");
    let bili = login_by_cookies(user, None)
        .await
        .change_context(AppError::Custom(user.to_string()))
        .map_err(report_to_response)?;
    Ok(Json(
        bili.list_seasons()
            .await
            .change_context(AppError::Unknown)
            .map_err(report_to_response)?,
    ))
}

/// 代理请求端点
pub async fn get_proxy_endpoint(
    State(client): State<StatelessClient>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Bytes, Response> {
    // 代理HTTP请求
    client
        .client
        .get(&params["url"])
        .send()
        .await
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?
        .bytes()
        .await
        .change_context(AppError::Unknown)
        .map_err(report_to_response)
}
