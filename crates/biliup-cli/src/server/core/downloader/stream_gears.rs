use crate::server::common::construct_headers;
use crate::server::common::util::parse_time;
use crate::server::core::downloader::{DownloadConfig, DownloadStatus, SegmentEvent, SegmentInfo};
use crate::server::errors::{AppError, AppResult};
use biliup::client::StatelessClient;
use biliup::downloader::error::Error as DownloadError;
use biliup::downloader::flv_parser::header;
use biliup::downloader::httpflv::Connection;
use biliup::downloader::util::{
    LifecycleFile, SegmentCloseHandle, SegmentCloseReason, Segmentable,
};
use biliup::downloader::{hls, httpflv};
use error_stack::{ResultExt, bail};
use nom::Err;
use std::path::PathBuf;
use std::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// Stream-gears下载器实现
/// 使用stream-gears库进行直播流下载
pub struct StreamGears {
    /// 代理设置（可选）
    proxy: Option<String>,

    token: RwLock<CancellationToken>,
}

impl StreamGears {
    /// 创建新的Stream-gears下载器实例
    ///
    /// # 参数
    /// * `url` - 流URL
    /// * `header_map` - HTTP请求头
    /// * `file_name` - 输出文件名
    /// * `segment` - 分段配置
    /// * `proxy` - 代理设置（可选）
    pub fn new(proxy: Option<String>) -> Self {
        Self {
            proxy,
            token: RwLock::new(CancellationToken::new()),
        }
    }

    async fn start_download<'a>(
        &self,
        mut callback: Box<dyn FnMut(SegmentEvent) + Send + Sync + 'a>,
        download_config: DownloadConfig,
        close_handle: SegmentCloseHandle,
    ) -> AppResult<DownloadStatus> {
        let url = download_config.url.clone();
        let attempt_id = download_config
            .attempt_id
            .clone()
            .unwrap_or_else(|| "untracked".to_string());
        let stream_host = url::Url::parse(&url)
            .ok()
            .and_then(|parsed| parsed.host_str().map(ToString::to_string))
            .unwrap_or_else(|| "unknown".to_string());
        let requested_protocol = if download_config.suffix.eq_ignore_ascii_case("m3u8")
            || download_config.suffix.eq_ignore_ascii_case("ts")
        {
            "hls"
        } else {
            "flv"
        };
        let file_name = download_config.recorder.filename_template();
        let headers_in = construct_headers(&download_config.headers).map_err(AppError::Custom)?;
        let proxy = self.proxy.clone();
        let segment = Segmentable::new(
            download_config.segment_time.as_deref().map(parse_time),
            download_config.file_size,
        );

        // 创建HTTP客户端
        let client = StatelessClient::new(headers_in, proxy.as_deref());
        // 获取可重试的响应
        let response = match client.retryable(&url).await {
            Ok(response) => response,
            Err(err) => {
                let status = classify_reqwest_error(err);
                warn!(
                    attempt_id,
                    stream_host,
                    protocol = requested_protocol,
                    quality = download_config.quality.as_deref().unwrap_or("unknown"),
                    result = ?status,
                    "download stream request failed"
                );
                return Ok(status);
            }
        };
        // 创建连接
        let mut connection = Connection::new(response);
        // 读取帧头
        let bytes = match connection.read_frame(9).await {
            Ok(bytes) => bytes,
            Err(err) => {
                let diagnostics = connection.diagnostics();
                let status = classify_download_error(err);
                warn!(
                    attempt_id,
                    http_status = diagnostics.http_status,
                    content_encoding = diagnostics.content_encoding.as_deref().unwrap_or("none"),
                    transfer_encoding = diagnostics.transfer_encoding.as_deref().unwrap_or("none"),
                    received_bytes = diagnostics.received_bytes,
                    connected_for = ?diagnostics.connected_for,
                    buffered = diagnostics.buffered,
                    stream_host,
                    protocol = requested_protocol,
                    quality = download_config.quality.as_deref().unwrap_or("unknown"),
                    result = ?status,
                    "download stream header read failed"
                );
                return Ok(status);
            }
        };
        // let mut i = 0;
        // let mut prev_file_path = None;
        // 创建分段回调钩子
        let hook = {
            let mut i = 0;
            let attempt_id = attempt_id.clone();
            move |s: &str, close_reason| {
                let file_path = PathBuf::from(s);

                let event = SegmentInfo {
                    prev_file_path: file_path,
                    danmaku_file_path: None,
                    next_file_path: None,
                    segment_index: i,
                    close_reason,
                    attempt_id: Some(attempt_id.clone()),
                    recovery_source_paths: Vec::new(),
                };
                callback(SegmentEvent::Segment(event));

                i += 1;
            }
        };
        // 解析流头部，判断流类型
        match header(&bytes) {
            Ok((_i, header)) => {
                debug!("header: {header:#?}");
                info!(
                    attempt_id,
                    stream_host = %stream_host,
                    protocol = "flv",
                    quality = download_config.quality.as_deref().unwrap_or("unknown"),
                    "starting stream download"
                );
                // FLV流下载
                let file = LifecycleFile::with_hook_and_close_handle(
                    &file_name,
                    "flv",
                    close_handle,
                    hook,
                );
                let log_context = httpflv::HttpFlvLogContext {
                    attempt_id: attempt_id.clone(),
                    stream_host: stream_host.clone(),
                    protocol: "flv".to_string(),
                    quality: download_config.quality.clone(),
                };
                match httpflv::download_with_context(connection, file, segment.clone(), log_context)
                    .await
                {
                    Ok(()) => Ok(DownloadStatus::StreamEnded),
                    Err(err) => {
                        let status = classify_download_error(err);
                        warn!(
                            attempt_id,
                            stream_host,
                            protocol = "flv",
                            quality = download_config.quality.as_deref().unwrap_or("unknown"),
                            result = ?status,
                            "download stream ended with classified error"
                        );
                        Ok(status)
                    }
                }
            }
            Err(Err::Incomplete(needed)) => {
                error!("needed: {needed:?}");
                Ok(DownloadStatus::IncompleteFrame {
                    buffered: bytes.len(),
                })
            }
            Err(e) => {
                error!("{e}");
                // HLS流下载
                info!(
                    attempt_id,
                    stream_host,
                    protocol = "hls",
                    quality = download_config.quality.as_deref().unwrap_or("unknown"),
                    "starting stream download"
                );
                let file =
                    LifecycleFile::with_hook_and_close_handle(&file_name, "ts", close_handle, hook);
                hls::download(&url, &client, file, segment.clone())
                    .await
                    .change_context(AppError::Unknown)?;
                Ok(DownloadStatus::StreamEnded)
            }
        }
    }
}

