pub mod cover_downloader;
/// FFmpeg下载器实现
pub mod ffmpeg_downloader;
/// Stream-gears下载器实现
pub mod stream_gears;
pub mod streamlink;
pub mod ytdlp;

use crate::server::common::util::Recorder;
use crate::server::core::downloader::ffmpeg_downloader::FfmpegDownloader;
use crate::server::core::downloader::stream_gears::StreamGears;
use crate::server::core::downloader::streamlink::Streamlink;
use crate::server::core::downloader::ytdlp::YouTubeDownloader;
use crate::server::errors::{AppError, AppResult};
use async_trait::async_trait;
use biliup::downloader::live::StreamCandidate;
use biliup::downloader::util::SegmentCloseReason;
use danmaku_client::{DanmakuRecorder, RecorderConfig, RecorderHandle};
use error_stack::Report;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

/// 一次重连所继承的缺口线索。`silent_measured` 区分实测与估算，估算不冒充测量。
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct ReconnectContext {
    pub gap_ms: u64,
    pub silent_ms: u64,
    pub silent_measured: bool,
}

/// 下载器配置
/// 包含下载过程中需要的各种参数和设置
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct DownloadConfig {
    /// 流 URL
    pub(crate) url: String,
    /// 平台响应真实提供的下载路线；上层熔断器把本次选中的候选写入 `url`。
    pub(crate) stream_candidates: Vec<StreamCandidate>,
    /// 分段时长 (格式: "HH:MM:SS")
    pub segment_time: Option<String>,

    /// 分段文件大小限制 (字节)
    pub file_size: Option<u64>,

    /// HTTP请求头
    pub headers: HashMap<String, String>,

    /// 录制器实例
    pub recorder: Recorder,

    /// 输出目录路径
    pub output_dir: PathBuf,

    pub suffix: String,

    /// 录制身份：房间与场次由调用方给出，事件层不从文件名反推。
    #[serde(default)]
    pub owner: crate::observe::RecordingIdentity,

    /// 上一次连接的缺口；有值表示本次是重连，连接真正建立时才据此记录恢复。
    #[serde(default)]
    pub reconnect: Option<ReconnectContext>,

    /// 脱敏的单次选流/下载关联 ID。
    pub attempt_id: Option<String>,
    pub quality: Option<String>,

    /// 码流停顿看门狗阈值（秒）：连续这么久没收到任何字节就判连接已死。
    /// `None` 表示沿用 `biliup` 侧的 30 秒默认值。
    #[serde(default)]
    pub stall_timeout_secs: Option<u64>,
}

impl DownloadConfig {
    /// 生成输出文件名
    ///
    /// # 返回
    /// 返回完整的输出文件路径
    fn generate_output_filename(&self, suffix: &str) -> PathBuf {
        self.output_dir.join(self.recorder.generate_path(suffix))
    }
}

/// 下载器类型枚举
/// 定义支持的各种下载器类型
#[derive(PartialEq, Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DownloaderType {
    /// Ytarchive下载器
    Ytarchive,
    /// 同步下载器
    #[serde(rename = "sync-downloader")]
    SyncDownloader,
    /// 使用stream-gears
    #[serde(rename = "stream-gears")]
    StreamGears,
    /// FFmpeg下载器
    Ffmpeg,
    /// FFmpeg外部分段
    FfmpegExternal,
    /// FFmpeg内部分段
    FfmpegInternal,
    /// Streamlink下载器
    Streamlink,
    /// yt-dlp下载器
    YtDlp,
}

/// 实际的下载器枚举（包含实例）
pub enum DownloaderRuntime {
    Ffmpeg(FfmpegDownloader),
    StreamGears(StreamGears),
    StreamLink(Streamlink),
    YtDlp(YouTubeDownloader),
}

impl DownloaderRuntime {
    /// 从配置创建
    pub fn from_type(downloader_type: DownloaderType) -> Self {
        match downloader_type {
            DownloaderType::Ffmpeg => Self::Ffmpeg(FfmpegDownloader::new(
                Vec::new(),
                DownloaderType::FfmpegExternal,
            )),
            _ => Self::StreamGears(StreamGears::new(None)),
            // ...
        }
    }

