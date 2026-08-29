use crate::server::core::downloader::DownloaderType;
use crate::server::errors::{AppError, AppResult};
use crate::server::infrastructure::models::hook_step::HookStep;
use biliup::bilibili::Credit;
use error_stack::{ResultExt, bail};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, path::Path, path::PathBuf};
use struct_patch::Patch;

/// 全局配置结构体
#[derive(bon::Builder, Debug, PartialEq, Clone, Serialize, Deserialize, Patch)]
#[patch(attribute(derive(Debug, Clone, Default, Deserialize, Serialize)))]
pub struct Config {
    // ===== 全局录播与上传设置 =====
    /// 下载器类型：streamlink | ffmpeg | stream-gears | 自定义
    #[serde(default)]
    pub downloader: Option<DownloaderType>,

    /// 文件大小限制（字节）
    #[patch(attribute(serde(default, deserialize_with = "deserialize_option_patch")))]
    #[serde(default = "default_file_size")]
    pub file_size: Option<u64>,

    /// 分段时间，格式如 "00:00:00"，保留为字符串以保持直观
    #[serde(default)]
    pub segment_time: Option<String>,

    /// 过滤阈值（MB）
    #[builder(default = default_filtering_threshold())]
    #[serde(default = "default_filtering_threshold")]
    pub filtering_threshold: u64,

    /// 保留通过媒体探测的短分段。true = 新的止损行为；false = 旧的体积过滤。
    #[builder(default)]
    #[serde(default)]
    pub preserve_recoverable_short_segments: bool,

    /// 可恢复短分段策略。`merge_or_defer` 是安全默认值；关闭保留开关可整体回滚。
    #[builder(default = default_recoverable_short_segment_mode())]
    #[serde(default = "default_recoverable_short_segment_mode")]
    pub recoverable_short_segment_mode: String,

    /// 单个恢复批次最多包含的文件数。
    #[builder(default = default_recoverable_short_batch_max_files())]
    #[serde(default = "default_recoverable_short_batch_max_files")]
    pub recoverable_short_batch_max_files: usize,

    /// 延迟恢复建议重试间隔（秒），写入 durable manifest 供恢复器/运维使用。
    #[builder(default = default_recoverable_short_retry_interval_secs())]
    #[serde(default = "default_recoverable_short_retry_interval_secs")]
    pub recoverable_short_retry_interval_secs: u64,

    /// 是否启用独立的拉流线路健康计数与指数退避。true = 开启；false = 旧流程。
    #[builder(default)]
    #[serde(default)]
    pub route_health_enabled: bool,

    /// 码流停顿看门狗阈值（秒）：连续这么久一个字节都没收到就判连接已死并重连。
    ///
    /// 每收到一个 chunk 就重置，语义不是连接总时长。缺省沿用 30 秒——高码率源被上游掐断后
    /// 这 30 秒是白等的，边界缺口的大头就在这里；确认被掐的房间可下调到 6~8 秒。
    /// 代价是网络抖动超过阈值时会多一次重连（多一个分 P），所以默认保持 30 以便回滚。
    #[serde(default)]
    pub stream_stall_timeout_secs: Option<u64>,

    /// 文件名前缀
    #[serde(default)]
    pub filename_prefix: Option<String>,

    /// 分段处理器是否并行执行
    #[serde(default)]
    pub segment_processor_parallel: Option<bool>,

    /// 删除时机：本地切片文件何时执行后处理（如 rm 删本地）。
    /// "stream_end"/None = 下播后统一删除（默认）；
    /// "per_segment" = 每片上传成功后立即删除，磁盘峰值≈单个切片，适合小磁盘机器。
    /// 注意：submit（生成稿件）始终在下播后一次性进行，此项只改后处理（删除）的时机。
    #[serde(default)]
    pub segment_delete_mode: Option<String>,

    /// 上传前时间戳异常检测与修复总开关。
    /// None/true = 开启（默认）：每个分段上传前扫描时间戳，异常则自动 remux/重编码修复，
    /// 避免 B 站转码因「时间戳跳变」失败；正常片零额外写盘。
    /// false = 关闭：直接上传原片（旧行为）。
    #[serde(default)]
    pub timestamp_repair: Option<bool>,

