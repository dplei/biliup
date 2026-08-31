use crate::observe::{self, standalone::UploadTask};
use crate::server::common::missing_segment::{
    MissingSegmentDeleteClaim, claim_missing_segment_for_delete, remove_missing_segment_files,
};
use crate::server::common::recording_lease;
use crate::server::common::recovery_eligibility::RecoveryEligibility;
use crate::server::common::recovery_scheduler::{recover_due_segments, spawn_claimed_recovery};
use crate::server::common::upload::{
    RecoveryClaim, StopAttemptOutcome, SubmissionTrigger, build_studio, claim_manual_recovery,
    claim_retry_recovery, rescan_local_valid_segments, spawn_session_submission,
    stop_missing_segment_attempt, submit_to_bilibili, upload_with_task,
};
use crate::server::common::upload_line_health;
use crate::server::common::upload_line_selection::{cooling_lines, plan_upload_line};
use crate::server::common::upload_session::{
    EmptySessionDiscardResult, RequestSessionSubmit, SessionCompleteness, discard_empty_session,
    get_streamer_info as load_streamer_info, match_streamer_by_filename, missing_status_where,
    request_session_submit, session_completeness,
};
use crate::server::common::util::Recorder;
use crate::server::config::Config;
use crate::server::core::download_manager::DownloadManager;
use crate::server::errors::{ApiError, AppError, report_to_response};
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
    del_streamer, find_streamer, get_all_streamer, get_streamer_by_url, get_upload_config,
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
use tracing::instrument::WithSubscriber;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

pub async fn get_streamers_endpoint(
    State(pool): State<ConnectionPool>,
    State(managers): State<Arc<DownloadManager>>,
) -> Result<Json<Vec<LiveStreamerResponse>>, Response> {
    let live_streamers = get_all_streamer(&pool).await.map_err(report_to_response)?;
    let mut leases = recording_lease::current_lease_projections(&pool)
        .await
        .map_err(report_to_response)?;
    let mut results = Vec::new();
    let server_now = Utc::now();
    let workers = managers.get_rooms().await;
    for x in live_streamers {
        let live_streamer_id = x.id;
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
            recording_lease: leases.remove(&live_streamer_id),
            server_now,
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
    let worker = service_register.worker(live_streamers.clone(), upload_config);
    recording_lease::apply_initial_state(&pool, &worker)
        .await
        .map_err(report_to_response)?;
    let Some(_) = managers.add_room(worker).await else {
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
    Json(mut payload): Json<LiveStreamer>,
) -> Result<Json<LiveStreamer>, Response> {
    // 载荷里整项缺失就沿用库里的值。这条路由收的是 LiveStreamer 本身，而主播编辑页
    // 拼载荷用的是显式白名单（见 OverrideModal 的 baseValues），背景字段那一票落地前
    // 不会出现在里面；缺项被 serde 读成 None，直接 update_all_fields 就把配好的背景清空了。
    //
    // 查库失败必须往上抛，不能降级成「按载荷原样更新」——那会把一次数据库抖动
    // 变成一次配置丢失。查不到行（None）则维持载荷原样，交给 update_all_fields 空转。
    if payload.cover_background.is_none() {
        payload.cover_background = find_streamer(&pool, payload.id)
            .await
            .map_err(report_to_response)?
            .and_then(|row| row.cover_background);
    }

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

    let worker = service_register.worker(streamer.clone(), upload_config);
    recording_lease::apply_initial_state(&pool, &worker)
        .await
        .map_err(report_to_response)?;
    managers
        .add_room(worker)
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
    json_data.validate_segment_limits().map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(error.to_string())),
        )
            .into_response()
    })?;
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

/// 全局上传节流状态：供运维查看 601 冷却、等待任务和 pre_upload 计数。
pub async fn get_upload_rate_health() -> Json<serde_json::Value> {
    Json(
        serde_json::to_value(crate::server::common::upload_rate_gate::snapshot().await)
            .unwrap_or_else(|_| serde_json::json!({ "state": "unknown" })),
    )
}

pub async fn get_upload_enrollment_health() -> Json<serde_json::Value> {
    Json(crate::server::common::segment_enrollment::outbox_health(
        std::path::Path::new(crate::server::common::segment_enrollment::DEFAULT_OUTBOX_DIRECTORY),
    ))
}

/// 缺失分段生命周期状态计数 + 已过 5 分钟仍 uploading 未被后台收敛的行，
/// 供运维在自愈周期（60s）跑完前就能看到卡住的补传（对应 08 号任务第 3/4 节）。
pub async fn get_upload_missing_segment_health(
    State(service_register): State<ServiceRegister>,
) -> Result<Json<crate::server::common::missing_segment::MissingSegmentHealth>, Response> {
    crate::server::common::missing_segment::missing_segment_health(
        &service_register.pool,
        Utc::now(),
    )
    .await
    .map(Json)
    .map_err(report_to_response)
}

