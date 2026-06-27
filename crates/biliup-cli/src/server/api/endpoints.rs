use crate::server::common::missing_segment::{
    MissingSegmentDeleteClaim, claim_missing_segment_for_delete, remove_missing_segment_files,
};
use crate::server::common::upload::{
    build_studio, manual_recover_missing_segment, retry_missing_segment, submit_to_bilibili, upload,
};
use crate::server::common::upload_session::missing_status_where;
use crate::server::common::util::Recorder;
use crate::server::config::Config;
use crate::server::core::download_manager::DownloadManager;
use crate::server::errors::{AppError, report_to_response};
use crate::server::infrastructure::connection_pool::ConnectionPool;
use crate::server::infrastructure::context::{Stage, WorkerStatus};
use crate::server::infrastructure::dto::LiveStreamerResponse;
use crate::server::infrastructure::models::UploadMissingSegment;
use crate::server::infrastructure::models::live_streamer::{InsertLiveStreamer, LiveStreamer};
use crate::server::infrastructure::models::upload_streamer::{
    InsertUploadStreamer, UploadStreamer,
};
use crate::server::infrastructure::models::{
    Configuration, FileItem, InsertConfiguration, StreamerInfo,
};
use crate::server::infrastructure::repositories::{
    del_streamer, get_all_streamer, get_upload_config,
};
use crate::server::infrastructure::service_register::ServiceRegister;
use crate::{LogHandle, UploadLine};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use biliup::credential::Credential;
use chrono::Utc;
use clap::ValueEnum;
use error_stack::{Report, ResultExt};
use ormlite::{Insert, Model};
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, UNIX_EPOCH};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

pub async fn get_streamers_endpoint(
    State(pool): State<ConnectionPool>,
    State(managers): State<Arc<DownloadManager>>,
) -> Result<Json<Vec<LiveStreamerResponse>>, Response> {
    let live_streamers = get_all_streamer(&pool).await.map_err(report_to_response)?;
    let mut results = Vec::new();
    let workers = managers.get_rooms().await;
    for x in live_streamers {
        let option = workers
            .clone()
            .into_iter()
            .find(|worker| worker.live_streamer.id == x.id);

        let status = match option.as_ref() {
            Some(t) => format!("{:?}", *t.downloader_status.read().unwrap()),
            None => String::new(),
        };

        let recording_quality = option.as_ref().and_then(|t| t.recording_quality());

        results.push(LiveStreamerResponse {
            status,
            inner: x,
            upload_status: option
                .map(|t| format!("{:?}", *t.uploader_status.read().unwrap()))
                .unwrap_or_default(),
            recording_quality,
        });
    }
    Ok(Json(results))
}

pub async fn post_streamers_endpoint(
    State(service_register): State<ServiceRegister>,
    State(managers): State<Arc<DownloadManager>>,
    State(pool): State<ConnectionPool>,
    Json(payload): Json<InsertLiveStreamer>,
) -> Result<Json<LiveStreamer>, Response> {
    let url = &payload.url.clone();
    // You can insert the model directly.
    let live_streamers = payload
        .insert(&pool)
        .await
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;
    let upload_config = get_upload_config(&pool, live_streamers.id)
        .await
        .map_err(report_to_response)?;
    let Some(_) = managers
        .add_room(service_register.worker(live_streamers.clone(), upload_config))
        .await
    else {
        info!("not supported url: {}", url);
        return Err((StatusCode::BAD_REQUEST, "Not supported url").into_response());
    };

    info!(url = url, "successfully inserted new live streamers");
    Ok(Json(live_streamers))
}

pub async fn put_streamers_endpoint(
    State(service_register): State<ServiceRegister>,
    State(managers): State<Arc<DownloadManager>>,
    State(pool): State<ConnectionPool>,
    Json(payload): Json<LiveStreamer>,
) -> Result<Json<LiveStreamer>, Response> {
    let streamer = payload
        .update_all_fields(&pool)
        .await
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;

    let id = streamer.id;
    managers.del_room(id).await;

    let upload_config = get_upload_config(&pool, id)
        .await
        .map_err(report_to_response)?;

    managers
        .add_room(service_register.worker(streamer.clone(), upload_config))
        .await
        .ok_or(AppError::Unknown)
        .map_err(report_to_response)?;

    info!(id = id, "successfully update live streamers");
    Ok(Json(streamer))
}