    /// 上传前自动统一录音响度。默认关闭；只重编码主音轨，不重编码视频。
    #[builder(default)]
    #[serde(default)]
    pub audio_normalization_enabled: bool,

    /// 相对推荐目标（-16 LUFS）的音量偏移，网页推子使用，允许 -6..=4 dB。
    #[builder(default)]
    #[serde(default)]
    pub audio_normalization_offset_db: i8,

    /// 全局通知 webhook（可选）。字段名保留 `cookie_health` 只是历史原因——它早已是所有
    /// 运维通知的统一出口：cookie 失效与恢复、抖音录制画质降级、上传线路熔断、投稿结果、
    /// 录制租约到期暂停，全部走这一个地址，没有分事件的独立配置项。
    /// URL 含 `{title}`/`{content}` 占位 → GET 替换（兼容 Bark/Server酱）；
    /// 否则 POST JSON `{"title":..,"content":..}`（兼容企业微信/钉钉/自建）。
    /// 留空则以上通知都不发送（cookie 问题仍在网页横幅提示，租约到期会停在 `not_configured`）。
    #[serde(default)]
    pub cookie_health_webhook: Option<String>,

    /// 合集分区ID（season section_id）。每个主播在「录播管理」里经 override 各设各的，
    /// 投稿成功后自动把新稿件加入该合集（B 站「视频合集」）。留空=不加合集。
    /// section_id 通过创作中心合集管理或「列出我的合集」接口获取。
    #[serde(default)]
    pub season_section_id: Option<i64>,

    /// 增量投稿重启续接时间窗口（分钟）。留空回退默认 30。
    /// 重启后某 room 在窗口内存在未 finalize 的会话则续接其 aid，否则新建稿。
    #[serde(default)]
    pub recovery_window_minutes: Option<u64>,

    /// 上传器类型：Noop | bili_web | biliup-rs | 其他
    #[serde(default)]
    pub uploader: Option<String>,

    /// 进程级预上传节流与 B 站 601 冷却总开关。
    #[builder(default = default_upload_rate_gate_enabled())]
    #[serde(default = "default_upload_rate_gate_enabled")]
    pub upload_rate_gate_enabled: bool,

    /// 两次 pre_upload 之间的最小间隔（秒）。
    #[builder(default = default_upload_min_request_interval_secs())]
    #[serde(default = "default_upload_min_request_interval_secs")]
    pub upload_min_request_interval_secs: u64,

    /// 首次 601 的冷却时间（秒）。
    #[builder(default = default_upload_601_initial_cooldown_secs())]
    #[serde(default = "default_upload_601_initial_cooldown_secs")]
    pub upload_601_initial_cooldown_secs: u64,

    /// 连续 601 指数退避上限（秒）。
    #[builder(default = default_upload_601_max_cooldown_secs())]
    #[serde(default = "default_upload_601_max_cooldown_secs")]
    pub upload_601_max_cooldown_secs: u64,

    /// 提交API类型：web | client
    #[serde(default)]
    pub submit_api: Option<String>,

    /// 上传线路：AUTO | alia | bda2 | bldsa | qn | tx | txa
    #[builder(default = default_lines())]
    #[serde(default = "default_lines")]
    pub lines: String,

    /// 上传线程数
    #[builder(default = default_threads())]
    #[serde(default = "default_threads")]
    pub threads: u32,

    /// 延迟时间（秒）
    #[builder(default = default_delay())]
    #[serde(default = "default_delay")]
    pub delay: u64,

    /// 事件循环间隔（秒）
    #[builder(default = default_event_loop_interval())]
    #[serde(default = "default_event_loop_interval")]
    pub event_loop_interval: u64,

    /// 检查器休眠时间（秒）
    #[builder(default = default_checker_sleep())]
    #[serde(default = "default_checker_sleep")]
    pub checker_sleep: u64,

    /// 连接池1大小
    #[builder(default = default_pool1_size())]
    #[serde(default = "default_pool1_size")]
    pub pool1_size: u32,