#[derive(serde::Serialize, sqlx::FromRow)]
pub struct RecoveryBatchView {
    pub id: i64,
    pub recovery_batch_id: String,
    pub live_streamer_id: i64,
    pub streamer_info_id: i64,
    pub state: String,
    pub files_json: String,
    pub manifest_path: String,
    pub attempts: i64,
    pub next_retry_at: chrono::DateTime<chrono::Utc>,
    pub last_error: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

pub async fn get_recovery_batches(
    State(pool): State<ConnectionPool>,
) -> Result<Json<Vec<RecoveryBatchView>>, Response> {
    let rows = sqlx::query_as::<_, RecoveryBatchView>(
        "SELECT * FROM recoverable_short_batch ORDER BY created_at DESC",
    )
    .fetch_all(&pool)
    .await
    .change_context(AppError::Unknown)
    .map_err(report_to_response)?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
pub struct PostUploads {
    files: Vec<PathBuf>,
    params: UploadStreamer,
}

// #[debug_handler]
pub async fn post_uploads(
    State(config): State<Arc<RwLock<Config>>>,
    State(pool): State<ConnectionPool>,
    Json(json_data): Json<PostUploads>,
) -> Result<Json<serde_json::Value>, Response> {
    // A page request may mix files from different recordings. The first-file lookup below
    // is only for template rendering; it cannot establish lineage for the whole upload.
    let task = UploadTask::default();
    let task_id = task.submission.task_id.clone();
    let upload_config = json_data.params;
    let files = json_data.files;
    let (line, limit, submit_api) = {
        let config = config.read().unwrap();
        let line = UploadLine::from_str(&config.lines, true).ok();
        let limit = config.threads;
        let submit_api = config.submit_api.clone();
        (line, limit, submit_api)
    };

    // 按第一段文件（P1）反查它属于哪个主播，用真实 StreamerInfo 填标题/简介模板。
    // 未命中（手动拷入、不在 filelist 的文件）沿用占位兜底，不阻断上传。
    let placeholder = || StreamerInfo::placeholder(&upload_config.template_name, Utc::now());
    let (streamer_info, matched, streamer_name) = match files.first() {
        Some(first) => match match_streamer_by_filename(&pool, first).await {
            Ok(Some(sid)) => match load_streamer_info(&pool, sid).await {
                Ok(info) => {
                    let name = info.name.clone();
                    (info, true, Some(name))
                }
                Err(e) => {
                    error!(?e, "历史文件上传：streamer_info 载入失败，回退占位");
                    (placeholder(), false, None)
                }
            },
            Ok(None) => (placeholder(), false, None),
            Err(e) => {
                error!(?e, "历史文件上传：反查主播失败，回退占位");
                (placeholder(), false, None)
            }
        },
        None => (placeholder(), false, None),
    };

    // 主播级背景覆盖模板级。只在真的反查到主播时才查它的背景——未命中时 streamer_info
    // 是占位对象，它的 url 不是真实直播间地址，拿去查库等于赌「不会撞上任何一行」。
    // 查库失败退到模板级，不阻断上传。
    let streamer_background = if matched {
        match get_streamer_by_url(&pool, &streamer_info.url).await {
            Ok(streamer) => streamer.and_then(|s| s.cover_background),
            Err(e) => {
                error!(?e, "历史文件上传：取主播背景失败，回退到模板级背景");
                None
            }
        }
    } else {
        None
    };

    info!(matched, ?streamer_name, "通过页面开始上传");
    let runtime_config = config.read().unwrap().clone();
    tokio::spawn(
        async move {
            let (bilibili, videos) = upload_with_task(
                upload_config
                    .user_cookie
                    .as_deref()
                    .unwrap_or("cookies.json"),
                None,
                line,
                &files,
                limit as usize,
                &runtime_config,
                &pool,
                Some(&task),
            )
            .await?;
            if !videos.is_empty() {
                let recorder = Recorder::new(upload_config.title.clone(), streamer_info);
                let studio = task.check(
                    build_studio(
                        &upload_config,
                        streamer_background.as_deref(),
                        &bilibili,
                        videos,
                        &recorder,
                    )
                    .await,
                    "studio_build_failed",
                )?;
                let response_data = task
                    .submit(submit_to_bilibili(
                        &bilibili,
                        &studio,
                        submit_api.as_deref(),
                    ))
                    .await?;
                info!("通过页面上传成功 {:?}", response_data);
            } else {
                observe::submission_decided(&task.submission, "skipped", "no_videos", 0);
            }
            Ok::<_, Report<AppError>>(())
        }
        .with_current_subscriber(),
    );

    Ok(Json(serde_json::json!({
        // This is an observation correlation key, not a durable job or a success receipt.
        "task_id": task_id,
        "matched": matched,
        "streamer_name": streamer_name,
    })))
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
    pub session_submit_state: Option<String>,
    pub session_completeness: Option<SessionCompleteness>,
    pub next_line: String,
    pub line_skip_reason: Option<String>,
    /// The full candidate sequence behind `next_line`, so the page can show what a fallback
    /// would try next without guessing.
    pub line_candidates: Vec<String>,
}

pub async fn get_upload_line_health(
    State(service_register): State<ServiceRegister>,
) -> Result<Json<Vec<upload_line_health::UploadLineHealth>>, Response> {
    upload_line_health::all_health(&service_register.pool)
        .await
        .map(Json)
        .map_err(report_to_response)
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
    let mut session_map: std::collections::HashMap<
        i64,
        (Option<i64>, Option<String>, String, Option<String>),
    > = std::collections::HashMap::new();
    let mut completeness_map = std::collections::HashMap::new();
    if !session_ids.is_empty() {
        // session_ids 全部来自 DB 的 i64 内部主键，非外部输入，直接拼 IN 列表安全。
        let in_list = session_ids
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT id, aid, bvid, status, submit_state FROM upload_session WHERE id IN ({in_list})"
        );
        let sessions =
            sqlx::query_as::<_, (i64, Option<i64>, Option<String>, String, Option<String>)>(&sql)
                .fetch_all(&service_register.pool)
                .await
                .change_context(AppError::Unknown)
                .map_err(report_to_response)?;
        for (id, aid, bvid, status, submit_state) in sessions {
            session_map.insert(id, (aid, bvid, status, submit_state));
            let completeness = session_completeness(&service_register.pool, id)
                .await
                .map_err(report_to_response)?;
            completeness_map.insert(id, completeness);
        }
    }

    // The page's "next line" is the *same* decision the uploader will make, evaluated against the
    // same cooldown snapshot. It used to be a second copy of the hardcoded `bda2 -> tx -> auto`
    // constant, so page and reality were consistently wrong together.
    let now = Utc::now();
    let cooling = cooling_lines(&service_register.pool, now)
        .await
        .map_err(report_to_response)?;
    let configured_line = service_register.config.read().unwrap().lines.clone();

    let views = rows
        .into_iter()
        .map(|r| {
            let sess = r.upload_session_id.and_then(|id| session_map.get(&id));
            let plan = plan_upload_line(&configured_line, None, &cooling, now);
            let next_line = plan.chosen.clone();
            let line_skip_reason = plan.skip_reason();
            MissingSegmentView {
                session_aid: sess.and_then(|s| s.0),
                session_bvid: sess.and_then(|s| s.1.clone()),
                session_status: sess.map(|s| s.2.clone()),
                session_submit_state: sess.and_then(|s| s.3.clone()),
                session_completeness: r
                    .upload_session_id
                    .and_then(|id| completeness_map.get(&id).cloned()),
                next_line,
                line_skip_reason,
                line_candidates: plan.candidates,
                segment: r,
            }
        })
        .collect::<Vec<_>>();
    Ok(Json(views))
}

/// Stable operator-facing state for a session with durable submit intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingSubmitAction {
    WaitingSegments,
    ReadyToSubmit,
    Submitting,
    RetryScheduled,
    ManualInspection,
}

