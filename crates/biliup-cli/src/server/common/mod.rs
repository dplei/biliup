use axum::http::{HeaderMap, HeaderName, HeaderValue};
use std::collections::HashMap;
use std::str::FromStr;

pub mod attempt_lease;
pub mod audio_normalization;
/// 平台 cookie 健康监测（检测失效并经横幅/webhook 提示）
pub mod cookie_health;
pub mod cover_generator;
/// 文件系统可用空间探测（标准化的准入与硬水位共用）
pub mod disk_space;
pub mod download;
pub mod ffmpeg_scan;
pub mod lifecycle_backfill;
pub mod missing_segment;
/// 用户提供路径的安全解析（静态文件接口与后续的图片上传共用）
pub mod path_safety;
pub mod process_priority;
/// 单直播间录制租约：持久状态机、到期扫描、录制准入与可靠通知。
pub mod recording_lease;
pub mod recovery_eligibility;
/// 到期补传的主动扫描与后台执行（接口只 claim，上传在这里跑）
pub mod recovery_scheduler;
pub mod route_health;
pub mod segment_enrollment;
/// 数据库驱动的待投稿会话启动/周期协调扫描。
pub mod submission_scheduler;
/// 上传前时间戳异常检测与修复
pub mod timestamp_repair;
pub mod upload;
pub mod upload_line_health;
/// 全仓唯一的上传线路决策（配置优先、冷却回退、auto 探测）
pub mod upload_line_selection;
pub mod upload_rate_gate;
pub mod upload_session;
/// 通用工具函数
pub mod util;

pub fn construct_headers(hash_map: &HashMap<String, String>) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    for (key, value) in hash_map.iter() {
        let name =
            HeaderName::from_str(key).map_err(|e| format!("invalid header name {key:?}: {e}"))?;
        let value = HeaderValue::from_str(value)
            .map_err(|e| format!("invalid header value for {key:?}: {e}"))?;
        headers.insert(name, value);
    }
    Ok(headers)
}