    /// 连接池2大小
    #[builder(default = default_pool2_size())]
    #[serde(default = "default_pool2_size")]
    pub pool2_size: u32,

    // ===== 各平台录播设置 =====
    /// 是否使用直播封面
    #[serde(default)]
    pub use_live_cover: Option<bool>,

    // 斗鱼平台设置
    /// 斗鱼CDN节点
    #[serde(default)]
    pub douyu_cdn: Option<String>,
    /// 斗鱼弹幕录制
    #[serde(default)]
    pub douyu_danmaku: Option<bool>,
    /// 斗鱼码率
    #[serde(default)]
    pub douyu_rate: Option<u32>,
    /// 斗鱼互动游戏运行时跳过录制
    #[serde(default)]
    pub douyu_disable_interactive_game: Option<bool>,

    // 虎牙平台设置
    /// 虎牙CDN节点
    #[serde(default)]
    pub huya_cdn: Option<String>,
    /// 虎牙CDN回退
    #[serde(default)]
    pub huya_cdn_fallback: Option<bool>,
    /// 虎牙弹幕录制
    #[serde(default)]
    pub huya_danmaku: Option<bool>,
    /// 虎牙最大比率
    #[serde(default)]
    pub huya_max_ratio: Option<u32>,
    /// 虎牙 Flv or Hls
    #[serde(default)]
    pub huya_protocol: Option<String>,
    /// 虎牙是否保留 imgplus 流名
    #[serde(default)]
    pub huya_imgplus: Option<bool>,
    /// 虎牙编码参数
    #[serde(default)]
    pub huya_codec: Option<String>,

    // 抖音平台设置
    /// 抖音弹幕录制
    #[serde(default)]
    pub douyin_danmaku: Option<bool>,
    /// 抖音画质
    #[serde(default)]
    pub douyin_quality: Option<String>,
    /// 抖音直播协议：flv 或 hls
    #[serde(default)]
    pub douyin_protocol: Option<String>,
    /// 双屏直播录制方式
    #[serde(default)]
    pub douyin_double_screen: Option<bool>,
    /// 抖音真原画
    #[serde(default)]
    pub douyin_true_origin: Option<bool>,
    /// 抖音候选线路模型与后续自动切线总开关。
    #[serde(default)]
    pub douyin_route_failover: Option<bool>,
    /// 是否保留同画质的备用协议候选（默认 FLV 后 HLS）。
    #[serde(default)]
    pub douyin_protocol_fallback: Option<bool>,
    /// 是否允许生成低一档画质候选；默认关闭。
    #[serde(default)]
    pub douyin_quality_fallback: Option<bool>,
    /// 自动降画质允许到达的最低档，默认 hd。
    #[serde(default)]
    pub douyin_min_fallback_quality: Option<String>,
    /// 抖音画质降级告警阈值：实际录到的画质低于此档时 webhook 推送。
    /// 取值同画质（origin/uhd/hd/sd/ld/md），"off"=关闭；缺省视为 "uhd"（蓝光）。
    #[serde(default)]
    pub douyin_quality_alert: Option<String>,

    // 快手平台设置
    /// 快手Cookie
    #[serde(default)]
    pub kuaishou_cookie: Option<String>,

    // 网易 CC 平台设置
    /// 直播协议：hls 或 flv
    #[serde(default)]
    pub cc_protocol: Option<String>,

    // Kilakila 平台设置
    /// 直播协议：hls 或 flv
    #[serde(default)]
    pub kila_protocol: Option<String>,