#[derive(Debug, sqlx::FromRow)]
struct PendingSubmitRow {
    id: i64,
    live_streamer_id: i64,
    streamer_info_id: i64,
    streamer_name: String,
    stream_title: String,
    stream_started_at: chrono::DateTime<Utc>,
    submit_requested_at: chrono::DateTime<Utc>,
    submit_state: Option<String>,
    submit_attempts: i64,
    submit_retry_attempts: i64,
    last_submit_at: Option<chrono::DateTime<Utc>>,
    last_submit_error: Option<String>,
    next_submit_at: Option<chrono::DateTime<Utc>>,
    submit_claim_token: Option<String>,
    submit_claimed_at: Option<chrono::DateTime<Utc>>,
    aid: Option<i64>,
    bvid: Option<String>,
    status: String,
}

#[derive(Debug, serde::Serialize)]
pub struct PendingSubmitSessionView {
    pub id: i64,
    pub live_streamer_id: i64,
    pub streamer_info_id: i64,
    pub streamer_name: String,
    pub stream_title: String,
    pub stream_started_at: chrono::DateTime<Utc>,
    pub submit_requested_at: chrono::DateTime<Utc>,
    pub submit_state: Option<String>,
    pub submit_attempts: i64,
    pub submit_retry_attempts: i64,
    pub last_submit_at: Option<chrono::DateTime<Utc>>,
    pub last_submit_error: Option<String>,
    pub next_submit_at: Option<chrono::DateTime<Utc>>,
    pub submit_claimed: bool,
    pub action: PendingSubmitAction,
    pub action_message: String,
    pub completeness: SessionCompleteness,
    pub aid: Option<i64>,
    pub bvid: Option<String>,
    pub status: String,
}

const ACTIVE_SUBMIT_DISPLAY_WINDOW: chrono::Duration = chrono::Duration::minutes(15);

fn pending_submit_action(
    row: &PendingSubmitRow,
    completeness: &SessionCompleteness,
    now: chrono::DateTime<Utc>,
) -> (PendingSubmitAction, String) {
    if row.submit_claim_token.is_some() {
        if row.submit_state.as_deref() == Some("submitting")
            && row
                .submit_claimed_at
                .is_some_and(|claimed| claimed >= now - ACTIVE_SUBMIT_DISPLAY_WINDOW)
        {
            return (
                PendingSubmitAction::Submitting,
                "投稿协调器已取得唯一 claim，正在提交；请勿重复操作。".to_string(),
            );
        }
        let message = if row.submit_state.as_deref() == Some("ok_no_aid") {
            "远端可能已接受投稿但没有返回稳定 aid；请先在创作中心核对，系统不会自动重投。"
        } else {
            "投稿 claim 长时间未收敛或结果不确定；请人工核对远端稿件，系统不会自动偷取 claim。"
        };
        return (PendingSubmitAction::ManualInspection, message.to_string());
    }
    if matches!(
        row.submit_state.as_deref(),
        Some("ok_no_aid" | "submitting")
    ) {
        return (
            PendingSubmitAction::ManualInspection,
            "会话处于不确定投稿状态但缺少可验证的 claim；请人工检查数据库与远端稿件。".to_string(),
        );
    }
    if !completeness.is_complete() {
        return (
            PendingSubmitAction::WaitingSegments,
            format!(
                "仍有 {} 个未完成或异常分段，需先完成分段恢复。",
                completeness.incomplete_count()
            ),
        );
    }
    if let Some(next_at) = row.next_submit_at
        && next_at > now
    {
        return (
            PendingSubmitAction::RetryScheduled,
            format!("上次投稿明确失败，系统将在 {next_at} 后自动重试。"),
        );
    }
    (
        PendingSubmitAction::ReadyToSubmit,
        "分段账本已完整，等待投稿协调器领取；可手动再次唤醒。".to_string(),
    )
}

/// Sessions awaiting the one-shot submission, independent of the missing-segment filter.
pub async fn get_pending_submit_sessions(
    State(service_register): State<ServiceRegister>,
) -> Result<Json<Vec<PendingSubmitSessionView>>, Response> {
    let rows = sqlx::query_as::<_, PendingSubmitRow>(
        "SELECT s.id, s.live_streamer_id, s.streamer_info_id, \
                COALESCE(NULLIF(l.remark, ''), i.name) AS streamer_name, \
                i.title AS stream_title, i.date AS stream_started_at, \
                s.submit_requested_at, s.submit_state, s.submit_attempts, \
                s.submit_retry_attempts, s.last_submit_at, \
                s.last_submit_error, s.next_submit_at, s.submit_claim_token, \
                s.submit_claimed_at, s.aid, s.bvid, s.status \
         FROM upload_session s \
         JOIN streamerinfo i ON i.id = s.streamer_info_id \
         JOIN livestreamers l ON l.id = s.live_streamer_id \
         WHERE s.status != 'finalized' AND s.submit_requested_at IS NOT NULL \
         ORDER BY s.submit_requested_at ASC, s.id ASC",
    )
    .fetch_all(&service_register.pool)
    .await
    .change_context(AppError::Unknown)
    .map_err(report_to_response)?;
    let now = Utc::now();
    let mut views = Vec::with_capacity(rows.len());
    for row in rows {
        let completeness = session_completeness(&service_register.pool, row.id)
            .await
            .map_err(report_to_response)?;
        let (action, action_message) = pending_submit_action(&row, &completeness, now);
        views.push(PendingSubmitSessionView {
            id: row.id,
            live_streamer_id: row.live_streamer_id,
            streamer_info_id: row.streamer_info_id,
            streamer_name: row.streamer_name,
            stream_title: row.stream_title,
            stream_started_at: row.stream_started_at,
            submit_requested_at: row.submit_requested_at,
            submit_state: row.submit_state,
            submit_attempts: row.submit_attempts,
            submit_retry_attempts: row.submit_retry_attempts,
            last_submit_at: row.last_submit_at,
            last_submit_error: row.last_submit_error,
            next_submit_at: row.next_submit_at,
            submit_claimed: row.submit_claim_token.is_some(),
            action,
            action_message,
            completeness,
            aid: row.aid,
            bvid: row.bvid,
            status: row.status,
        });
    }
    Ok(Json(views))
}