fn classify_reqwest_error(err: reqwest::Error) -> DownloadStatus {
    let err = err.without_url();
    if let Some(status) = err.status() {
        DownloadStatus::HttpStatus {
            status: status.as_u16(),
        }
    } else if err.is_timeout() {
        DownloadStatus::ReadTimeout { buffered: 0 }
    } else {
        DownloadStatus::Error(err.to_string())
    }
}

fn classify_download_error(err: DownloadError) -> DownloadStatus {
    match err {
        DownloadError::HttpFlvIncompleteFrame { buffered } => {
            DownloadStatus::IncompleteFrame { buffered }
        }
        DownloadError::HttpFlvReadTimeout { buffered } => DownloadStatus::ReadTimeout { buffered },
        DownloadError::ReqwestError(err) => classify_reqwest_error(err),
        other => DownloadStatus::Error(other.to_string()),
    }
}

impl StreamGears {
    /// 开始下载流
    ///
    /// # 参数
    /// * `callback` - 分段完成时的回调函数
    pub(crate) async fn download<'a>(
        &self,
        callback: Box<dyn FnMut(SegmentEvent) + Send + Sync + 'a>,
        download_config: DownloadConfig,
    ) -> AppResult<DownloadStatus> {
        *self.token.write().unwrap() = CancellationToken::new();
        let token = self.token.read().unwrap().clone();
        let close_handle = SegmentCloseHandle::default();
        tokio::select! {
            _ = token.cancelled() => {
                close_handle.set(SegmentCloseReason::Cancelled);
                bail!(AppError::Custom("StreamGears token cancelled".into()))
            }
            res = self.start_download(callback, download_config, close_handle.clone()) => {res}
        }
    }

    /// 停止下载
    pub(crate) async fn stop(&self) -> AppResult<()> {
        // 仅发出取消信号并更新状态
        // 如果底层下载函数不支持取消，这里不能真正中断正在进行的下载
        self.token.read().unwrap().cancel();
        Ok(())
    }
}
