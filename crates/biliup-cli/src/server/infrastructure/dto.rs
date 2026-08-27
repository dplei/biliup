use crate::server::common::recording_lease::RecordingLeaseProjection;
use crate::server::infrastructure::models::live_streamer::LiveStreamer;
use chrono::{DateTime, Utc};
use serde::Serialize;

/// 直播主播响应数据传输对象
/// 包含主播信息和当前工作状态
#[derive(Serialize)]
pub struct LiveStreamerResponse {
    /// 主播基本信息（展开到顶层）
    #[serde(flatten)]
    pub inner: LiveStreamer,

    /// 当前工作状态
    pub status: String,
    /// 上传状态
    pub upload_status: String,
    /// 当前录制的实际画质代码（录制中才有值）
    pub recording_quality: Option<String>,
    /// 当前活动录制租约；终态历史不出现在卡片列表。
    pub recording_lease: Option<RecordingLeaseProjection>,
    /// 本次列表响应生成时的服务器 UTC 时间，供期限弹窗显示；权威校验仍在 mutation 内完成。
    pub server_now: DateTime<Utc>,
}