pub async fn delete_streamers_endpoint(
    State(managers): State<Arc<DownloadManager>>,
    State(pool): State<ConnectionPool>,
    Path(id): Path<i64>,
) -> Result<Json<LiveStreamer>, Response> {
    managers.del_room(id).await;

    let live_streamers = del_streamer(&pool, id).await.map_err(report_to_response)?;
    info!(workers=?live_streamers, "successfully inserted new live streamers");
    Ok(Json(live_streamers))
}

// #[axum::debug_handler(state = ServiceRegister)]
pub async fn pause_streamers_endpoint(
    State(managers): State<Arc<DownloadManager>>,
    Path(id): Path<i64>,
) -> Result<Json<()>, Response> {
    let worker = managers.get_room_by_id(id).await;
    if let Some(w) = worker {
        let worker_status = w.downloader_status.read().unwrap().clone();
        match worker_status {
            WorkerStatus::Working(_) => {
                w.change_status(Stage::Download, WorkerStatus::Pause).await;
                info!(url=?&w.live_streamer.url, "successfully pause live streamers");
                managers.make_waker(id).await;
            }
            WorkerStatus::Pause => {
                w.change_status(Stage::Download, WorkerStatus::Idle).await;
                managers.wake_waker(id).await;
                info!(url=?&w.live_streamer.url, "successfully start live streamers");
            }
            WorkerStatus::Pending => {
                w.change_status(Stage::Download, WorkerStatus::Pause).await;
                managers.make_waker(id).await;
                info!(url=?&w.live_streamer.url, "successfully pause live streamers");
            }
            WorkerStatus::Idle => {
                w.change_status(Stage::Download, WorkerStatus::Pause).await;
                managers.make_waker(id).await;
                info!(url=?&w.live_streamer.url, "successfully pause live streamers");
            }
        };
    }

    Ok(Json(()))
}

pub async fn get_configuration(
    State(config): State<Arc<RwLock<Config>>>,
) -> Result<Json<Config>, Response> {
    Ok(Json(config.read().unwrap().clone()))
}

