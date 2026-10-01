use crate::server::config::Config;
use crate::server::errors::{AppError, report_to_response};
use crate::server::infrastructure::connection_pool::ConnectionPool;
use crate::server::infrastructure::models::Configuration;
use crate::server::infrastructure::models::live_streamer::LiveStreamer;
use crate::server::infrastructure::models::upload_streamer::UploadStreamer;
use axum::Json;
use axum::extract::{Query, State};
use axum::response::Response;
use biliup::client::StatelessClient;
use biliup::uploader::credential::login_by_cookies;
use bytes::Bytes;
use error_stack::{Report, ResultExt};
use ormlite::Model;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use struct_patch::Patch;

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
/// 参数 user=cookie文件路径（如 data/<UID>.json）。返回 data.seasons[i].sections.sections[j].id 即 section_id。
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

/// 预演历史稿件补录进合集：只列出每个主播待加入的 aid，不写 B 站。参数 id=主播id（可选）。
pub async fn season_backfill_preview_endpoint(
    State(pool): State<ConnectionPool>,
    State(config): State<Arc<RwLock<Config>>>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, Response> {
    season_backfill(&pool, &config, &params, false).await
}

/// 执行历史稿件补录进合集。已在合集里的跳过，可重复执行。参数同预演。
pub async fn season_backfill_run_endpoint(
    State(pool): State<ConnectionPool>,
    State(config): State<Arc<RwLock<Config>>>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, Response> {
    season_backfill(&pool, &config, &params, true).await
}

/// 按主播把 upload_session 里的历史 aid 补进该主播生效配置里的 season_section_id。
/// 没配 season_section_id 的主播跳过；单个主播出错只记在它自己那一项里。
async fn season_backfill(
    pool: &ConnectionPool,
    config: &Arc<RwLock<Config>>,
    params: &HashMap<String, String>,
    execute: bool,
) -> Result<Json<serde_json::Value>, Response> {
    let only: Option<i64> = params.get("id").and_then(|s| s.parse().ok());
    let streamers = LiveStreamer::select()
        .fetch_all(pool)
        .await
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;
    let base = config.read().unwrap().clone();
    let mut report = Vec::new();
    for ls in streamers
        .into_iter()
        .filter(|s| only.is_none_or(|id| s.id == id))
    {
        let mut cfg = base.clone();
        if let Some(o) = ls.override_cfg.clone() {
            cfg.apply(o);
        }
        let Some(section_id) = cfg.season_section_id else {
            continue;
        };
        let mut entry = match backfill_streamer(pool, &ls, section_id, execute).await {
            Ok(v) => v,
            Err(e) => json!({ "error": e }),
        };
        entry["id"] = json!(ls.id);
        entry["remark"] = json!(ls.remark);
        entry["section_id"] = json!(section_id);
        report.push(entry);
    }
    Ok(Json(json!({ "execute": execute, "streamers": report })))
}

async fn backfill_streamer(
    pool: &ConnectionPool,
    ls: &LiveStreamer,
    section_id: i64,
    execute: bool,
) -> Result<serde_json::Value, String> {
    let template_id = ls.upload_streamers_id.ok_or("未挂投稿模板")?;
    let template = UploadStreamer::select()
        .where_("id = ?")
        .bind(template_id)
        .fetch_one(pool)
        .await
        .map_err(|e| format!("读取投稿模板失败: {e}"))?;
    let cookie = template.user_cookie.as_deref().unwrap_or("cookies.json");
    let bili = login_by_cookies(cookie, None)
        .await
        .map_err(|e| format!("cookie 登录失败({cookie}): {e:?}"))?;

    let db_aids: Vec<i64> = sqlx::query_scalar(
        "SELECT aid FROM upload_session WHERE live_streamer_id = ? AND aid IS NOT NULL \
         ORDER BY created_at, id",
    )
    .bind(ls.id)
    .fetch_all(pool)
    .await
    .map_err(|e| format!("读取 upload_session 失败: {e}"))?;
    let existing = bili
        .list_section_aids(section_id)
        .await
        .map_err(|e| format!("读取合集分区失败: {e:?}"))?;
    let to_add = backfill_plan(&db_aids, &existing);

    let mut entry = json!({
        "cookie": cookie,
        "db_aids": db_aids.len(),
        "already_in_section": existing.len(),
        "to_add": to_add,
    });
    if !execute {
        return Ok(entry);
    }
    let (mut added, mut failed) = (Vec::new(), Vec::new());
    for aid in to_add {
        match bili.add_archive_to_season(section_id, aid).await {
            Ok(()) => added.push(aid),
            Err(e) => failed.push(json!({ "aid": aid, "error": format!("{e:?}") })),
        }
        // 逐个加，给创作中心接口留点间隔，避免触发频控
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    entry["added"] = json!(added);
    entry["failed"] = json!(failed);
    Ok(entry)
}

/// 待加入合集的 aid：保持库里的时间顺序、去重、跳过已在合集里的。
fn backfill_plan(db_aids: &[i64], existing: &[u64]) -> Vec<u64> {
    let mut seen: HashSet<u64> = existing.iter().copied().collect();
    db_aids
        .iter()
        .filter_map(|&a| u64::try_from(a).ok())
        .filter(|a| seen.insert(*a))
        .collect()
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

#[cfg(test)]
mod tests {
    use super::backfill_plan;

    #[test]
    fn backfill_plan_keeps_order_dedupes_and_skips_existing() {
        // 同一稿件多场续接会出现多行；3 已在合集里
        assert_eq!(backfill_plan(&[5, 3, 5, 1, 7], &[3]), vec![5, 1, 7]);
        assert!(backfill_plan(&[], &[1]).is_empty());
    }
}