/// Optional per-task line override from the recovery page. `None` (or `"auto"`) means "follow
/// configuration"; anything else is honoured unless that line is cooling.
#[derive(serde::Deserialize, Default)]
pub struct RecoveryRequest {
    #[serde(default)]
    pub line: Option<String>,
}

/// What the page gets back the moment a recovery is accepted.
#[derive(serde::Serialize)]
pub struct RecoveryAccepted {
    pub ok: bool,
    pub missing_id: i64,
    pub eligibility: RecoveryEligibility,
    pub attempt_token: Option<String>,
    pub line: Option<String>,
    pub line_skip_reason: Option<String>,
    pub status: &'static str,
}

/// Turn a claim into a response and hand the upload to a background task.
///
/// The handler used to `await` the whole upload. For a 3.32 GB segment that is guaranteed to
/// exceed any reverse proxy's read timeout, and the resulting dropped future took the watchdog
/// down with it, leaving the row stuck at `uploading` forever.
fn accept_recovery(
    service_register: &ServiceRegister,
    config: Config,
    missing_id: i64,
    claim: RecoveryClaim,
) -> Json<RecoveryAccepted> {
    match claim {
        RecoveryClaim::Claimed(claim) => {
            let accepted = RecoveryAccepted {
                ok: true,
                missing_id: claim.missing_id(),
                eligibility: RecoveryEligibility::Eligible,
                attempt_token: claim.attempt_token().map(str::to_string),
                line: claim.line_key().map(str::to_string),
                line_skip_reason: claim.line_skip_reason(),
                status: "uploading",
            };
            spawn_claimed_recovery(config, service_register.pool.clone(), claim);
            Json(accepted)
        }
        RecoveryClaim::Rejected(eligibility) => Json(RecoveryAccepted {
            ok: matches!(eligibility, RecoveryEligibility::LegacyFinalizedEdit),
            missing_id,
            eligibility,
            attempt_token: None,
            line: None,
            line_skip_reason: None,
            status: "rejected",
        }),
    }
}

pub async fn recover_missing_upload(
    State(service_register): State<ServiceRegister>,
    axum::extract::Path(id): axum::extract::Path<i64>,
    body: Option<Json<RecoveryRequest>>,
) -> Result<Json<RecoveryAccepted>, Response> {
    let config = service_register.config.read().unwrap().clone();
    let line = body.and_then(|Json(request)| request.line);
    let claim = claim_manual_recovery(&config, &service_register.pool, id, line.as_deref())
        .await
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;
    Ok(accept_recovery(&service_register, config, id, claim))
}

#[derive(serde::Deserialize)]
pub struct LocalSegmentRescanRequest {
    pub streamer_info_id: i64,
}

pub async fn rescan_missing_uploads(
    State(service_register): State<ServiceRegister>,
    Json(request): Json<LocalSegmentRescanRequest>,
) -> Result<Json<crate::server::common::upload::LocalSegmentRescanResult>, Response> {
    let config = service_register.config.read().unwrap().clone();
    let working_directory = std::env::current_dir()
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;
    rescan_local_valid_segments(
        &config,
        &service_register.pool,
        request.streamer_info_id,
        &working_directory,
    )
    .await
    .map(Json)
    .map_err(report_to_response)
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
    body: Option<Json<RecoveryRequest>>,
) -> Result<Json<RecoveryAccepted>, Response> {
    let config = service_register.config.read().unwrap().clone();
    let line = body.and_then(|Json(request)| request.line);
    let claim = claim_retry_recovery(&config, &service_register.pool, id, line.as_deref())
        .await
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;
    Ok(accept_recovery(&service_register, config, id, claim))
}