    pub async fn download<'a>(
        &self,
        callback: Box<dyn FnMut(SegmentEvent) + Send + Sync + 'a>,
        download_config: DownloadConfig,
    ) -> AppResult<DownloadStatus> {
        match self {
            Self::Ffmpeg(d) => d.download(callback, download_config).await,
            Self::StreamGears(d) => d.download(callback, download_config).await,
            DownloaderRuntime::StreamLink(d) => d.download(callback, download_config).await,
            Self::YtDlp(d) => d.download(callback, download_config).await,
        }
    }

    /// 取走上一次 FLV 连接结束时记录的缺口线索。
    ///
    /// 只有 stream-gears 走 `httpflv` 自研解析、能测到静默时长；其余下载器交给外部进程，
    /// 拿不到这个口径，返回 `None` 让上层退回旧的「报错之后」记账。
    pub fn take_last_gap(&self) -> Option<StreamGapReport> {
        match self {
            Self::StreamGears(d) => d.take_last_gap(),
            _ => None,
        }
    }

    pub async fn stop(&self) -> AppResult<()> {
        match self {
            Self::Ffmpeg(d) => d.stop().await,
            Self::StreamGears(d) => d.stop().await,
            DownloaderRuntime::StreamLink(d) => d.stop().await,
            Self::YtDlp(d) => d.stop().await,
        }
    }
}

/// 一次 FLV 连接结束时测到的缺口线索。
///
/// `silent_for` 是「上游最后一个字节 → 连接判死」，也就是原先完全看不见的那一段：
/// 缺口的大头在这里，而不是在报错之后的重连里。
#[derive(Debug, Clone, Copy)]
pub struct StreamGapReport {
    pub silent_for: Duration,
    pub connected_for: Duration,
    pub stall_timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct SegmentInfo {
    /// 分段文件路径
    pub prev_file_path: PathBuf,
    pub danmaku_file_path: Option<PathBuf>,
    pub next_file_path: Option<PathBuf>,
    /// 分段序号
    pub segment_index: usize,
    pub close_reason: SegmentCloseReason,
    pub attempt_id: Option<String>,
    /// Stable identity assigned when the file was created, carried through close, enrollment and
    /// upload. Empty only for segments produced by a path that predates the identity.
    pub segment_id: Option<String>,
    /// 合并成功后保留的原始短片；上传 durable 后才允许交给后处理清理。
    pub recovery_source_paths: Vec<PathBuf>,
    /// Durable v2 lifecycle identity. Upload code must consume this identity instead of
    /// allocating another session/order after the event has crossed an in-memory channel.
    pub enrollment: Option<SegmentEnrollment>,
    // /// 分段开始时间戳
    // start_time: std::time::SystemTime,
    // /// 分段结束时间戳
    // end_time: std::time::SystemTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentEnrollment {
    pub missing_id: i64,
    pub upload_session_id: i64,
    /// The enrollment transaction created the destination upload session together with this row.
    pub created_session: bool,
    pub segment_order: i64,
    pub normalized_file_path: PathBuf,
    pub total_bytes: u64,
    pub duplicate: bool,
    /// Identity carried from file creation, or the one already stored for a duplicate row.
    pub segment_id: Option<String>,
}

impl SegmentInfo {
    pub fn new(
        prev_file_path: PathBuf,
        danmaku_file_path: Option<PathBuf>,
        next_file_path: Option<PathBuf>,
        segment_index: usize,
    ) -> Self {
        Self {
            prev_file_path,
            danmaku_file_path,
            next_file_path,
            segment_index,
            close_reason: SegmentCloseReason::Unknown,
            attempt_id: None,
            segment_id: None,
            recovery_source_paths: Vec::new(),
            enrollment: None,
        }
    }
}

/// 分段事件
/// 当下载器完成一个分段时触发的事件
#[derive(Debug, Clone)]
pub enum SegmentEvent {
    Start {
        /// 分段文件路径
        next_file_path: PathBuf,
    },
    Segment(SegmentInfo),
}

/// 下载状态
/// 表示下载器当前的状态
#[derive(Debug, Clone, PartialEq)]
pub enum DownloadStatus {
    /// 正在下载
    Downloading,
    /// 正常分段（外部分段触发）
    SegmentCompleted,
    /// 直播流结束
    StreamEnded,
    /// 直播流在一帧未读完整时结束
    IncompleteFrame { buffered: usize },
    /// 直播流读取超时
    ReadTimeout { buffered: usize },
    /// 拉流 URL 返回 HTTP 错误状态
    HttpStatus { status: u16 },
    /// 用户主动取消下载。
    Cancelled,
    /// 错误
    Error(String),
}

impl DownloadStatus {
    /// 返回该终止状态是否应计入线路传输失败。
    ///
    /// `StreamEnded` 后直播间仍为 Live 时同样属于线路提前结束；调用方会在完成
    /// 直播状态检查后再使用本分类，因此正常下播不会污染线路健康状态。
    pub fn is_transport_failure(&self) -> bool {
        matches!(
            self,
            Self::StreamEnded
                | Self::IncompleteFrame { .. }
                | Self::ReadTimeout { .. }
                | Self::HttpStatus { .. }
                | Self::Error(_)
        )
    }
}

#[async_trait]
// 弹幕客户端 (需要根据实际情况实现)
pub trait DanmakuClient {
    /// Starts danmaku recording and manages lifecycle
    async fn download(&self) -> AppResult<()>;

