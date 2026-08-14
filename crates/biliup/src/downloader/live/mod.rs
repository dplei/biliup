use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

mod acfun;
mod afreecatv;
mod bigo;
mod bilibili;
mod cc;
mod douyin;
mod douyu;
mod general;
mod huya;
mod inke;
mod kilakila;
mod kuaishou;
mod missevan;
mod niconico;
mod picarto;
mod ttinglive;
mod twitcasting;
mod twitch;
mod wbi;
mod youtube;
mod yy;

pub use acfun::Acfun;
pub use afreecatv::AfreecaTV;
pub use bigo::Bigo;
pub use bilibili::Bilibili;
pub use cc::CC;
pub use douyin::Douyin;
pub use douyu::Douyu;
pub use general::General;
pub use huya::Huya;
pub use inke::Inke;
pub use kilakila::Kilakila;
pub use kuaishou::Kuaishou;
pub use missevan::Missevan;
pub use niconico::Niconico;
pub use picarto::Picarto;
pub use ttinglive::TTingLive;
pub use twitcasting::Twitcasting;
pub use twitch::{Twitch, TwitchVideos};
pub use youtube::Youtube;
pub use yy::YY;

pub type LiveResult<T> = Result<T, LiveError>;

#[derive(Debug, thiserror::Error)]
pub enum LiveError {
    #[error("{0}")]
    Custom(String),
    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Header(#[from] reqwest::header::InvalidHeaderValue),
}

impl LiveError {
    pub fn custom(message: impl Into<String>) -> Self {
        Self::Custom(message.into())
    }
}

#[async_trait]
pub trait LivePlugin: Send + Sync {
    fn name(&self) -> &'static str;
    fn matches(&self, url: &str) -> bool;
    async fn check_stream(&self, request: LiveRequest) -> LiveResult<LiveStatus>;
}

#[derive(Debug, Clone)]
pub struct LiveRequest {
    pub client: Client,
    pub url: String,
    pub name: String,
    pub options: LiveOptions,
    pub credentials: LiveCredentials,
}

#[derive(Debug, Clone, Default)]
pub struct LiveOptions {
    pub bilibili: BilibiliOptions,
    pub cc: CcOptions,
    pub douyin: DouyinOptions,
    pub douyu: DouyuOptions,
    pub huya: HuyaOptions,
    pub kilakila: KilakilaOptions,
    pub kuaishou: KuaishouOptions,
    pub twitcasting: TwitcastingOptions,
    pub twitch: TwitchOptions,
    pub youtube: YoutubeOptions,
}

#[derive(Debug, Clone)]
pub struct BilibiliOptions {
    pub qn: u32,
    pub protocol: String,
    pub cdn: Vec<String>,
    pub cdn_fallback: bool,
    pub hls_transcode_timeout: u64,
    pub anonymous_origin: bool,
    pub live_api: Option<String>,
    pub fallback_api: Option<String>,
    pub danmaku: bool,
    pub danmaku_raw: bool,
    pub danmaku_detail: bool,
}

impl Default for BilibiliOptions {
    fn default() -> Self {
        Self {
            qn: 25000,
            protocol: "stream".to_string(),
            cdn: Vec::new(),
            cdn_fallback: false,
            hls_transcode_timeout: 60,
            anonymous_origin: false,
            live_api: None,
            fallback_api: None,
            danmaku: false,
            danmaku_raw: false,
            danmaku_detail: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CcOptions {
    pub protocol: String,
}

impl Default for CcOptions {
    fn default() -> Self {
        Self {
            protocol: "hls".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DouyinOptions {
    pub quality: String,
    pub protocol: String,
    pub double_screen: bool,
    pub true_origin: bool,
    pub danmaku: bool,
    pub route_failover: bool,
    pub protocol_fallback: bool,
    pub quality_fallback: bool,
    pub min_fallback_quality: String,
}

impl Default for DouyinOptions {
    fn default() -> Self {
        Self {
            quality: "origin".to_string(),
            protocol: "flv".to_string(),
            double_screen: false,
            true_origin: false,
            danmaku: false,
            // 自动切线先按主播灰度；满足阶段 6 的观测门槛后再考虑默认开启。
            route_failover: false,
            protocol_fallback: true,
            quality_fallback: false,
            min_fallback_quality: "hd".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DouyuOptions {
    pub cdn: String,
    pub rate: u32,
    pub disable_interactive_game: bool,
    pub danmaku: bool,
}

impl Default for DouyuOptions {
    fn default() -> Self {
        Self {
            cdn: "hw-h5".to_string(),
            rate: 0,
            disable_interactive_game: false,
            danmaku: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HuyaOptions {
    pub cdn: String,
    pub max_ratio: u32,
    pub protocol: String,
    pub imgplus: bool,
    pub codec: String,
    pub danmaku: bool,
}

impl Default for HuyaOptions {
    fn default() -> Self {
        Self {
            cdn: String::new(),
            max_ratio: 0,
            protocol: "Flv".to_string(),
            imgplus: true,
            codec: "264".to_string(),
            danmaku: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct KilakilaOptions {
    pub protocol: String,
}

impl Default for KilakilaOptions {
    fn default() -> Self {
        Self {
            protocol: "hls".to_string(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct KuaishouOptions {
    pub cookie: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct TwitcastingOptions {
    pub password: Option<String>,
    pub quality: Option<String>,
    pub danmaku: bool,
}

#[derive(Debug, Clone)]
pub struct TwitchOptions {
    pub danmaku: bool,
    pub disable_ads: bool,
}

impl Default for TwitchOptions {
    fn default() -> Self {
        Self {
            danmaku: false,
            disable_ads: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct YoutubeOptions {
    pub enable_download_live: bool,
    pub enable_download_playback: bool,
    pub after_date: Option<String>,
    pub before_date: Option<String>,
    pub prefer_vcodec: Option<String>,
    pub prefer_acodec: Option<String>,
    pub max_resolution: Option<u32>,
    pub max_videosize: Option<String>,
    pub danmaku: bool,
}

impl Default for YoutubeOptions {
    fn default() -> Self {
        Self {
            enable_download_live: true,
            enable_download_playback: true,
            after_date: None,
            before_date: None,
            prefer_vcodec: None,
            prefer_acodec: None,
            max_resolution: None,
            max_videosize: None,
            danmaku: false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct LiveCredentials {
    pub bilibili_cookie: Option<String>,
    pub bilibili_cookie_file: Option<PathBuf>,
    pub douyin_cookie: Option<String>,
    pub twitcasting_cookie: Option<String>,
    pub twitch_cookie: Option<String>,
    pub youtube_cookie: Option<PathBuf>,
    pub afreecatv_username: Option<String>,
    pub afreecatv_password: Option<String>,
    pub niconico_email: Option<String>,
    pub niconico_password: Option<String>,
    pub niconico_user_session: Option<String>,
    pub niconico_purge_credentials: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LiveStatus {
    Live { stream: Box<LiveStream> },
    Offline,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveStream {
    pub name: String,
    pub url: String,
    pub title: String,
    pub date: DateTime<Utc>,
    pub live_cover_url: String,
    pub raw_stream_url: String,
    pub platform: String,
    pub stream_headers: HashMap<String, String>,
    pub suffix: String,
    pub danmaku: Option<DanmakuSource>,
    pub downloader_hint: DownloaderHint,
    pub runtime_options: Option<RuntimeOptions>,
    /// 平台响应真实提供的可选下载路线。`raw_stream_url` 仍是兼容主选。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stream_candidates: Vec<StreamCandidate>,
    /// 实际选中的画质代码（origin/uhd/...）。仅抖音填充，其它平台为 None。
    #[serde(default)]
    pub recording_quality: Option<String>,
    /// 一次选流/下载尝试的脱敏关联 ID；不包含 URL、Cookie 或签名参数。
    #[serde(default)]
    pub attempt_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamProtocol {
    Flv,
    Hls,
}

impl std::fmt::Display for StreamProtocol {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Flv => formatter.write_str("flv"),
            Self::Hls => formatter.write_str("hls"),
        }
    }
}

/// 一条由平台响应提供并已签名的真实下载路线。
///
/// Debug 刻意隐藏完整 URL，避免 `LiveStream` 的普通诊断日志泄露签名 query。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamCandidate {
    pub url: String,
    pub host: Option<String>,
    pub protocol: StreamProtocol,
    pub quality: Option<String>,
    pub codec: Option<String>,
    pub resolution: Option<String>,
    pub priority: u16,
}

impl std::fmt::Debug for StreamCandidate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StreamCandidate")
            .field("url", &"<redacted>")
            .field("host", &self.host)
            .field("protocol", &self.protocol)
            .field("quality", &self.quality)
            .field("codec", &self.codec)
            .field("resolution", &self.resolution)
            .field("priority", &self.priority)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DanmakuSource {
    pub platform: String,
    pub url: String,
    pub room_id: Option<String>,
    pub cookie: Option<String>,
    pub raw: bool,
    pub detail: bool,
    pub extra: HashMap<String, String>,
    pub movie_id: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RuntimeOptions {
    Streamlink(StreamlinkOptions),
    YtDlp(YtDlpOptions),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamlinkOptions {
    pub url: Option<String>,
    pub platform: StreamlinkPlatform,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StreamlinkPlatform {
    Bilibili,
    Twitch {
        disable_ads: bool,
        auth_token: Option<String>,
    },
    Niconico {
        email: Option<String>,
        password: Option<String>,
        user_session: Option<String>,
        purge_credentials: Option<String>,
    },
    Generic,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum YtDlpBackend {
    YtDlp,
    YtArchive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct YtDlpOptions {
    pub webpage_url: String,
    pub download_url: Option<String>,
    pub backend: YtDlpBackend,
    pub is_live: bool,
    pub use_live_cover: bool,
    pub cover_url: Option<String>,
    pub cookies_file: Option<PathBuf>,
    pub prefer_vcodec: Option<String>,
    pub prefer_acodec: Option<String>,
    pub max_filesize: Option<String>,
    pub max_height: Option<u32>,
    pub download_archive: Option<PathBuf>,
    pub extra_ytdlp_args: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum DownloaderHint {
    StreamGears,
    Ffmpeg,
    Streamlink,
    YtDlp,
}

pub fn builtin_plugins() -> Vec<Arc<dyn LivePlugin + Send + Sync>> {
    vec![
        Arc::new(Acfun::new()),
        Arc::new(AfreecaTV::new()),
        Arc::new(Bigo::new()),
        Arc::new(Bilibili::new()),
        Arc::new(CC::new()),
        Arc::new(Douyin::new()),
        Arc::new(Douyu::new()),
        Arc::new(Huya::new()),
        Arc::new(Inke::new()),
        Arc::new(Kilakila::new()),
        Arc::new(Kuaishou::new()),
        Arc::new(Missevan::new()),
        Arc::new(Niconico::new()),
        Arc::new(Picarto::new()),
        Arc::new(TTingLive::new()),
        Arc::new(Twitcasting::new()),
        Arc::new(TwitchVideos::new()),
        Arc::new(Twitch::new()),
        Arc::new(Youtube::new()),
        Arc::new(YY::new()),
        Arc::new(General::new()),
    ]
}

#[cfg(test)]
mod stream_candidate_compatibility_tests {
    use super::{DouyinOptions, DownloaderHint, LiveStream};
    use chrono::Utc;
    use std::collections::HashMap;

    fn non_douyin_stream() -> LiveStream {
        LiveStream {
            name: "fixture".to_string(),
            url: "https://example.com/room".to_string(),
            title: "fixture".to_string(),
            date: Utc::now(),
            live_cover_url: String::new(),
            raw_stream_url: "https://cdn.example/live.flv".to_string(),
            platform: "general".to_string(),
            stream_headers: HashMap::new(),
            suffix: "flv".to_string(),
            danmaku: None,
            downloader_hint: DownloaderHint::StreamGears,
            runtime_options: None,
            stream_candidates: Vec::new(),
            recording_quality: None,
            attempt_id: None,
        }
    }

    #[test]
    fn old_non_douyin_payload_defaults_candidates_to_empty() {
        let mut value = serde_json::to_value(non_douyin_stream()).expect("serialize fixture");
        assert!(value.get("stream_candidates").is_none());
        value
            .as_object_mut()
            .expect("fixture object")
            .remove("stream_candidates");
        let decoded: LiveStream = serde_json::from_value(value).expect("deserialize old payload");
        assert!(decoded.stream_candidates.is_empty());
        assert_eq!(decoded.raw_stream_url, "https://cdn.example/live.flv");
    }

    #[test]
    fn douyin_candidate_defaults_match_phase_six_rollout_policy() {
        let options = DouyinOptions::default();
        assert!(!options.route_failover);
        assert!(options.protocol_fallback);
        assert!(!options.quality_fallback);
        assert_eq!(options.min_fallback_quality, "hd");
    }
}

pub fn media_ext_from_url(input: &str) -> Option<String> {
    fn clean_ext(val: &str) -> Option<String> {
        let ext = val
            .trim()
            .trim_matches(|c: char| !c.is_ascii_alphanumeric())
            .to_ascii_lowercase();
        matches!(ext.as_str(), "flv" | "ts" | "m3u8" | "mp4" | "m4s").then_some(ext)
    }

    if let Ok(url) = url::Url::parse(input) {
        if let Some(seg) = url.path_segments().and_then(|mut s| s.next_back())
            && let Some((_, ext)) = seg.rsplit_once('.')
            && let Some(ext) = clean_ext(ext)
        {
            return Some(ext);
        }

        let keys = ["format", "type", "ext", "filetype", "fmt"];
        if let Some(ext) = url.query_pairs().find_map(|(k, v)| {
            if keys.iter().any(|t| k.as_ref().eq_ignore_ascii_case(t)) {
                clean_ext(&v)
            } else {
                None
            }
        }) {
            return Some(ext);
        }

        return None;
    }

    let before_q = input.split('?').next().unwrap_or(input);
    if let Some((_, ext)) = before_q.rsplit_once('.') {
        return clean_ext(ext);
    }

    None
}