/// Release a wedged attempt without starting another one.
pub async fn stop_missing_upload(
    State(service_register): State<ServiceRegister>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Json<StopAttemptOutcome>, Response> {
    let outcome = stop_missing_segment_attempt(
        &service_register.pool,
        id,
        "stopped from the missing-uploads page",
    )
    .await
    .map_err(report_to_response)?;
    if matches!(outcome, StopAttemptOutcome::CancelTimedOut) {
        return Err((
            StatusCode::CONFLICT,
            "上一次上传尚未退出，请稍后重试；强行释放会导致同一分段被上传两次",
        )
            .into_response());
    }
    Ok(Json(outcome))
}

#[derive(Debug, serde::Serialize)]
pub struct SessionRecoveryBlockingSummary {
    /// Stable machine-readable reason for the current wait.
    pub code: &'static str,
    /// Operator-facing explanation. It intentionally does not expose source file paths.
    pub message: String,
    /// Lifecycle rows involved in this particular wait, when applicable.
    pub segment_ids: Vec<i64>,
}

/// Immediate state returned after a whole-session recovery request is accepted.
///
/// Upload and submission themselves always remain detached. The page can render this snapshot and
/// then poll the session/missing-upload views instead of holding the request open until Bilibili
/// responds.
#[derive(Debug, serde::Serialize)]
pub struct SessionRecoveryAccepted {
    pub upload_session_id: i64,
    pub segments_started: Vec<i64>,
    pub segments_busy: bool,
    pub submission_queued: bool,
    pub blocking_summary: Option<SessionRecoveryBlockingSummary>,
    pub session_status: String,
    pub submit_state: Option<String>,
    pub submit_requested_at: chrono::DateTime<Utc>,
    pub next_submit_at: Option<chrono::DateTime<Utc>>,
    pub submit_claimed: bool,
    pub last_submit_error: Option<String>,
    pub completeness: SessionCompleteness,
}

#[derive(Debug, serde::Serialize)]
pub struct EmptySessionDiscarded {
    pub upload_session_id: i64,
    pub previous_status: String,
    pub status: &'static str,
    pub submit_state: Option<String>,
    pub discarded: bool,
    pub already_finalized: bool,
}

/// Logically finalize a closed session which has no lifecycle rows or remote identity.
///
/// This deliberately keeps the database row: its finalized identity is the boundary that stops a
/// later local rescan from recreating the historical empty shell.
pub async fn discard_empty_upload_session(
    State(service_register): State<ServiceRegister>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Json<EmptySessionDiscarded>, Response> {
    match discard_empty_session(&service_register.pool, id)
        .await
        .map_err(report_to_response)?
    {
        EmptySessionDiscardResult::Discarded { previous_status } => {
            tracing::info!(
                session = id,
                source = "manual",
                reason = "zero_lifecycle_baseline",
                "empty upload session logically finalized"
            );
            Ok(Json(EmptySessionDiscarded {
                upload_session_id: id,
                previous_status,
                status: "finalized",
                submit_state: Some("discarded_empty".to_string()),
                discarded: true,
                already_finalized: false,
            }))
        }
        EmptySessionDiscardResult::AlreadyFinalized { submit_state } => {
            let discarded = submit_state.as_deref() == Some("discarded_empty");
            Ok(Json(EmptySessionDiscarded {
                upload_session_id: id,
                previous_status: "finalized".to_string(),
                status: "finalized",
                submit_state,
                discarded,
                already_finalized: true,
            }))
        }
        EmptySessionDiscardResult::NotFound => {
            Err((StatusCode::NOT_FOUND, "upload session not found").into_response())
        }
        EmptySessionDiscardResult::Rejected(rejection) => {
            Err((StatusCode::CONFLICT, rejection.message()).into_response())
        }
    }
}

#[derive(Debug, sqlx::FromRow)]
struct SessionRecoverySnapshot {
    status: String,
    submit_state: Option<String>,
    submit_requested_at: Option<chrono::DateTime<Utc>>,
    next_submit_at: Option<chrono::DateTime<Utc>>,
    submit_claim_token: Option<String>,
    last_submit_error: Option<String>,
}

async fn session_recovery_snapshot(
    pool: &ConnectionPool,
    session_id: i64,
) -> Result<Option<SessionRecoverySnapshot>, Report<AppError>> {
    sqlx::query_as(
        "SELECT status, submit_state, submit_requested_at, next_submit_at, submit_claim_token, \
                last_submit_error FROM upload_session WHERE id = ?1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await
    .change_context(AppError::Unknown)
}

async fn uploading_segment_ids(
    pool: &ConnectionPool,
    session_id: i64,
) -> Result<Vec<i64>, Report<AppError>> {
    sqlx::query_scalar(
        "SELECT id FROM upload_missing_segment \
         WHERE upload_session_id = ?1 AND status = 'uploading' ORDER BY segment_order, id",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await
    .change_context(AppError::Unknown)
}

fn recovery_blocking_summary(
    snapshot: &SessionRecoverySnapshot,
    completeness: &SessionCompleteness,
    started: &[i64],
    busy_ids: Vec<i64>,
    recovery_group_busy: bool,
) -> Option<SessionRecoveryBlockingSummary> {
    if snapshot.submit_claim_token.is_some() {
        let message = if snapshot.submit_state.as_deref() == Some("ok_no_aid") {
            "远端可能已接受投稿，但没有可确认的 aid；已保留投稿 claim，请先人工核对稿件。"
                .to_string()
        } else {
            "会话已有投稿 claim，投稿正在进行或远端结果尚未确认；不会自动发起第二稿。".to_string()
        };
        return Some(SessionRecoveryBlockingSummary {
            code: "submission_claimed",
            message,
            segment_ids: Vec::new(),
        });
    }
    if !started.is_empty() {
        return Some(SessionRecoveryBlockingSummary {
            code: "segments_recovering",
            message: "已在后台开始补传分段；最后一个分段成功后会自动重新检查投稿。".to_string(),
            segment_ids: started.to_vec(),
        });
    }
    if recovery_group_busy || !busy_ids.is_empty() {
        return Some(SessionRecoveryBlockingSummary {
            code: "segments_busy",
            message: "该会话已有分段恢复任务在运行；本次请求没有抢占现有 attempt。".to_string(),
            segment_ids: busy_ids,
        });
    }
    if let Some(next_at) = snapshot.next_submit_at
        && next_at > Utc::now()
    {
        return Some(SessionRecoveryBlockingSummary {
            code: "submit_retry_scheduled",
            message: format!("投稿已进入退避，将在 {next_at} 后由后台扫描自动重试。"),
            segment_ids: Vec::new(),
        });
    }
    if completeness.is_complete() {
        return None;
    }

    let (code, action) = if completeness.source_missing > 0 {
        (
            "source_missing",
            "源文件不存在，无法自动补传；请恢复原文件后重试，或人工处理该分段。",
        )
    } else if completeness.deleting > 0 {
        (
            "deleting",
            "分段正在删除，当前不能补传或投稿；请等待删除完成后刷新。",
        )
    } else if completeness.unknown > 0 {
        (
            "unknown_segment_state",
            "存在未知生命周期状态，无法自动完成；请检查分段记录。",
        )
    } else if completeness.pending + completeness.failed > 0 {
        (
            "segments_not_due",
            "仍有待补传分段，但当前没有可领取的任务；请等待重试时间或检查最近错误。",
        )
    } else {
        (
            "incomplete_ledger",
            "会话账本尚不完整，提交协调器会保持阻塞；请检查分段状态。",
        )
    };
    Some(SessionRecoveryBlockingSummary {
        code,
        message: action.to_string(),
        segment_ids: completeness
            .earliest_blocking_segment_id
            .into_iter()
            .collect(),
    })
}

/// Recover everything still outstanding in one session and make the operator's submit intent
/// durable.
///
/// Deliberately distinct from `rescan`: rescan is for "the file is on disk but has no row", this
/// is for "the row exists but nobody is running it". If no segment work is claimable, it wakes the
/// shared session submission coordinator; it never performs a remote upload or submit inline.
pub async fn recover_session_uploads(
    State(service_register): State<ServiceRegister>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<(StatusCode, Json<SessionRecoveryAccepted>), Response> {
    let now = Utc::now();
    let requested_at = match request_session_submit(&service_register.pool, id, now)
        .await
        .map_err(report_to_response)?
    {
        RequestSessionSubmit::NotFound => {
            return Err((StatusCode::NOT_FOUND, "upload session not found").into_response());
        }
        RequestSessionSubmit::Finalized => {
            return Err((
                StatusCode::CONFLICT,
                "该会话已投稿完成，不会为它创建新的补传任务",
            )
                .into_response());
        }
        RequestSessionSubmit::Requested { requested_at, .. } => requested_at,
    };

    // An individual recovery click and an older recovery run do not necessarily own the
    // scheduler's in-process group key. The durable attempt state is therefore the authoritative
    // busy signal and must be inspected independently of `busy_sessions` below.
    let uploading_before = uploading_segment_ids(&service_register.pool, id)
        .await
        .map_err(report_to_response)?;
    let config = service_register.config.read().unwrap().clone();
    let result = recover_due_segments(&config, &service_register.pool, Some(id), now)
        .await
        .map_err(report_to_response)?;
    let recovery_group_busy = !result.busy_sessions.is_empty();
    let uploading_after = uploading_segment_ids(&service_register.pool, id)
        .await
        .map_err(report_to_response)?;
    let started_set = result
        .started
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    let mut busy_ids = uploading_before;
    busy_ids.extend(
        uploading_after
            .into_iter()
            .filter(|segment_id| !started_set.contains(segment_id)),
    );
    busy_ids.sort_unstable();
    busy_ids.dedup();

    let completeness = session_completeness(&service_register.pool, id)
        .await
        .map_err(report_to_response)?;
    let snapshot = session_recovery_snapshot(&service_register.pool, id)
        .await
        .map_err(report_to_response)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "upload session not found").into_response())?;
    let segments_busy = recovery_group_busy || !busy_ids.is_empty();
    let submission_due = snapshot.next_submit_at.is_none_or(|next_at| next_at <= now);
    let submission_queued = result.started.is_empty()
        && !segments_busy
        && snapshot.submit_claim_token.is_none()
        && submission_due;
    let blocking_summary = recovery_blocking_summary(
        &snapshot,
        &completeness,
        &result.started,
        busy_ids,
        recovery_group_busy,
    );

    if submission_queued {
        spawn_session_submission(
            config,
            service_register.pool.clone(),
            id,
            SubmissionTrigger::ManualRecovery,
        );
    }

    Ok((
        StatusCode::ACCEPTED,
        Json(SessionRecoveryAccepted {
            upload_session_id: id,
            segments_started: result.started,
            segments_busy,
            submission_queued,
            blocking_summary,
            session_status: snapshot.status,
            submit_state: snapshot.submit_state,
            submit_requested_at: snapshot.submit_requested_at.unwrap_or(requested_at),
            next_submit_at: snapshot.next_submit_at,
            submit_claimed: snapshot.submit_claim_token.is_some(),
            last_submit_error: snapshot.last_submit_error,
            completeness,
        }),
    ))
}

/// One historical attempt on a lifecycle row, newest first.
#[derive(serde::Serialize, sqlx::FromRow)]
pub struct AttemptHistoryView {
    pub id: i64,
    pub missing_id: i64,
    pub line_key: Option<String>,
    pub line_source: Option<String>,
    pub started_at: chrono::DateTime<Utc>,
    pub ended_at: Option<chrono::DateTime<Utc>>,
    pub phase_reached: Option<String>,
    pub outcome: Option<String>,
    pub uploaded_bytes: i64,
    pub last_chunk_index: Option<i64>,
    pub error: Option<String>,
}

pub async fn get_missing_upload_attempts(
    State(service_register): State<ServiceRegister>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Json<Vec<AttemptHistoryView>>, Response> {
    let rows = sqlx::query_as::<_, AttemptHistoryView>(
        "SELECT id, missing_id, line_key, line_source, started_at, ended_at, phase_reached, \
                outcome, uploaded_bytes, last_chunk_index, error \
         FROM upload_attempt WHERE missing_id = ? ORDER BY id DESC",
    )
    .bind(id)
    .fetch_all(&service_register.pool)
    .await
    .change_context(AppError::Unknown)
    .map_err(report_to_response)?;
    Ok(Json(rows))
}

#[cfg(test)]
mod pending_submit_view_tests {
    use super::*;

    fn row(now: chrono::DateTime<Utc>) -> PendingSubmitRow {
        PendingSubmitRow {
            id: 1,
            live_streamer_id: 2,
            streamer_info_id: 3,
            streamer_name: "test".to_string(),
            stream_title: "test".to_string(),
            stream_started_at: now,
            submit_requested_at: now,
            submit_state: None,
            submit_attempts: 0,
            submit_retry_attempts: 0,
            last_submit_at: None,
            last_submit_error: None,
            next_submit_at: None,
            submit_claim_token: None,
            submit_claimed_at: None,
            aid: None,
            bvid: None,
            status: "uploading".to_string(),
        }
    }

    fn complete() -> SessionCompleteness {
        SessionCompleteness {
            total_expected: 1,
            valid_videos: 1,
            succeeded: 1,
            ..Default::default()
        }
    }

    #[test]
    fn maps_all_five_pending_submit_actions() {
        let now = Utc::now();
        let mut value = row(now);
        let incomplete = SessionCompleteness {
            total_expected: 1,
            pending: 1,
            ..Default::default()
        };
        assert_eq!(
            pending_submit_action(&value, &incomplete, now).0,
            PendingSubmitAction::WaitingSegments
        );
        assert_eq!(
            pending_submit_action(&value, &complete(), now).0,
            PendingSubmitAction::ReadyToSubmit
        );

        value.submit_state = Some("submitting".to_string());
        value.submit_claim_token = Some("claim".to_string());
        value.submit_claimed_at = Some(now);
        assert_eq!(
            pending_submit_action(&value, &complete(), now).0,
            PendingSubmitAction::Submitting
        );

        value.submit_state = Some("failed".to_string());
        value.submit_claim_token = None;
        value.submit_claimed_at = None;
        value.next_submit_at = Some(now + chrono::Duration::minutes(1));
        assert_eq!(
            pending_submit_action(&value, &complete(), now).0,
            PendingSubmitAction::RetryScheduled
        );

        value.submit_state = Some("ok_no_aid".to_string());
        value.submit_claim_token = Some("uncertain".to_string());
        value.next_submit_at = None;
        assert_eq!(
            pending_submit_action(&value, &complete(), now).0,
            PendingSubmitAction::ManualInspection
        );
    }

    #[test]
    fn stale_submit_claim_requires_manual_inspection_without_releasing_it() {
        let now = Utc::now();
        let mut value = row(now);
        value.submit_state = Some("submitting".to_string());
        value.submit_claim_token = Some("held".to_string());
        value.submit_claimed_at = Some(now - chrono::Duration::hours(1));

        assert_eq!(
            pending_submit_action(&value, &complete(), now).0,
            PendingSubmitAction::ManualInspection
        );
        assert_eq!(value.submit_claim_token.as_deref(), Some("held"));
    }

    #[test]
    fn unknown_lifecycle_state_has_an_actionable_recovery_blocker() {
        let snapshot = SessionRecoverySnapshot {
            status: "uploading".to_string(),
            submit_state: Some("blocked_missing_segments".to_string()),
            submit_requested_at: Some(Utc::now()),
            next_submit_at: None,
            submit_claim_token: None,
            last_submit_error: None,
        };
        let completeness = SessionCompleteness {
            total_expected: 1,
            unknown: 1,
            earliest_blocking_segment_id: Some(42),
            reasons: vec!["segment #42 has unknown status".to_string()],
            ..Default::default()
        };

        let blocker =
            recovery_blocking_summary(&snapshot, &completeness, &[], Vec::new(), false).unwrap();
        assert_eq!(blocker.code, "unknown_segment_state");
        assert_eq!(blocker.segment_ids, vec![42]);
    }
}

#[cfg(test)]
mod session_recovery_tests {
    use super::*;
    use crate::server::core::download_manager::DownloadManager;
    use crate::server::infrastructure::connection_pool::ConnectionManager;
    use biliup::bilibili::Video;
    use tracing_subscriber::{EnvFilter, Registry, reload};

    async fn service() -> (tempfile::TempDir, ServiceRegister) {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("session-recovery.db");
        let pool = ConnectionManager::new_pool(database.to_str().unwrap())
            .await
            .unwrap();
        // A direct line keeps the due-segment contract test deterministic and network-free while
        // the claim is being prepared. The detached upload may fail after the response returns.
        let config = Config {
            lines: "bda2".to_string(),
            ..Config::default()
        };
        let (_filter_layer, log_handle) =
            reload::Layer::<EnvFilter, Registry>::new(EnvFilter::new("off"));
        let manager = DownloadManager::new(1, 0, pool.clone());
        let service =
            ServiceRegister::new(pool, Arc::new(RwLock::new(config)), manager, log_handle).await;
        (directory, service)
    }

    async fn insert_session(pool: &ConnectionPool, id: i64, status: &str) {
        let now = Utc::now();
        let room_id = id + 10_000;
        let streamer_info_id = id + 20_000;
        sqlx::query("INSERT INTO livestreamers (id, url, remark) VALUES (?1, ?2, ?3)")
            .bind(room_id)
            .bind(format!("https://example.invalid/live/{id}"))
            .bind(format!("recover-{id}"))
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO streamerinfo (id, name, url, title, date, live_cover_path) \
             VALUES (?1, ?2, ?3, ?4, ?5, '')",
        )
        .bind(streamer_info_id)
        .bind(format!("recover-{id}"))
        .bind(format!("https://example.invalid/live/{id}"))
        .bind("session recovery test")
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO upload_session \
             (id, live_streamer_id, streamer_info_id, videos_json, status, created_at, updated_at) \
             VALUES (?1, ?2, ?3, '[]', ?4, ?5, ?5)",
        )
        .bind(id)
        .bind(room_id)
        .bind(streamer_info_id)
        .bind(status)
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn insert_segment(
        pool: &ConnectionPool,
        session_id: i64,
        segment_id: i64,
        status: &str,
        file_path: &std::path::Path,
    ) {
        let now = Utc::now();
        let video_json = (status == "succeeded").then(|| {
            serde_json::to_string(&Video {
                title: Some(format!("part-{segment_id}")),
                filename: format!("remote-{segment_id}"),
                desc: String::new(),
            })
            .unwrap()
        });
        sqlx::query(
            "INSERT INTO upload_missing_segment \
             (id, live_streamer_id, streamer_info_id, upload_session_id, file_path, \
              normalized_file_path, segment_order, status, next_retry_at, created_at, updated_at, \
              lifecycle_version, video_json) \
             SELECT ?1, live_streamer_id, streamer_info_id, id, ?2, ?2, 0, ?3, ?4, ?4, ?4, 2, ?5 \
             FROM upload_session WHERE id = ?6",
        )
        .bind(segment_id)
        .bind(file_path.display().to_string())
        .bind(status)
        .bind(now)
        .bind(video_json)
        .bind(session_id)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn complete_legacy_session_records_intent_and_queues_submit() {
        let (_directory, service) = service().await;
        insert_session(&service.pool, 701, "uploading").await;
        insert_segment(
            &service.pool,
            701,
            7011,
            "succeeded",
            std::path::Path::new("/already-uploaded.flv"),
        )
        .await;

        let (status, Json(response)) =
            recover_session_uploads(State(service.clone()), axum::extract::Path(701))
                .await
                .unwrap();

        assert_eq!(status, StatusCode::ACCEPTED);
        assert!(response.segments_started.is_empty());
        assert!(!response.segments_busy);
        assert!(response.submission_queued);
        assert!(response.blocking_summary.is_none());
        assert!(response.completeness.is_complete());
        let requested: Option<chrono::DateTime<Utc>> =
            sqlx::query_scalar("SELECT submit_requested_at FROM upload_session WHERE id = 701")
                .fetch_one(&service.pool)
                .await
                .unwrap();
        assert!(requested.is_some());
    }

    #[tokio::test]
    async fn pending_session_view_does_not_depend_on_active_missing_filter() {
        let (_directory, service) = service().await;
        insert_session(&service.pool, 706, "uploading").await;
        insert_segment(
            &service.pool,
            706,
            7061,
            "succeeded",
            std::path::Path::new("/already-complete.flv"),
        )
        .await;
        request_session_submit(&service.pool, 706, Utc::now())
            .await
            .unwrap();

        let Json(views) = get_pending_submit_sessions(State(service)).await.unwrap();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].id, 706);
        assert_eq!(views[0].action, PendingSubmitAction::ReadyToSubmit);
        assert!(views[0].completeness.is_complete());
    }

    #[tokio::test]
    async fn empty_session_discard_endpoint_is_logical_and_idempotent() {
        let (_directory, service) = service().await;
        insert_session(&service.pool, 708, "uploading").await;
        request_session_submit(&service.pool, 708, Utc::now())
            .await
            .unwrap();

        let Json(first) =
            discard_empty_upload_session(State(service.clone()), axum::extract::Path(708))
                .await
                .unwrap();
        assert!(first.discarded);
        assert!(!first.already_finalized);
        assert_eq!(first.submit_state.as_deref(), Some("discarded_empty"));

        let Json(second) =
            discard_empty_upload_session(State(service.clone()), axum::extract::Path(708))
                .await
                .unwrap();
        assert!(second.discarded);
        assert!(second.already_finalized);
        let remaining: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM upload_session WHERE id = 708")
                .fetch_one(&service.pool)
                .await
                .unwrap();
        assert_eq!(
            remaining, 1,
            "discard must retain the finalized identity row"
        );
        let Json(pending) = get_pending_submit_sessions(State(service)).await.unwrap();
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn empty_session_discard_endpoint_rejects_nonempty_or_uncertain_sessions() {
        let (_directory, service) = service().await;
        insert_session(&service.pool, 709, "uploading").await;
        insert_segment(
            &service.pool,
            709,
            7091,
            "pending",
            std::path::Path::new("/pending.flv"),
        )
        .await;
        request_session_submit(&service.pool, 709, Utc::now())
            .await
            .unwrap();
        let nonempty =
            discard_empty_upload_session(State(service.clone()), axum::extract::Path(709))
                .await
                .unwrap_err();
        assert_eq!(nonempty.status(), StatusCode::CONFLICT);

        insert_session(&service.pool, 710, "uploading").await;
        request_session_submit(&service.pool, 710, Utc::now())
            .await
            .unwrap();
        sqlx::query("UPDATE upload_session SET submit_state = 'ok_no_aid' WHERE id = 710")
            .execute(&service.pool)
            .await
            .unwrap();
        let uncertain =
            discard_empty_upload_session(State(service.clone()), axum::extract::Path(710))
                .await
                .unwrap_err();
        assert_eq!(uncertain.status(), StatusCode::CONFLICT);

        let statuses: Vec<(i64, String)> = sqlx::query_as(
            "SELECT id, status FROM upload_session WHERE id IN (709, 710) ORDER BY id",
        )
        .fetch_all(&service.pool)
        .await
        .unwrap();
        assert_eq!(
            statuses,
            vec![
                (709, "uploading".to_string()),
                (710, "uploading".to_string())
            ]
        );
    }

    #[tokio::test]
    async fn unavailable_and_running_segments_return_actionable_blockers() {
        let (_directory, service) = service().await;
        insert_session(&service.pool, 702, "uploading").await;
        insert_segment(
            &service.pool,
            702,
            7021,
            "source_missing",
            std::path::Path::new("/gone.flv"),
        )
        .await;
        let (_, Json(missing)) =
            recover_session_uploads(State(service.clone()), axum::extract::Path(702))
                .await
                .unwrap();
        assert!(missing.submission_queued);
        assert_eq!(
            missing.blocking_summary.as_ref().map(|item| item.code),
            Some("source_missing")
        );

        insert_session(&service.pool, 703, "uploading").await;
        insert_segment(
            &service.pool,
            703,
            7031,
            "uploading",
            std::path::Path::new("/running.flv"),
        )
        .await;
        let (_, Json(running)) =
            recover_session_uploads(State(service.clone()), axum::extract::Path(703))
                .await
                .unwrap();
        assert!(running.segments_busy);
        assert!(!running.submission_queued);
        assert_eq!(
            running.blocking_summary.as_ref().map(|item| item.code),
            Some("segments_busy")
        );
    }

    #[tokio::test]
    async fn existing_submit_claim_requires_inspection_and_is_not_requeued() {
        let (_directory, service) = service().await;
        insert_session(&service.pool, 704, "uploading").await;
        insert_segment(
            &service.pool,
            704,
            7041,
            "succeeded",
            std::path::Path::new("/accepted-remotely.flv"),
        )
        .await;
        sqlx::query(
            "UPDATE upload_session SET submit_claim_token = 'uncertain-claim', \
             submit_claimed_at = ?1, submit_state = 'ok_no_aid' WHERE id = 704",
        )
        .bind(Utc::now())
        .execute(&service.pool)
        .await
        .unwrap();

        let (_, Json(response)) =
            recover_session_uploads(State(service.clone()), axum::extract::Path(704))
                .await
                .unwrap();
        assert!(response.submit_claimed);
        assert!(!response.submission_queued);
        assert_eq!(
            response.blocking_summary.as_ref().map(|item| item.code),
            Some("submission_claimed")
        );
    }

    #[tokio::test]
    async fn due_segment_is_claimed_but_remote_work_is_not_awaited() {
        let (directory, service) = service().await;
        insert_session(&service.pool, 705, "uploading").await;
        let file = directory.path().join("due.flv");
        std::fs::write(&file, b"not real media").unwrap();
        insert_segment(&service.pool, 705, 7051, "pending", &file).await;

        let (status, Json(response)) =
            recover_session_uploads(State(service.clone()), axum::extract::Path(705))
                .await
                .unwrap();
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(response.segments_started, vec![7051]);
        assert!(!response.submission_queued);
        assert_eq!(
            response.blocking_summary.as_ref().map(|item| item.code),
            Some("segments_recovering")
        );
    }

    #[tokio::test]
    async fn missing_and_finalized_sessions_keep_their_http_contract() {
        let (_directory, service) = service().await;
        let missing = recover_session_uploads(State(service.clone()), axum::extract::Path(706))
            .await
            .unwrap_err();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);

        insert_session(&service.pool, 707, "finalized").await;
        let finalized = recover_session_uploads(State(service.clone()), axum::extract::Path(707))
            .await
            .unwrap_err();
        assert_eq!(finalized.status(), StatusCode::CONFLICT);
        let requested: Option<chrono::DateTime<Utc>> =
            sqlx::query_scalar("SELECT submit_requested_at FROM upload_session WHERE id = 707")
                .fetch_one(&service.pool)
                .await
                .unwrap();
        assert!(requested.is_none());
    }
}
