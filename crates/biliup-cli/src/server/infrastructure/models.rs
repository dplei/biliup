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

    /// 反查不到主播时的占位信息：名字取自上传模板名，其余字段一律留空。
    ///
    /// 留空是有意的。这些字段会流进投稿：`url` 是「转载来源」在未显式配置时的兜底值，
    /// `title` 会展开进标题与简介模板。宁可空着让人看出「这里没信息」，
    /// 也不要塞一个看起来像数据的假值——那会被原样提交到 B 站。
    ///
    /// 单独开一个构造器，是因为 `new` 有三个相邻的 `&str` 参数，顺序写错照样编译。
    pub fn placeholder(name: &str, date: DateTime<Utc>) -> Self {
        Self::new(name, "", "", date, "")
    }
}

#[cfg(test)]
mod streamer_info_tests {
    use super::*;

    /// 锁住占位信息的字段映射。历史上这里的位置参数被写错过一位，
    /// 结果「转载来源」被提交成了字面量 `stream_title`。
    #[test]
    fn placeholder_leaves_everything_but_name_empty() {
        let info = StreamerInfo::placeholder("我的模板", Utc::now());

        assert_eq!(info.name, "我的模板");
        assert_eq!(info.url, "", "url 会成为转载来源的兜底值，必须留空");
        assert_eq!(info.title, "", "title 会展开进标题与简介模板，必须留空");
        assert_eq!(info.live_cover_path, "");
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
    /// 本场下播一次性投稿的累计尝试次数。
    pub submit_attempts: i64,
    /// 最近一次投稿时间。
    pub last_submit_at: Option<DateTime<Utc>>,
    /// 最近一次投稿异常摘要（成功且有 aid 时为 None）。
    pub last_submit_error: Option<String>,
    /// 投稿结果：ok_with_aid / ok_no_aid / failed；None=未投。
    pub submit_state: Option<String>,
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
    pub normalized_file_path: Option<String>,
    pub lifecycle_version: i64,
    pub video_json: Option<String>,
    pub total_bytes: Option<i64>,
    pub uploaded_bytes: i64,
    pub current_line: Option<String>,
    pub upload_started_at: Option<DateTime<Utc>>,
    pub last_progress_at: Option<DateTime<Utc>>,
    pub attempt_token: Option<String>,
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