// #[axum_macros::debug_handler(state = ServiceRegister)]
pub async fn put_configuration(
    State(config): State<Arc<RwLock<Config>>>,
    State(pool): State<ConnectionPool>,
    State(log_handle): State<LogHandle>,
    Json(json_data): Json<Config>,
) -> Result<Json<Config>, Response> {
    let mut json_data = json_data;
    json_data.normalize_segment_limits();
    json_data
        .validate_segment_limits()
        .map_err(report_to_response)?;
    // 将 JSON 序列化为 TEXT 存库
    let value_txt = serde_json::to_string(&json_data)
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;

    let mut tx = pool
        .begin()
        .await
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;

    // 最多取 2 条判断是否多行
    let ids: Vec<i64> =
        sqlx::query_scalar::<_, i64>("SELECT id FROM configuration WHERE key = ?1 LIMIT 2")
            .bind("config")
            .fetch_all(&mut *tx)
            .await
            .change_context(AppError::Unknown)
            .map_err(report_to_response)?;

    let saved: Configuration = if ids.is_empty() {
        // 插入
        sqlx::query("INSERT INTO configuration (key, value) VALUES (?1, ?2)")
            .bind("config")
            .bind(&value_txt)
            .execute(&mut *tx)
            .await
            .change_context(AppError::Unknown)
            .map_err(report_to_response)?;

        // 取 last_insert_rowid 并读回整行
        let id: i64 = sqlx::query_scalar::<_, i64>("SELECT last_insert_rowid()")
            .fetch_one(&mut *tx)
            .await
            .change_context(AppError::Unknown)
            .map_err(report_to_response)?;

        sqlx::query_as::<_, Configuration>("SELECT id, key, value FROM configuration WHERE id = ?1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .change_context(AppError::Unknown)
            .map_err(report_to_response)?
    } else if ids.len() == 1 {
        // 更新
        let id = ids[0];
        sqlx::query("UPDATE configuration SET value = ?1 WHERE id = ?2")
            .bind(&value_txt)
            .bind(id)
            .execute(&mut *tx)
            .await
            .change_context(AppError::Unknown)
            .map_err(report_to_response)?;

        sqlx::query_as::<_, Configuration>("SELECT id, key, value FROM configuration WHERE id = ?1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .change_context(AppError::Unknown)
            .map_err(report_to_response)?
    } else {
        // 多行报错
        return Err(report_to_response(Report::new(AppError::Custom(
            format!("有多个空间配置同时存在 (key='config'): {} 行", ids.len()).to_string(),
        ))));
    };

    tx.commit()
        .await
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;
    // 提交后从 DB 重新加载配置
    let mut saved_config: Config = serde_json::from_str(&saved.value)
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;
    saved_config.normalize_segment_limits();
    saved_config
        .validate_segment_limits()
        .map_err(report_to_response)?;
    *config.write().unwrap() = saved_config;
    let guard = config.read().unwrap();
    if let Some(loggers_level) = &guard.loggers_level {
        let new_filter = EnvFilter::try_new(loggers_level)
            .change_context(AppError::Custom(String::from("Invalid log level format")))
            .map_err(report_to_response)?;

        log_handle
            .modify(|filter| *filter = new_filter)
            .change_context(AppError::Unknown)
            .map_err(report_to_response)?;
    }

    Ok(Json(guard.clone()))
}

pub async fn get_streamer_info(
    // Extension(streamers_service): Extension<DynUploadStreamersRepository>,
    State(pool): State<ConnectionPool>,
) -> Result<Json<Vec<StreamerInfo>>, Response> {
    let streamer_infos = StreamerInfo::select()
        .fetch_all(&pool)
        .await
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;

    Ok(Json(streamer_infos))
}

pub async fn get_streamer_info_files(
    // Extension(streamers_service): Extension<DynUploadStreamersRepository>,
    State(pool): State<ConnectionPool>,
    Path(id): Path<i64>,
) -> Result<Json<Vec<FileItem>>, Response> {
    let file_items = FileItem::select()
        .where_("streamer_info_id = ?")
        .bind(id)
        .fetch_all(&pool)
        .await
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;

    Ok(Json(file_items))
}

pub async fn get_upload_streamers_endpoint(
    // Extension(streamers_service): Extension<DynUploadStreamersRepository>,
    State(pool): State<ConnectionPool>,
) -> Result<Json<Vec<UploadStreamer>>, Response> {
    let uploader_streamers = UploadStreamer::select()
        .fetch_all(&pool)
        .await
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;
    Ok(Json(uploader_streamers))
}

pub async fn add_upload_streamer_endpoint(
    // Extension(streamers_service): Extension<DynUploadStreamersRepository>,
    State(pool): State<ConnectionPool>,
    Json(upload_streamer): Json<InsertUploadStreamer>,
) -> Result<Json<serde_json::Value>, Response> {
    if upload_streamer.id.is_none() {
        Ok(Json(
            serde_json::to_value(
                ormlite::Insert::insert(upload_streamer, &pool)
                    .await
                    .change_context(AppError::Unknown)
                    .map_err(report_to_response)?,
            )
            .change_context(AppError::Unknown)
            .map_err(report_to_response)?,
        ))
    } else {
        Ok(Json(
            serde_json::to_value(
                upload_streamer
                    .update_all_fields(&pool)
                    .await
                    .change_context(AppError::Unknown)
                    .map_err(report_to_response)?,
            )
            .change_context(AppError::Unknown)
            .map_err(report_to_response)?,
        ))
    }
}

pub async fn get_upload_streamer_endpoint(
    State(pool): State<ConnectionPool>,
    Path(id): Path<i64>,
) -> Result<Json<UploadStreamer>, Response> {
    let uploader_streamers = UploadStreamer::select()
        .where_("id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;
    Ok(Json(uploader_streamers))
}
pub async fn delete_template_endpoint(
    State(pool): State<ConnectionPool>,
    Path(id): Path<i64>,
) -> Result<Json<()>, Response> {
    let uploader_streamers = UploadStreamer::select()
        .where_("id = ?")
        .bind(id)
        .fetch_one(&pool)
        .await
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;
    Ok(Json(
        uploader_streamers
            .delete(&pool)
            .await
            .change_context(AppError::Unknown)
            .map_err(report_to_response)?,
    ))
}

pub async fn get_users_endpoint(
    State(pool): State<ConnectionPool>,
) -> Result<Json<Vec<serde_json::Value>>, Response> {
    let configurations = Configuration::select()
        .where_("key = 'bilibili-cookies'")
        .fetch_all(&pool)
        .await
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;
    let mut res = Vec::new();
    for cookies in configurations {
        res.push(json!({
            "id": cookies.id,
            "name": cookies.value,
            "value": cookies.value,
            "platform": cookies.key,
        }))
    }
    Ok(Json(res))
}

pub async fn add_user_endpoint(
    State(pool): State<ConnectionPool>,
    Json(user): Json<InsertConfiguration>,
) -> Result<Json<Configuration>, Response> {
    let res = user
        .insert(&pool)
        .await
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;
    Ok(Json(res))
}

pub async fn delete_user_endpoint(
    Path(id): Path<i64>,
    State(pool): State<ConnectionPool>,
) -> Result<Json<()>, Response> {
    let x = sqlx::query("DELETE FROM configuration WHERE id = ?")
        .bind(id)
        .execute(&pool)
        .await
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;
    info!("{:?}", x);
    Ok(Json(()))
}

pub async fn get_qrcode() -> Result<Json<serde_json::Value>, Response> {
    let qrcode = Credential::new(None)
        .get_qrcode()
        .await
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;
    Ok(Json(qrcode))
}

pub async fn login_by_qrcode(
    Json(value): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, Response> {
    let info = tokio::time::timeout(
        Duration::from_secs(300),
        Credential::new(None).login_by_qrcode(value),
        // std::future::pending::<AppResult<LoginInfo>>(),
    )
    .await
    .change_context(AppError::Custom("deadline has elapsed".to_string()))
    .map_err(report_to_response)?
    .change_context(AppError::Unknown)
    .map_err(report_to_response)?;

    // extract mid
    let mid = info.token_info.mid;
    let filename = format!("data/{}.json", mid);

    let mut file = fs::File::create(&filename)
        .await
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;
    file.write_all(&serde_json::to_vec_pretty(&info).unwrap())
        .await
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;

    Ok(Json(json!({ "filename": filename })))
}

pub async fn get_videos() -> Result<Json<Vec<serde_json::Value>>, Response> {
    let media_extensions = [".mp4", ".flv", ".3gp", ".webm", ".mkv", ".ts"];
    let blacklist = ["next-env.d.ts"];

    let mut file_list = Vec::new();
    let mut index = 1;

    // **use tokio::fs::read_dir**
    if let Ok(mut entries) = fs::read_dir(".").await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let file_name = entry.file_name().to_string_lossy().into_owned();

            if blacklist.contains(&file_name.as_str()) {
                continue;
            }

            if let Some(ext) = path.extension().and_then(|e| e.to_str())
                && media_extensions
                    .iter()
                    .any(|allowed| ext == allowed.trim_start_matches('.'))
                && let Ok(metadata) = entry.metadata().await
            {
                let mtime = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);

                file_list.push(serde_json::json!({
                    "key": index,
                    "name": file_name,
                    "updateTime": mtime,
                    "size": metadata.len(),
                }));
                index += 1;
            }
        }
    }
    Ok(Json(file_list))
}

// #[axum::debug_handler(state = ServiceRegister)]
pub async fn get_status(
    State(_service_register): State<ServiceRegister>,
    State(managers): State<Arc<DownloadManager>>,
    State(config): State<Arc<RwLock<Config>>>,
) -> Result<Json<serde_json::Value>, Response> {
    let workers = managers.get_rooms().await;

    let mut sw = Vec::new();
    for worker in &workers {
        sw.push(serde_json::json!({
            "downloader_status": format!("{:?}", worker.downloader_status.read()),
            "uploader_status": format!("{:?}", worker.uploader_status.read().unwrap()),
            "live_streamer": worker.live_streamer,
            "upload_streamer": worker.upload_streamer,
        }));
    }

    Ok(Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "rooms": sw,
        "download_semaphore": managers.download_semaphore,
        "update_semaphore": managers.u_kills.len(),
        "config": config,
    })))
}

