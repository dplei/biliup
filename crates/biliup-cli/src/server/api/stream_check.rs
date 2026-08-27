use crate::server::core::monitor::{CheckOutcome, ManualCheckResult};
use crate::server::errors::ApiError;
use crate::server::infrastructure::service_register::ServiceRegister;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// 主动检查的响应：`outcome` 供前端分支，`message` 直接展示给人看。
#[derive(Debug, Serialize)]
pub struct CheckStreamResponse {
    pub outcome: &'static str,
    pub message: String,
}

fn ok(outcome: &'static str, message: &str) -> Result<Json<CheckStreamResponse>, Response> {
    Ok(Json(CheckStreamResponse {
        outcome,
        message: message.to_string(),
    }))
}

fn err(status: StatusCode, message: String) -> Result<Json<CheckStreamResponse>, Response> {
    Err((status, Json(ApiError::new(message))).into_response())
}

/// `POST /v1/streamers/{id}/check`：立刻检查一次直播流。
///
/// 轮询是「所有房间排队、每轮睡一个间隔」，服务意外重启后要绕完一圈才轮得到某个房间，
/// 已经在播的场次白白少录一段。这个接口把那一次检查提前，命中开播后走的是与轮询完全
/// 相同的录制流程（同场会话复用、租约准入、下载许可都不绕过）。
pub async fn check_stream_now(
    State(state): State<ServiceRegister>,
    Path(id): Path<i64>,
) -> Result<Json<CheckStreamResponse>, Response> {
    match state.managers.check_room_now(id).await {
        ManualCheckResult::Checked(CheckOutcome::Started) => {
            ok("started", "已连接直播流，开始录制")
        }
        ManualCheckResult::Checked(CheckOutcome::Offline) => ok("offline", "主播当前未开播"),
        ManualCheckResult::Checked(CheckOutcome::NoUploadTemplate) => ok(
            "no_upload_template",
            "该直播间未绑定投稿模板，绑定后才会录制",
        ),
        ManualCheckResult::Checked(CheckOutcome::DownloadPoolFull) => ok(
            "download_pool_full",
            "下载池已满，暂时无法开始新的录制，请等待其他录制结束",
        ),
        ManualCheckResult::Checked(CheckOutcome::LeaseRejected) => ok(
            "lease_rejected",
            "录制期限不允许开始新场次，请先延期或清除期限",
        ),
        ManualCheckResult::Recording => ok("already_recording", "该直播间正在录制中"),
        ManualCheckResult::Paused => ok("paused", "该直播间已暂停录制，请先恢复"),
        ManualCheckResult::Busy => ok("checking", "轮询正在检查这个直播间，请稍后查看状态"),
        ManualCheckResult::Checked(CheckOutcome::StartFailed) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "已检测到开播，但创建录制会话失败，请查看实时日志".to_string(),
        ),
        // 检查直播间本身失败（网络、cookie 失效等）如实报错，不要用 200 把它包装成正常结果。
        ManualCheckResult::Checked(CheckOutcome::CheckFailed(reason)) => {
            err(StatusCode::BAD_GATEWAY, format!("检查直播间出错：{reason}"))
        }
        ManualCheckResult::NotFound => err(
            StatusCode::NOT_FOUND,
            "直播间不存在或未在监控中".to_string(),
        ),
    }
}