    // 哔哩哔哩平台设置
    /// B站弹幕录制
    #[serde(default)]
    pub bilibili_danmaku: Option<bool>,
    /// B站弹幕详细信息
    #[serde(default)]
    pub bilibili_danmaku_detail: Option<bool>,
    /// B站弹幕原始数据
    #[serde(default)]
    pub bilibili_danmaku_raw: Option<bool>,
    /// B站协议类型：stream | hls_ts | hls_fmp4
    #[serde(default)]
    pub bili_protocol: Option<String>,
    /// B站CDN节点列表
    #[serde(default)]
    pub bili_cdn: Option<Vec<String>>,
    /// B站强制原画
    #[serde(default)]
    pub bili_force_source: Option<bool>,
    /// B站直播API
    #[serde(default)]
    pub bili_liveapi: Option<String>,
    /// B站回退API
    #[serde(default)]
    pub bili_fallback_api: Option<String>,
    /// B站CDN回退
    #[serde(default)]
    pub bili_cdn_fallback: Option<bool>,
    /// B站 hls_fmp4 转码等待时间（秒）
    #[serde(default)]
    pub bili_hls_transcode_timeout: Option<u64>,
    /// B站cn01节点替换
    #[serde(default)]
    pub bili_replace_cn01: Option<Vec<String>>,
    /// B站画质编号
    #[serde(default)]
    pub bili_qn: Option<u32>,
    /// B站免登录原画
    #[serde(default)]
    pub bili_anonymous_origin: Option<bool>,

    // YouTube平台设置
    /// YouTube首选视频编码
    #[serde(default)]
    pub youtube_prefer_vcodec: Option<String>,
    /// YouTube首选音频编码
    #[serde(default)]
    pub youtube_prefer_acodec: Option<String>,
    /// YouTube最大分辨率
    #[serde(default)]
    pub youtube_max_resolution: Option<u32>,
    /// YouTube最大视频大小
    #[serde(default)]
    pub youtube_max_videosize: Option<String>,
    /// YouTube开始日期
    #[serde(default)]
    pub youtube_after_date: Option<String>,
    /// YouTube结束日期
    #[serde(default)]
    pub youtube_before_date: Option<String>,
    /// YouTube启用直播下载
    #[serde(default)]
    pub youtube_enable_download_live: Option<bool>,
    /// YouTube启用回放下载
    #[serde(default)]
    pub youtube_enable_download_playback: Option<bool>,
    /// YouTube弹幕录制
    #[serde(default)]
    pub youtube_danmaku: Option<bool>,
    /// 兼容旧版配置的 YouTube 弹幕录制字段
    #[serde(default)]
    pub ytb_danmaku: Option<bool>,

    // Twitch平台设置
    /// Twitch弹幕录制
    #[serde(default)]
    pub twitch_danmaku: Option<bool>,
    /// Twitch禁用广告
    #[serde(default)]
    pub twitch_disable_ads: Option<bool>,

    // TwitCasting平台设置
    /// TwitCasting弹幕录制
    #[serde(default)]
    pub twitcasting_danmaku: Option<bool>,
    /// TwitCasting密码
    #[serde(default)]
    pub twitcasting_password: Option<String>,
    /// TwitCasting画质 high | medium | low
    #[serde(default)]
    pub twitcasting_quality: Option<String>,

    /// 录制主播配置映射
    #[serde(default)]
    pub streamers: HashMap<String, StreamerConfig>,

    /// 用户Cookie配置
    #[serde(default)]
    pub user: Option<UserConfig>,

    pub loggers_level: Option<String>,
}