/// 平台 cookie 健康状态：供前端横幅轮询。返回 `{ platforms: [{platform, unhealthy, ...}] }`。
pub async fn get_cookie_health() -> Json<serde_json::Value> {
    Json(crate::server::common::cookie_health::snapshot())
}

#[derive(Deserialize)]
pub struct PostUploads {
    files: Vec<PathBuf>,
    params: UploadStreamer,
}

// #[debug_handler]
pub async fn post_uploads(
    State(config): State<Arc<RwLock<Config>>>,
    Json(json_data): Json<PostUploads>,
) -> Result<Json<serde_json::Value>, Response> {
    let upload_config = json_data.params;
    let (line, limit, submit_api) = {
        let config = config.read().unwrap();
        let line = UploadLine::from_str(&config.lines, true).ok();
        let limit = config.threads;
        let submit_api = config.submit_api.clone();
        (line, limit, submit_api)
    };
    info!("通过页面开始上传");
    tokio::spawn(async move {
        let (bilibili, videos) = upload(
            upload_config
                .user_cookie
                .as_deref()
                .unwrap_or("cookies.json"),
            None,
            line,
            &json_data.files,
            limit as usize,
        )
        .await?;
        if !videos.is_empty() {
            let recorder = Recorder::new(
                upload_config.title.clone(),
                StreamerInfo::new(
                    &upload_config.template_name,
                    "stream_title",
                    "",
                    Utc::now(),
                    "",
                ),
            );
            let studio = build_studio(&upload_config, &bilibili, videos, &recorder).await?;
            let response_data =
                submit_to_bilibili(&bilibili, &studio, submit_api.as_deref()).await?;
            info!("通过页面上传成功 {:?}", response_data);
        }
        Ok::<_, Report<AppError>>(())
    });

    Ok(Json(serde_json::json!({})))
}

