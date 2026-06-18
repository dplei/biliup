/// 钩子步骤模块
pub mod hook_step;
pub mod live_streamer;
pub mod upload_streamer;

use chrono::serde::ts_seconds;
use chrono::{DateTime, Utc};
use ormlite::{Insert, Model};
use serde::{Deserialize, Serialize};
/// 主播信息模型
/// 存储主播的基本信息和直播状态
#[derive(Model, Debug, Clone, Serialize, Deserialize, Default)]
#[ormlite(table = "streamerinfo")]
pub struct StreamerInfo {
    /// 主键ID
    pub id: i64,
    /// 主播名称
    pub name: String,
    /// 直播间URL
    pub url: String,
    /// 直播标题
    pub title: String,
    #[serde(with = "ts_seconds")]
    /// 直播开始时间
    pub date: DateTime<Utc>,
    /// 直播封面路径（可选）
    pub live_cover_path: String,
}

impl StreamerInfo {
    pub fn new(
        name: &str,
        url: &str,
        title: &str,
        date: DateTime<Utc>,
        live_cover_path: &str,
    ) -> Self {
        Self {
            id: -1,
            name: name.to_string(),
            url: url.to_string(),
            title: title.to_string(),
            date,
            live_cover_path: live_cover_path.to_string(),
        }
    }
}

/// 文件列表模型
/// 存储录制文件的信息
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[ormlite(table = "filelist", insert = "InsertFileItem")]
pub struct FileItem {
    /// 主键ID
    pub id: i64,
    /// 文件路径
    pub file: String,
    /// 关联的主播信息ID（外键，非空）
    pub streamer_info_id: i64,
}

/// 增量投稿会话模型
/// 一场直播对应一行，记录 B 站稿件号与已投稿视频列表（JSON）。
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[ormlite(table = "upload_session", insert = "InsertUploadSession")]
pub struct UploadSession {
    /// 主键ID
    pub id: i64,
    /// 配置直播间(room)稳定 id，跨重启匹配用
    pub live_streamer_id: i64,
    /// 当前挂接的会话 id（重启续接时更新）
    pub streamer_info_id: i64,
    /// B站稿件号，None=还没建稿
    pub aid: Option<i64>,
    /// B站 bvid
    pub bvid: Option<String>,
    /// 已成功投稿的 Video 列表（JSON 字符串），edit 时携带
    pub videos_json: String,
    /// uploading / submitted / finalized
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Missing segment recovery queue.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[ormlite(
    table = "upload_missing_segment",
    insert = "InsertUploadMissingSegment"
)]
pub struct UploadMissingSegment {
    pub id: i64,
    pub live_streamer_id: i64,
    pub streamer_info_id: i64,
    pub upload_session_id: Option<i64>,
    pub aid: Option<i64>,
    pub file_path: String,
    pub danmaku_file_path: Option<String>,
    pub segment_order: i64,
    pub status: String,
    pub attempts: i64,
    pub line_index: i64,
    pub next_retry_at: DateTime<Utc>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// 配置模型
/// 存储应用程序的配置信息
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[ormlite(table = "configuration")]
pub struct Configuration {
    /// 主键ID
    pub id: i64,
    /// 配置键
    pub key: String,
    /// 配置值（TEXT类型）
    pub value: String,
}

/// 插入配置的数据结构
/// 用于创建新的配置记录
#[derive(Insert, Debug, Clone, Serialize, Deserialize)]
#[ormlite(returns = "Configuration")]
pub struct InsertConfiguration {
    /// 配置键
    pub key: String,
    /// 配置值（TEXT类型）
    pub value: String,
}
