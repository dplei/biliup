use crate::server::common::construct_headers;
use crate::server::common::util::parse_time;
use crate::server::core::downloader::{
    DownloadConfig, DownloadStatus, SegmentEvent, SegmentInfo, StreamGapReport,
};
use crate::server::errors::{AppError, AppResult};
use biliup::client::StatelessClient;
use biliup::downloader::error::Error as DownloadError;
use biliup::downloader::flv_parser::header;
use biliup::downloader::httpflv::Connection;
use biliup::downloader::util::{
    LifecycleFile, SegmentCloseHandle, SegmentCloseReason, Segmentable,
};
use biliup::downloader::{hls, httpflv};
use error_stack::ResultExt;
use nom::Err;
use std::path::PathBuf;
use std::sync::{Mutex, RwLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// Stream-gears下载器实现
/// 使用stream-gears库进行直播流下载
pub struct StreamGears {
    /// 代理设置（可选）
    proxy: Option<String>,

    token: RwLock<CancellationToken>,

    /// 上一次 FLV 连接结束时测到的缺口线索，供重连循环记账。
    last_gap: Mutex<Option<StreamGapReport>>,
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
            last_gap: Mutex::new(None),
        }
    }

    pub(crate) fn take_last_gap(&self) -> Option<StreamGapReport> {
        self.last_gap.lock().unwrap().take()
    }

    fn record_gap(&self, connection: &Connection) {
        let diagnostics = connection.diagnostics();
        *self.last_gap.lock().unwrap() = Some(StreamGapReport {
            silent_for: diagnostics.silent_for,
            connected_for: diagnostics.connected_for,
            stall_timeout: diagnostics.stall_timeout,
        });
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
        let candidate_count = download_config.stream_candidates.len();
        let file_name = download_config.recorder.filename_template();
        let headers_in = construct_headers(&download_config.headers).map_err(AppError::Custom)?;
        let proxy = self.proxy.clone();
        let segment = Segmentable::new(
            download_config.segment_time.as_deref().map(parse_time),
            download_config.file_size,
        );

        let stall_timeout = download_config
            .stall_timeout_secs
            .filter(|secs| *secs > 0)
            .map(Duration::from_secs)
            .unwrap_or(httpflv::DEFAULT_STALL_TIMEOUT);

        // 创建HTTP客户端
        let client = StatelessClient::new(headers_in, proxy.as_deref());
        let on_ready = || {
            if let Some(reconnect) = download_config.reconnect {
                crate::observe::reconnected(
                    &download_config.owner,
                    reconnect.gap_ms,
                    reconnect.silent_ms,
                    reconnect.silent_measured,
                    Some(&attempt_id),
                );
            }
        };
        // let mut i = 0;
        // let mut prev_file_path = None;
        // 创建分段回调钩子
        let hook = {
            let mut i = 0;
            let attempt_id = attempt_id.clone();
            move |s: &str, close_reason, identity: biliup::downloader::util::SegmentIdentity| {
                let file_path = PathBuf::from(s);

                let event = SegmentInfo {
                    prev_file_path: file_path,
                    danmaku_file_path: None,
                    next_file_path: None,
                    segment_index: i,
                    close_reason,
                    attempt_id: Some(attempt_id.clone()),
                    segment_id: Some(identity.segment_id),
                    recovery_source_paths: Vec::new(),
                    enrollment: None,
                };
                callback(SegmentEvent::Segment(event));

                i += 1;
            }
        };
        // Known HLS sources do not need a preliminary FLV-header request. In particular,
        // its bytes and errors must not be counted as measured FLV media silence.
        if requested_protocol == "hls" {
            info!(
                attempt_id,
                stream_host,
                protocol = "hls",
                candidate_count,
                quality = download_config.quality.as_deref().unwrap_or("unknown"),
                "starting stream download"
            );
            let file =
                LifecycleFile::with_hook_and_close_handle(&file_name, "ts", close_handle, hook)
                    .with_owner(download_config.owner.owner(Some(&attempt_id)));
            hls::download_with_ready(&url, &client, file, segment, on_ready)
                .await
                .change_context(AppError::Unknown)?;
            return Ok(DownloadStatus::StreamEnded);
        }
        // 获取可重试的响应
        let response = match client.retryable(&url).await {
            Ok(response) => response,
            Err(err) => {
                let status = classify_reqwest_error(err);
                warn!(
                    attempt_id,
                    stream_host,
                    protocol = requested_protocol,
                    candidate_count,
                    quality = download_config.quality.as_deref().unwrap_or("unknown"),
                    result = ?status,
                    "download stream request failed"
                );
                return Ok(status);
            }
        };
        // 创建连接
        let mut connection = Connection::with_stall_timeout(response, stall_timeout);
        // 读取帧头
        let bytes = match connection.read_frame(9).await {
            Ok(bytes) => bytes,
            Err(err) => {
                let diagnostics = connection.diagnostics();
                self.record_gap(&connection);
                let status = classify_download_error(err);
                warn!(
                    attempt_id,
                    http_status = diagnostics.http_status,
                    content_encoding = diagnostics.content_encoding.as_deref().unwrap_or("none"),
                    transfer_encoding = diagnostics.transfer_encoding.as_deref().unwrap_or("none"),
                    received_bytes = diagnostics.received_bytes,
                    connected_for = ?diagnostics.connected_for,
                    buffered = diagnostics.buffered,
                    stall_timeout_secs = stall_timeout.as_secs(),
                    stream_host,
                    protocol = requested_protocol,
                    quality = download_config.quality.as_deref().unwrap_or("unknown"),
                    result = ?status,
                    "download stream header read failed"
                );
                return Ok(status);
            }
        };
        // 解析流头部，判断流类型
        match header(&bytes) {
            Ok((_i, header)) => {
                on_ready();
                debug!("header: {header:#?}");
                info!(
                    attempt_id,
                    stream_host = %stream_host,
                    protocol = "flv",
                    candidate_count,
                    quality = download_config.quality.as_deref().unwrap_or("unknown"),
                    stall_timeout_secs = stall_timeout.as_secs(),
                    "starting stream download"
                );
                // FLV流下载
                let file = LifecycleFile::with_hook_and_close_handle(
                    &file_name,
                    "flv",
                    close_handle,
                    hook,
                )
                .with_owner(download_config.owner.owner(Some(&attempt_id)));
                let log_context = httpflv::HttpFlvLogContext {
                    attempt_id: attempt_id.clone(),
                    stream_host: stream_host.clone(),
                    protocol: "flv".to_string(),
                    quality: download_config.quality.clone(),
                };
                let download_result = httpflv::download_with_context(
                    &mut connection,
                    file,
                    segment.clone(),
                    log_context,
                )
                .await;
                self.record_gap(&connection);
                match download_result {
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
                    candidate_count,
                    quality = download_config.quality.as_deref().unwrap_or("unknown"),
                    "starting stream download"
                );
                let file =
                    LifecycleFile::with_hook_and_close_handle(&file_name, "ts", close_handle, hook)
                        .with_owner(download_config.owner.owner(Some(&attempt_id)));
                hls::download_with_ready(&url, &client, file, segment.clone(), on_ready)
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
        // Keep the future alive until the cancellation branch records the reason. select!
        // drops its losing futures before running the winning handler; an owned future there
        // would finalize the active file as a transport error before we can mark cancellation.
        let download = self.start_download(callback, download_config, close_handle.clone());
        tokio::pin!(download);
        tokio::select! {
            _ = token.cancelled() => {
                close_handle.set(SegmentCloseReason::Cancelled);
                Ok(DownloadStatus::Cancelled)
            }
            res = &mut download => {res}
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::RecordingIdentity;
    use crate::server::common::util::Recorder;
    use crate::server::core::downloader::ReconnectContext;
    use crate::server::infrastructure::models::StreamerInfo;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tracing::field::{Field, Visit};
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<BTreeMap<String, String>>>>);

    struct Collector<'a>(&'a mut BTreeMap<String, String>);
    impl Visit for Collector<'_> {
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
        fn record_u64(&mut self, field: &Field, value: u64) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
        fn record_bool(&mut self, field: &Field, value: bool) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0
                .insert(field.name().to_string(), format!("{value:?}"));
        }
    }

    impl<S: tracing::Subscriber> Layer<S> for Captured {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if event.metadata().target() != crate::observe::EVENT_TARGET {
                return;
            }
            let mut fields = BTreeMap::new();
            event.record(&mut Collector(&mut fields));
            self.0.lock().unwrap().push(fields);
        }
    }

    impl Captured {
        fn named(&self, name: &str) -> Vec<BTreeMap<String, String>> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|fields| fields.get("event_name").map(String::as_str) == Some(name))
                .cloned()
                .collect()
        }
    }

    fn append_tag(bytes: &mut Vec<u8>, tag_type: u8, body: &[u8], timestamp: u32) {
        bytes.push(tag_type);
        bytes.extend_from_slice(&[
            ((body.len() >> 16) & 0xff) as u8,
            ((body.len() >> 8) & 0xff) as u8,
            (body.len() & 0xff) as u8,
            ((timestamp >> 16) & 0xff) as u8,
            ((timestamp >> 8) & 0xff) as u8,
            (timestamp & 0xff) as u8,
            ((timestamp >> 24) & 0xff) as u8,
            0,
            0,
            0,
        ]);
        bytes.extend_from_slice(body);
        bytes.extend_from_slice(&((11 + body.len()) as u32).to_be_bytes());
    }

    #[tokio::test]
    async fn hls_server_reconnect_requires_media_and_cancel_preserves_identity() {
        use axum::{Router, http::StatusCode, routing::get};
        // Recorder sanitizes templates into basenames, so the executor writes in the process
        // cwd. Remove only paths delivered by this test, including when an assertion fails.
        struct Cleanup(Arc<Mutex<Vec<SegmentInfo>>>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                for segment in self.0.lock().unwrap().iter() {
                    let _ = std::fs::remove_file(&segment.prev_file_path);
                }
            }
        }
        for mode in ["ended", "invalid", "absent", "cancel"] {
            let directory = tempfile::tempdir().unwrap();
            let captured = Captured::default();
            let _guard = tracing::subscriber::set_default(
                tracing_subscriber::registry().with(captured.clone()),
            );
            let playlist = if mode == "invalid" {
                "invalid playlist".to_string()
            } else {
                format!(
                    "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:1,\npart.ts\n{}",
                    if mode == "cancel" {
                        ""
                    } else {
                        "#EXT-X-ENDLIST\n"
                    }
                )
            };
            let app = Router::new()
                .route(
                    "/index.m3u8",
                    get(move || {
                        let body = playlist.clone();
                        async move { body }
                    }),
                )
                .route(
                    "/part.ts",
                    get(move || async move {
                        if mode == "absent" {
                            (StatusCode::NOT_FOUND, "absent")
                        } else {
                            (StatusCode::OK, "synthetic media")
                        }
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/index.m3u8", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let streamer_info = StreamerInfo::new(
                "受控主播",
                "http://127.0.0.1/controlled",
                "受控标题",
                chrono::Utc::now(),
                "",
            );
            let config = DownloadConfig {
                url,
                stream_candidates: Vec::new(),
                segment_time: None,
                file_size: None,
                headers: Default::default(),
                recorder: Recorder::new(
                    Some(directory.path().join("hls-%s-%f").display().to_string()),
                    streamer_info,
                ),
                output_dir: directory.path().to_path_buf(),
                suffix: "m3u8".into(),
                owner: RecordingIdentity::server(7, 42, "受控主播"),
                reconnect: Some(ReconnectContext {
                    gap_ms: 1000,
                    silent_ms: 0,
                    silent_measured: false,
                }),
                attempt_id: Some("attempt-controlled-hls".into()),
                quality: None,
                stall_timeout_secs: Some(5),
            };
            let segments = Arc::new(Mutex::new(Vec::new()));
            let _cleanup = Cleanup(segments.clone());
            let hook = segments.clone();
            let downloader = StreamGears::new(None);
            let download = downloader.download(
                Box::new(move |event| {
                    if let SegmentEvent::Segment(info) = event {
                        hook.lock().unwrap().push(info);
                    }
                }),
                config,
            );
            let stop = async {
                if mode == "cancel" {
                    while captured.named("recording.reconnected").is_empty() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    downloader.stop().await.unwrap();
                }
            };
            let (result, _) = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(download, stop)
            })
            .await
            .unwrap();
            server.abort();
            let reconnected = captured.named("recording.reconnected");
            let created = captured.named("recording.segment_created");
            let closed = captured.named("recording.segment_closed");
            assert!(
                downloader.take_last_gap().is_none(),
                "HLS must not fabricate FLV silence measurements"
            );
            if mode == "invalid" || mode == "absent" {
                assert!(result.is_err());
                assert!(reconnected.is_empty());
                let disconnected = captured.named("recording.disconnected");
                assert_eq!(disconnected.len(), 1);
                assert_eq!(
                    disconnected[0]["download_attempt_id"],
                    "attempt-controlled-hls"
                );
                assert_eq!(
                    disconnected[0]["reason_code"],
                    if mode == "invalid" {
                        "invalid_playlist"
                    } else {
                        "http_error"
                    }
                );
                continue;
            }
            assert!(matches!(
                (mode, result.unwrap()),
                ("ended", DownloadStatus::StreamEnded) | ("cancel", DownloadStatus::Cancelled)
            ));
            assert_eq!(reconnected.len(), 1);
            assert_eq!(reconnected[0]["reason_code"], "estimated_gap");
            assert_eq!(created.len(), 1);
            assert_eq!(closed.len(), 1);
            assert_eq!(
                closed[0]["reason_code"],
                if mode == "cancel" {
                    "user_cancel"
                } else {
                    "stream_end"
                }
            );
            assert_eq!(closed[0]["segment_id"], created[0]["segment_id"]);
            assert_eq!(created[0]["live_streamer_id"], "7");
            assert_eq!(created[0]["streamer_info_id"], "42");
            assert_eq!(created[0]["download_attempt_id"], "attempt-controlled-hls");
            let segments = segments.lock().unwrap();
            assert_eq!(segments.len(), 1);
            assert_eq!(
                segments[0].segment_id.as_deref(),
                Some(created[0]["segment_id"].as_str())
            );
            assert!(captured.named("recording.disconnected").is_empty());
        }
    }

    fn splittable_flv(keyframes: usize, payload: usize) -> Vec<u8> {
        let mut bytes = vec![b'F', b'L', b'V', 1, 5, 0, 0, 0, 9, 0, 0, 0, 0];
        let mut metadata = vec![0x02, 0x00, 0x0a];
        metadata.extend_from_slice(b"onMetaData");
        metadata.push(0x05);
        append_tag(&mut bytes, 18, &metadata, 0);
        append_tag(&mut bytes, 8, &[0xaf, 0x00, 0x12, 0x10], 0);
        append_tag(&mut bytes, 9, &[0x17, 0x00, 0, 0, 0, 0x01, 0x64, 0x00], 0);
        for index in 0..keyframes {
            let mut frame = vec![0x17, 0x01, 0, 0, 0];
            frame.resize(5 + payload, 0x41);
            append_tag(&mut bytes, 9, &frame, (index as u32 + 1) * 1_000);
        }
        bytes
    }

    async fn serve_once(body: Vec<u8>) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let _ = socket.read(&mut request).await;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: video/x-flv\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
            socket.flush().await.unwrap();
        });
        port
    }

    /// The server recording path must carry the room and session identity onto every native
    /// event, and prove a reconnect only when the connection is actually established.
    #[tokio::test]
    async fn server_recording_events_carry_room_session_and_attempt_identity() {
        let directory = tempfile::tempdir().unwrap();
        let captured = Captured::default();
        let _guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));
        let port = serve_once(splittable_flv(6, 2_000)).await;

        let streamer_info = StreamerInfo::new(
            "受控主播",
            "http://127.0.0.1/controlled",
            "受控标题",
            chrono::Utc::now(),
            "",
        );
        let template = directory
            .path()
            .join("segment-%H%M%S")
            .display()
            .to_string();
        let config = DownloadConfig {
            url: format!("http://127.0.0.1:{port}/stream.flv"),
            stream_candidates: Vec::new(),
            segment_time: None,
            file_size: Some(4_000),
            headers: Default::default(),
            recorder: Recorder::new(Some(template), streamer_info),
            output_dir: directory.path().to_path_buf(),
            suffix: "flv".to_string(),
            owner: RecordingIdentity::server(7, 42, "受控主播"),
            reconnect: Some(ReconnectContext {
                gap_ms: 8_000,
                silent_ms: 3_000,
                silent_measured: true,
            }),
            attempt_id: Some("attempt-controlled".to_string()),
            quality: None,
            stall_timeout_secs: Some(5),
        };

        let segments: Arc<Mutex<Vec<SegmentInfo>>> = Arc::default();
        let collected = segments.clone();
        let downloader = StreamGears::new(None);
        let status = downloader
            .download(
                Box::new(move |event| {
                    if let SegmentEvent::Segment(info) = event {
                        collected.lock().unwrap().push(info);
                    }
                }),
                config,
            )
            .await
            .unwrap();
        assert!(matches!(status, DownloadStatus::StreamEnded));

        let reconnected = captured.named("recording.reconnected");
        assert_eq!(
            reconnected.len(),
            1,
            "one reconnect per established connection"
        );
        assert_eq!(reconnected[0]["gap_ms"], "8000");
        assert_eq!(reconnected[0]["reason_code"], "measured_gap");
        assert_eq!(reconnected[0]["download_attempt_id"], "attempt-controlled");
        assert_eq!(reconnected[0]["live_streamer_id"], "7");
        assert_eq!(reconnected[0]["streamer_info_id"], "42");

        let created = captured.named("recording.segment_created");
        assert!(created.len() >= 2, "the fixture must split");
        for event in &created {
            assert_eq!(event["live_streamer_id"], "7");
            assert_eq!(event["streamer_info_id"], "42");
            assert_eq!(event["download_attempt_id"], "attempt-controlled");
        }

        // The identity the hook hands to the upload pipeline is the one the events reported.
        let reported: Vec<String> = created
            .iter()
            .map(|event| event["segment_id"].clone())
            .collect();
        let delivered = segments.lock().unwrap();
        assert!(!delivered.is_empty());
        for info in delivered.iter() {
            let segment_id = info.segment_id.clone().expect("segment identity");
            assert!(
                reported.contains(&segment_id),
                "{segment_id} was never created"
            );
        }
    }
}