#[derive(serde::Deserialize)]
pub struct MissingQuery {
    pub status: Option<String>,
}

/// 缺失补传行 + 其所属会话的投稿结果（aid/bvid/status）。
/// missing 行自身的 aid 在方案B 下通常为空（入队时稿件还没建、投稿后也不回填），
/// 真正的番号在 upload_session 上，这里 JOIN 出来供前端「去向」列直接给出 B 站链接。
#[derive(serde::Serialize)]
pub struct MissingSegmentView {
    #[serde(flatten)]
    pub segment: UploadMissingSegment,
    pub session_aid: Option<i64>,
    pub session_bvid: Option<String>,
    pub session_status: Option<String>,
}

pub async fn get_missing_uploads(
    State(service_register): State<ServiceRegister>,
    Query(q): Query<MissingQuery>,
) -> Result<Json<Vec<MissingSegmentView>>, Response> {
    let where_clause = missing_status_where(q.status.as_deref());
    let sql = format!(
        "SELECT * FROM upload_missing_segment WHERE {where_clause} ORDER BY created_at DESC"
    );
    let rows = sqlx::query_as::<_, UploadMissingSegment>(&sql)
        .fetch_all(&service_register.pool)
        .await
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;

    // 取这批行涉及的会话，映射 id -> (aid, bvid, status)，再拼回每行（避免逐行查库）。
    let session_ids: Vec<i64> = rows.iter().filter_map(|r| r.upload_session_id).collect();
    let mut session_map: std::collections::HashMap<i64, (Option<i64>, Option<String>, String)> =
        std::collections::HashMap::new();
    if !session_ids.is_empty() {
        // session_ids 全部来自 DB 的 i64 内部主键，非外部输入，直接拼 IN 列表安全。
        let in_list = session_ids
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let sql =
            format!("SELECT id, aid, bvid, status FROM upload_session WHERE id IN ({in_list})");
        let sessions = sqlx::query_as::<_, (i64, Option<i64>, Option<String>, String)>(&sql)
            .fetch_all(&service_register.pool)
            .await
            .change_context(AppError::Unknown)
            .map_err(report_to_response)?;
        for (id, aid, bvid, status) in sessions {
            session_map.insert(id, (aid, bvid, status));
        }
    }

    let views = rows
        .into_iter()
        .map(|r| {
            let sess = r.upload_session_id.and_then(|id| session_map.get(&id));
            MissingSegmentView {
                session_aid: sess.and_then(|s| s.0),
                session_bvid: sess.and_then(|s| s.1.clone()),
                session_status: sess.map(|s| s.2.clone()),
                segment: r,
            }
        })
        .collect::<Vec<_>>();
    Ok(Json(views))
}

