use crate::server::common::recording_lease::{self, RecordingLeaseProjection, RecordingLeaseState};
use crate::server::errors::{ApiError, report_to_response};
use crate::server::infrastructure::context::{Stage, WorkerStatus};
use crate::server::infrastructure::repositories::find_streamer;
use crate::server::infrastructure::service_register::ServiceRegister;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::info;

#[derive(Debug, Deserialize)]
pub struct PutRecordingLeaseRequest {
    pub expires_at: DateTime<Utc>,
    /// 选填；不传等同于空备注。
    #[serde(default)]
    pub customer_note: String,
    pub expected_lease_id: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct RecordingLeaseMutationResponse {
    pub recording_lease: Option<RecordingLeaseProjection>,
    pub server_now: DateTime<Utc>,
}

#[derive(Debug, Deserialize, Clone, Copy)]
pub struct RecordingStateRequest {
    pub paused: bool,
}

fn api_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(ApiError::new(message.into()))).into_response()
}

async fn streamer_exists(state: &ServiceRegister, id: i64) -> Result<(), Response> {
    match find_streamer(&state.pool, id).await {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(api_error(StatusCode::NOT_FOUND, "直播间不存在")),
        Err(report) => Err(report_to_response(report)),
    }
}

pub async fn put_recording_lease(
    State(state): State<ServiceRegister>,
    Path(id): Path<i64>,
    Json(mut payload): Json<PutRecordingLeaseRequest>,
) -> Result<Json<RecordingLeaseMutationResponse>, Response> {
    streamer_exists(&state, id).await?;
    let now = Utc::now();
    payload.customer_note = payload.customer_note.trim().to_string();
    // 备注是选填的：留空存空串，通知文案自己兜底，不必为此改列约束。
    if payload.customer_note.chars().count() > 200 {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "客户/需求备注去除首尾空格后不得超过 200 个字符",
        ));
    }
    if payload.expires_at <= now {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "录制期限必须晚于服务器当前时间",
        ));
    }
    let current = recording_lease::current_lease(&state.pool, id)
        .await
        .map_err(report_to_response)?;
    if current.as_ref().map(|lease| lease.id) != payload.expected_lease_id {
        return Err(api_error(
            StatusCode::CONFLICT,
            "录制期限已被其他页面更新，请刷新后重试",
        ));
    }
    let outcome = recording_lease::replace_lease(
        &state.pool,
        id,
        payload.expires_at,
        &payload.customer_note,
        payload.expected_lease_id,
        now,
    )
    .await
    .map_err(|report| {
        if report.to_string().contains("其他页面") {
            api_error(StatusCode::CONFLICT, report.to_string())
        } else {
            report_to_response(report)
        }
    })?;
    if outcome.resume_lease_owned_pause {
        recording_lease::resume_worker_if_owned(&state.managers, id).await;
    }
    Ok(Json(RecordingLeaseMutationResponse {
        recording_lease: Some(outcome.lease.projection()),
        server_now: now,
    }))
}

pub async fn delete_recording_lease(
    State(state): State<ServiceRegister>,
    Path((id, lease_id)): Path<(i64, i64)>,
) -> Result<Json<RecordingLeaseMutationResponse>, Response> {
    streamer_exists(&state, id).await?;
    if let Some(current) = recording_lease::current_lease(&state.pool, id)
        .await
        .map_err(report_to_response)?
        && current.id != lease_id
    {
        return Err(api_error(
            StatusCode::CONFLICT,
            "录制期限已被其他页面更新，请刷新后重试",
        ));
    }
    let now = Utc::now();
    let outcome = recording_lease::cancel_lease(&state.pool, id, lease_id, now)
        .await
        .map_err(|report| {
            let message = report.to_string();
            if message.contains("不存在") {
                api_error(StatusCode::NOT_FOUND, message)
            } else if message.contains("替换") || message.contains("其他页面") {
                api_error(StatusCode::CONFLICT, message)
            } else {
                report_to_response(report)
            }
        })?;
    if outcome.resume_lease_owned_pause {
        recording_lease::resume_worker_if_owned(&state.managers, id).await;
    }
    Ok(Json(RecordingLeaseMutationResponse {
        recording_lease: None,
        server_now: now,
    }))
}

pub async fn set_recording_state(
    state: &ServiceRegister,
    id: i64,
    paused: bool,
) -> Result<(), Response> {
    let Some(worker) = state.managers.get_room_by_id(id).await else {
        return Err(api_error(StatusCode::NOT_FOUND, "直播间不存在"));
    };
    let current_status = worker.downloader_status.read().unwrap().clone();
    if paused {
        if !matches!(current_status, WorkerStatus::Pause) {
            worker
                .change_status(Stage::Download, WorkerStatus::Pause)
                .await;
            state.managers.make_waker(id).await;
            info!(live_streamer_id = id, "人工暂停直播间");
        }
        return Ok(());
    }

    if recording_lease::current_lease(&state.pool, id)
        .await
        .map_err(report_to_response)?
        .is_some_and(|lease| lease.parsed_state().ok() == Some(RecordingLeaseState::ExpiredPaused))
    {
        return Err(api_error(
            StatusCode::CONFLICT,
            "该直播间已因录制期限到期暂停，请先延期或清除期限",
        ));
    }
    if matches!(current_status, WorkerStatus::Pause) {
        worker
            .change_status(Stage::Download, WorkerStatus::Idle)
            .await;
        state.managers.wake_waker(id).await;
        info!(live_streamer_id = id, "人工恢复直播间");
    }
    Ok(())
}

pub async fn put_recording_state(
    State(state): State<ServiceRegister>,
    Path(id): Path<i64>,
    Json(payload): Json<RecordingStateRequest>,
) -> Result<Json<()>, Response> {
    set_recording_state(&state, id, payload.paused).await?;
    Ok(Json(()))
}

/// 旧 toggle 路由的兼容层；恢复分支仍经过同一租约守卫。
pub async fn toggle_recording_state(
    State(state): State<ServiceRegister>,
    Path(id): Path<i64>,
) -> Result<Json<()>, Response> {
    let Some(worker) = state.managers.get_room_by_id(id).await else {
        return Err(api_error(StatusCode::NOT_FOUND, "直播间不存在"));
    };
    let paused = !matches!(
        *worker.downloader_status.read().unwrap(),
        WorkerStatus::Pause
    );
    set_recording_state(&state, id, paused).await?;
    Ok(Json(()))
}