/// 主播配置结构体
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StreamerConfig {
    /// 直播间URL列表
    pub url: Vec<String>,

    /// 视频标题
    #[serde(default)]
    pub title: Option<String>,

    /// 分区ID
    #[serde(default)]
    pub tid: Option<u32>,

    /// 版权类型
    #[serde(default)]
    pub copyright: Option<u8>,

    /// 转载来源
    #[serde(default)]
    pub copyright_source: Option<String>,

    /// 封面路径
    #[serde(default)]
    pub cover_path: Option<PathBuf>,

    /// 封面文字模板（留空=用 cover_path；填写=生成黑底封面，优先）
    #[serde(default)]
    pub cover_template: Option<String>,

    /// 封面背景图文件名（留空=纯黑底）。文件需在背景图目录下，可用网页上传。
    ///
    /// config.toml 导入走的是 upsert，模型里有的列都会被写一遍；这里若不给
    /// 配置项，导入就会把 `config:` 模板已配的背景清成 NULL。
    #[serde(default)]
    pub cover_background: Option<String>,

    /// 视频描述（保留缩进和多行格式）
    #[serde(default)]
    pub description: Option<String>,

    #[serde(default)]
    pub credits: Option<Vec<Credit>>,

    #[serde(default)]
    pub dynamic: Option<String>,

    #[serde(default)]
    pub dtime: Option<u64>,

    #[serde(default)]
    pub dolby: Option<u8>,

    #[serde(default)]
    pub hires: Option<u8>,

    #[serde(default)]
    pub charging_pay: Option<u8>,

    #[serde(default)]
    pub no_reprint: Option<u8>,

    #[serde(default)]
    pub up_selection_reply: Option<u8>,

    #[serde(default)]
    pub up_close_reply: Option<u8>,

    #[serde(default)]
    pub up_close_danmu: Option<u8>,

    #[serde(default)]
    pub is_only_self: Option<u8>,

    #[serde(default)]
    pub uploader: Option<String>,

    #[serde(default)]
    pub filename_prefix: Option<String>,

    #[serde(default)]
    pub user_cookie: Option<String>,

    #[serde(default)]
    pub use_live_cover: Option<bool>,

    #[serde(default)]
    pub tags: Option<Vec<String>>,

    #[serde(default)]
    pub extra_fields: Option<String>,

    #[serde(default)]
    pub time_range: Option<String>,

    #[serde(default)]
    pub excluded_keywords: Option<Vec<String>>,

    #[serde(default)]
    pub preprocessor: Option<Vec<HookStep>>,

    #[serde(default)]
    pub segment_processor: Option<Vec<HookStep>>,

    #[serde(default)]
    pub downloaded_processor: Option<Vec<HookStep>>,

    #[serde(default)]
    pub postprocessor: Option<Vec<HookStep>>,

    #[serde(default)]
    pub format: Option<String>,

    #[serde(default)]
    pub opt_args: Option<Vec<String>>,

    // “override” 是字段名，这里改为 override_cfg 避免与保留字混淆
    #[serde(rename = "override", default)]
    pub override_cfg: Option<HashMap<String, serde_json::Value>>,
}

/// 用户配置结构体
#[derive(bon::Builder, PartialEq, Debug, Clone, Serialize, Deserialize, Default, Patch)]
#[patch(attribute(derive(Debug, Default, Deserialize)))]
pub struct UserConfig {
    // B站配置
    /// B站Cookie字符串
    #[serde(default)]
    pub bili_cookie: Option<String>,
    /// B站Cookie文件路径
    #[serde(default)]
    pub bili_cookie_file: Option<PathBuf>,

    // 抖音配置
    /// 抖音Cookie
    #[serde(default)]
    pub douyin_cookie: Option<String>,

    // Twitch配置
    /// Twitch Cookie
    #[serde(default)]
    pub twitch_cookie: Option<String>,

    // TwitCasting配置
    /// TwitCasting Cookie
    #[serde(default)]
    pub twitcasting_cookie: Option<String>,

    // YouTube配置
    /// YouTube Cookie文件路径
    #[serde(default)]
    pub youtube_cookie: Option<PathBuf>,

    // Niconico配置（使用rename保持与配置文件一致）
    /// Niconico邮箱
    #[serde(rename = "niconico-email", default)]
    pub niconico_email: Option<String>,
    /// Niconico密码
    #[serde(rename = "niconico-password", default)]
    pub niconico_password: Option<String>,
    /// Niconico用户会话
    #[serde(rename = "niconico-user-session", default)]
    pub niconico_user_session: Option<String>,
    /// Niconico清除凭据
    #[serde(rename = "niconico-purge-credentials", default)]
    pub niconico_purge_credentials: Option<String>,

    // AfreecaTV配置
    /// AfreecaTV用户名
    #[serde(default)]
    pub afreecatv_username: Option<String>,
    /// AfreecaTV密码
    #[serde(default)]
    pub afreecatv_password: Option<String>,
}

/// 默认文件大小：2.5GB
fn default_file_size() -> Option<u64> {
    Some(2_621_440_000)
}

fn deserialize_option_patch<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// 默认分段时间：不启用时长分段
pub fn default_segment_time() -> Option<String> {
    None
}