pub async fn recover_missing_upload(
    State(service_register): State<ServiceRegister>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Json<serde_json::Value>, Response> {
    let config = service_register.config.read().unwrap().clone();
    manual_recover_missing_segment(&config, &service_register.pool, id)
        .await
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn delete_missing_upload(
    State(service_register): State<ServiceRegister>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Json<serde_json::Value>, Response> {
    let row = match claim_missing_segment_for_delete(&service_register.pool, id, Utc::now())
        .await
        .map_err(report_to_response)?
    {
        MissingSegmentDeleteClaim::Claimed(row) => row,
        MissingSegmentDeleteClaim::NotFound => {
            return Err((StatusCode::NOT_FOUND, "missing upload not found").into_response());
        }
        MissingSegmentDeleteClaim::NotDeletable { status } => {
            return Err((
                StatusCode::CONFLICT,
                format!("missing upload status '{status}' cannot be deleted"),
            )
                .into_response());
        }
    };

    let file_path = PathBuf::from(&row.file_path);
    let danmaku_path = row.danmaku_file_path.as_deref().map(PathBuf::from);
    if let Err(cleanup_error) =
        remove_missing_segment_files(&file_path, danmaku_path.as_deref()).await
    {
        let cleanup_message = format!("{cleanup_error:?}");
        if let Err(mark_error) = sqlx::query(
            "UPDATE upload_missing_segment SET last_error = ?1, updated_at = ?2 \
             WHERE id = ?3 AND status = 'deleting'",
        )
        .bind(cleanup_message)
        .bind(Utc::now())
        .bind(id)
        .execute(&service_register.pool)
        .await
        .change_context(AppError::Unknown)
        {
            error!(
                id,
                error = ?mark_error,
                "failed to record missing upload delete cleanup error"
            );
        }
        return Err(report_to_response(cleanup_error));
    }

    let delete =
        sqlx::query("DELETE FROM upload_missing_segment WHERE id = ? AND status = 'deleting'")
            .bind(id)
            .execute(&service_register.pool)
            .await
            .change_context(AppError::Unknown)
            .map_err(report_to_response)?;
    if delete.rows_affected() == 0 {
        return Err((
            StatusCode::CONFLICT,
            "missing upload delete claim was not available",
        )
            .into_response());
    }

    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn retry_missing_upload(
    State(service_register): State<ServiceRegister>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Json<serde_json::Value>, Response> {
    let config = service_register.config.read().unwrap().clone();
    retry_missing_segment(&config, &service_register.pool, id)
        .await
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}