    async fn stop(&self) -> AppResult<()>;
    /// 滚动保存（用于弹幕等）
    ///
    /// # 参数
    /// * `file_name` - 文件名
    ///
    /// 返回 true 表示本次滚动保存产生了可交给后处理的弹幕文件。
    fn rolling(&self, _file_name: &str) -> Result<bool, Box<dyn std::error::Error>> {
        Ok(false)
    }
}

pub struct RustDanmakuClient {
    config: RecorderConfig,
    handle: Mutex<Option<RecorderHandle>>,
}

impl RustDanmakuClient {
    pub fn new(config: RecorderConfig) -> Self {
        Self {
            config,
            handle: Mutex::new(None),
        }
    }
}

#[async_trait]
impl DanmakuClient for RustDanmakuClient {
    async fn download(&self) -> AppResult<()> {
        let mut handle = self.handle.lock().unwrap();
        if handle.is_some() {
            return Ok(());
        }

        let recorder = DanmakuRecorder::new(self.config.clone())
            .map_err(|e| Report::new(AppError::Custom(e.to_string())))?;
        *handle = Some(recorder.start());
        Ok(())
    }

    async fn stop(&self) -> AppResult<()> {
        let handle = self.handle.lock().unwrap().take();
        if let Some(handle) = handle {
            handle
                .stop()
                .await
                .map_err(|e| Report::new(AppError::Custom(e.to_string())))?;
        }
        Ok(())
    }

    fn rolling(&self, file_name: &str) -> Result<bool, Box<dyn std::error::Error>> {
        let handle = self
            .handle
            .lock()
            .map_err(|_| "danmaku handle lock poisoned")?
            .clone();
        if let Some(handle) = handle {
            return tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current()
                    .block_on(handle.rolling(Some(PathBuf::from(file_name))))
            })
            .map_err(Into::into);
        }
        Ok(false)
    }
}

/// 解析时长字符串 "HH:MM:SS" 为秒数
///
/// # 参数
/// * `duration` - 时长字符串，格式为"HH:MM:SS"
///
/// # 返回
/// 返回总秒数
fn parse_duration(duration: &str) -> u64 {
    let parts: Vec<&str> = duration.split(':').collect();
    if parts.len() == 3 {
        let hours: u64 = parts[0].parse().unwrap_or(0);
        let minutes: u64 = parts[1].parse().unwrap_or(0);
        let seconds: u64 = parts[2].parse().unwrap_or(0);
        hours * 3600 + minutes * 60 + seconds
    } else {
        0
    }
}

// 使用示例
// #[tokio::main]
// async fn main() -> Result<(), Box<dyn std::error::Error>> {
//     let config = DownloadConfig {
//         format: "mp4".to_string(),
//         segment_time: Some("01:00:00".to_string()),
//         file_size: Some(2 * 1024 * 1024 * 1024), // 2GB
//         headers: HashMap::from([("User-Agent".to_string(), "Mozilla/5.0".to_string())]),
//         extra_args: vec![],
//         downloader_type: DownloaderType::FfmpegInternal,
//         filename_prefix: "stream".to_string(),
//     };
//
//     // 分段回调
//     let segment_callback = Arc::new(|event: SegmentEvent| {
//         println!("New segment: {:?}", event.file_path);
//         // 这里可以触发上传等后续处理
//     });
//
//     let downloader = FfmpegDownloader::new(
//         "http://example.com/stream.m3u8".to_string(),
//         config,
//         PathBuf::from("./downloads"),
//         Some(segment_callback),
//     );
//
//     // 检查流
//     // if downloader.check_stream().await? {
//     //     // 开始下载
//     //     let status = downloader.download().await?;
//     //     println!("Download completed with status: {:?}", status);
//     // }
//
//     Ok(())
// }