/// 默认过滤阈值：20MB
fn default_filtering_threshold() -> u64 {
    20
}

fn default_recoverable_short_segment_mode() -> String {
    "merge_or_defer".to_string()
}

fn default_recoverable_short_batch_max_files() -> usize {
    60
}

fn default_recoverable_short_retry_interval_secs() -> u64 {
    15 * 60
}

fn default_upload_rate_gate_enabled() -> bool {
    true
}

fn default_upload_min_request_interval_secs() -> u64 {
    2
}

fn default_upload_601_initial_cooldown_secs() -> u64 {
    60
}

fn default_upload_601_max_cooldown_secs() -> u64 {
    30 * 60
}

/// 默认上传线路：自动选择
fn default_lines() -> String {
    "AUTO".to_string()
}

/// 默认线程数：3
fn default_threads() -> u32 {
    3
}

/// 默认延迟：300秒
fn default_delay() -> u64 {
    300
}

/// 默认事件循环间隔：30秒
fn default_event_loop_interval() -> u64 {
    30
}

/// 默认检查器休眠时间：10秒
fn default_checker_sleep() -> u64 {
    10
}

/// 默认连接池1大小：5
fn default_pool1_size() -> u32 {
    5
}

/// 默认连接池2大小：3
fn default_pool2_size() -> u32 {
    3
}

impl Default for Config {
    fn default() -> Self {
        serde_json::from_value(serde_json::json!({})).expect("default config should deserialize")
    }
}

impl Config {
    pub fn validate_segment_limits(&self) -> AppResult<()> {
        if !(-6..=4).contains(&self.audio_normalization_offset_db) {
            bail!(AppError::Custom(format!(
                "audio_normalization_offset_db must be between -6 and 4, got {}",
                self.audio_normalization_offset_db
            )));
        }
        Ok(())
    }

    /// 实际响度目标。配置保存时会拒绝越界值；这里仍 clamp，保护旧数据库或手工构造值。
    pub fn effective_audio_target_lufs(&self) -> f64 {
        -16.0 + f64::from(self.audio_normalization_offset_db.clamp(-6, 4))
    }

    pub fn normalize_segment_limits(&mut self) {
        if self
            .segment_time
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            self.segment_time = None;
        }
    }

    pub fn load<P: AsRef<Path>>(path: P) -> AppResult<Self> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path)
            .change_context(AppError::Unknown)
            .attach_with(|| format!("read config {}", path.display()))?;
        let extension = path.extension().and_then(|ext| ext.to_str());
        let mut config: Config = match extension {
            Some("toml") => toml::from_str(&contents)
                .change_context(AppError::Unknown)
                .attach_with(|| format!("parse toml config {}", path.display()))?,
            Some("yaml") | Some("yml") => serde_yaml::from_str(&contents)
                .change_context(AppError::Unknown)
                .attach_with(|| format!("parse yaml config {}", path.display()))?,
            _ => bail!(AppError::Custom(format!(
                "unsupported config file extension: {}",
                path.display()
            ))),
        };
        config.normalize_segment_limits();
        config.validate_segment_limits()?;
        Ok(config)
    }

    /// 从指定路径加载配置文件，如果不存在则创建默认配置
    pub fn load_or_create<P: AsRef<Path>>(path: P) -> AppResult<Self> {
        Self::load(path)
    }
}

#[cfg(test)]
mod timestamp_repair_config_tests {
    use super::Config;

    #[test]
    fn timestamp_repair_defaults_to_none_and_treated_as_on() {
        // 空配置（不含 timestamp_repair 字段）应能反序列化，字段为 None
        let cfg: Config = serde_yaml::from_str("{}").expect("empty config should parse");
        assert_eq!(cfg.timestamp_repair, None);
        // 约定：None 视为开
        assert!(cfg.timestamp_repair.unwrap_or(true));
    }

    #[test]
    fn timestamp_repair_can_be_disabled() {
        let cfg: Config = serde_yaml::from_str("timestamp_repair: false").expect("parse");
        assert_eq!(cfg.timestamp_repair, Some(false));
    }
}

#[cfg(test)]
mod audio_normalization_config_tests {
    use super::{Config, ConfigPatch};
    use struct_patch::Patch;

    #[test]
    fn defaults_are_backward_compatible() {
        let config: Config = serde_yaml::from_str("{}").unwrap();
        assert!(!config.audio_normalization_enabled);
        assert_eq!(config.audio_normalization_offset_db, 0);
        assert_eq!(config.effective_audio_target_lufs(), -16.0);
    }

    #[test]
    fn validates_fader_bounds() {
        for value in [-6, 0, 4] {
            let config: Config = serde_yaml::from_str(&format!(
                "audio_normalization_enabled: true\naudio_normalization_offset_db: {value}"
            ))
            .unwrap();
            assert!(config.validate_segment_limits().is_ok());
        }
        for value in [-7, 5] {
            let config: Config =
                serde_yaml::from_str(&format!("audio_normalization_offset_db: {value}")).unwrap();
            assert!(config.validate_segment_limits().is_err());
        }
    }

    #[test]
    fn patch_can_override_audio_settings() {
        let mut config = Config::default();
        let patch: ConfigPatch = serde_json::from_str(
            r#"{"audio_normalization_enabled":true,"audio_normalization_offset_db":2}"#,
        )
        .unwrap();
        config.apply(patch);
        assert!(config.audio_normalization_enabled);
        assert_eq!(config.audio_normalization_offset_db, 2);
    }
}

#[cfg(test)]
mod recoverable_short_segment_config_tests {
    use super::Config;

    #[test]
    fn preservation_is_opt_in_and_matches_the_switch_default() {
        let default: Config = serde_yaml::from_str("{}").expect("default config");
        assert!(!default.preserve_recoverable_short_segments);
        assert_eq!(
            serde_json::to_value(&default).expect("serialize config")["preserve_recoverable_short_segments"],
            false
        );

        let enabled: Config =
            serde_yaml::from_str("preserve_recoverable_short_segments: true").expect("config");
        assert!(enabled.preserve_recoverable_short_segments);
    }
}

#[cfg(test)]
mod upload_rate_gate_config_tests {
    use super::Config;

    #[test]
    fn safe_upload_gate_defaults_are_enabled() {
        let config: Config = serde_yaml::from_str("{}").expect("default config");
        assert!(config.upload_rate_gate_enabled);
        assert_eq!(config.upload_min_request_interval_secs, 2);
        assert_eq!(config.upload_601_initial_cooldown_secs, 60);
        assert_eq!(config.upload_601_max_cooldown_secs, 1800);
        assert_eq!(config.recoverable_short_segment_mode, "merge_or_defer");
        assert_eq!(config.recoverable_short_batch_max_files, 60);
    }
}

#[cfg(test)]
mod route_health_config_tests {
    use super::Config;

    #[test]
    fn route_health_is_opt_in_and_matches_the_switch_default() {
        let default: Config = serde_yaml::from_str("{}").expect("default config");
        assert!(!default.route_health_enabled);
        assert_eq!(
            serde_json::to_value(&default).expect("serialize config")["route_health_enabled"],
            false
        );

        let enabled: Config = serde_yaml::from_str("route_health_enabled: true").expect("config");
        assert!(enabled.route_health_enabled);
    }

    #[test]
    fn stall_timeout_defaults_to_the_downloader_builtin() {
        // 缺省不下发，由 biliup 侧沿用 30 秒——回滚安全。
        let default: Config = serde_yaml::from_str("{}").expect("default config");
        assert_eq!(default.stream_stall_timeout_secs, None);

        let tuned: Config =
            serde_yaml::from_str("stream_stall_timeout_secs: 8").expect("tuned config");
        assert_eq!(tuned.stream_stall_timeout_secs, Some(8));
    }
}

#[cfg(test)]
mod douyin_candidate_config_tests {
    use super::Config;

    #[test]
    fn candidate_fallback_options_deserialize_independently() {
        let config: Config = serde_yaml::from_str(
            r#"
douyin_route_failover: false
douyin_protocol_fallback: false
douyin_quality_fallback: true
douyin_min_fallback_quality: sd
"#,
        )
        .expect("douyin candidate config");
        assert_eq!(config.douyin_route_failover, Some(false));
        assert_eq!(config.douyin_protocol_fallback, Some(false));
        assert_eq!(config.douyin_quality_fallback, Some(true));
        assert_eq!(config.douyin_min_fallback_quality.as_deref(), Some("sd"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_missing_file_size_uses_default() {
        let config: Config = serde_json::from_str(r#"{}"#).unwrap();

        assert_eq!(config.file_size, default_file_size());
        assert_eq!(config.segment_time, None);
        assert!(config.validate_segment_limits().is_ok());
    }

    #[test]
    fn deserialize_null_file_size_keeps_none() {
        let config: Config =
            serde_json::from_str(r#"{"file_size": null, "segment_time": "01:00:00"}"#).unwrap();

        assert_eq!(config.file_size, None);
        assert_eq!(config.segment_time, Some("01:00:00".to_string()));
        assert!(config.validate_segment_limits().is_ok());
    }

    #[test]
    fn size_or_time_segment_limit_is_valid() {
        let mut size_only = Config {
            file_size: Some(1024),
            segment_time: None,
            ..Config::default()
        };
        assert!(size_only.validate_segment_limits().is_ok());

        let mut time_only = Config {
            file_size: None,
            segment_time: Some("01:00:00".to_string()),
            ..Config::default()
        };
        assert!(time_only.validate_segment_limits().is_ok());

        time_only.segment_time = Some("".to_string());
        time_only.normalize_segment_limits();
        assert_eq!(time_only.segment_time, None);
        assert!(time_only.validate_segment_limits().is_ok());

        size_only.segment_time = Some("00:30:00".to_string());
        assert!(size_only.validate_segment_limits().is_ok());
    }

    #[test]
    fn empty_size_and_time_disables_segmentation() {
        let mut config = Config {
            file_size: None,
            segment_time: Some("".to_string()),
            ..Config::default()
        };

        config.normalize_segment_limits();

        assert_eq!(config.file_size, None);
        assert_eq!(config.segment_time, None);
        assert!(config.validate_segment_limits().is_ok());
    }

    #[test]
    fn config_patch_can_clear_file_size() {
        let mut config = Config::default();
        let patch: ConfigPatch =
            serde_json::from_str(r#"{"file_size": null, "segment_time": "01:00:00"}"#).unwrap();

        config.apply(patch);

        assert_eq!(config.file_size, None);
        assert_eq!(config.segment_time, Some("01:00:00".to_string()));
        assert!(config.validate_segment_limits().is_ok());
    }

    #[test]
    fn config_patch_can_explicitly_disable_resilience_switches() {
        let mut config = Config {
            preserve_recoverable_short_segments: true,
            route_health_enabled: true,
            ..Config::default()
        };

        config.apply(serde_json::from_str::<ConfigPatch>(r#"{}"#).unwrap());
        assert!(config.preserve_recoverable_short_segments);
        assert!(config.route_health_enabled);

        let patch: ConfigPatch = serde_json::from_str(
            r#"{
                "preserve_recoverable_short_segments": false,
                "route_health_enabled": false
            }"#,
        )
        .unwrap();
        config.apply(patch);

        assert!(!config.preserve_recoverable_short_segments);
        assert!(!config.route_health_enabled);
    }

    /// 只对确认被上游掐断的房间下调阈值，其余房间保持 30 秒——
    /// per-streamer 覆写就是这个灰度的载体。
    #[test]
    fn stall_timeout_can_be_lowered_per_streamer() {
        let mut config = Config::default();
        assert_eq!(config.stream_stall_timeout_secs, None);

        config.apply(serde_json::from_str::<ConfigPatch>(r#"{}"#).unwrap());
        assert_eq!(config.stream_stall_timeout_secs, None);

        config.apply(
            serde_json::from_str::<ConfigPatch>(r#"{"stream_stall_timeout_secs": 8}"#).unwrap(),
        );
        assert_eq!(config.stream_stall_timeout_secs, Some(8));
    }
}
