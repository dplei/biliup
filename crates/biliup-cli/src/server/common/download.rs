use crate::server::common::cookie_health;
use crate::server::common::upload::UploaderMessage;
use crate::server::common::util::{FileValidator, MediaValidation};
use crate::server::core::downloader::cover_downloader;
use crate::server::core::downloader::{
    DanmakuClient, DownloadStatus, DownloaderRuntime, SegmentEvent, SegmentInfo,
};
use crate::server::core::live::{danmaku_client, downloader_runtime, live_request};
use crate::server::core::monitor::Monitor;
use crate::server::errors::{AppError, AppResult};
use crate::server::infrastructure::context::{Context, Stage, WorkerStatus};
use crate::server::infrastructure::models::hook_step::process;
use async_channel::Sender;
use biliup::downloader::live::{LivePlugin, LiveStatus, LiveStream};
use error_stack::{ResultExt, bail};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

const RETRY_BASE_DELAY: Duration = Duration::from_secs(2);
const RETRY_MAX_DELAY: Duration = Duration::from_secs(30);
const ROUTE_STABLE_THRESHOLD: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
struct RouteKey {
    host: Option<String>,
    protocol: &'static str,
    quality: Option<String>,
}

impl RouteKey {
    fn from_stream(stream: &LiveStream) -> Self {
        let parsed = url::Url::parse(&stream.raw_stream_url).ok();
        let host = parsed
            .as_ref()
            .and_then(|url| url.host_str())
            .map(ToString::to_string);
        let path = parsed.as_ref().map(url::Url::path).unwrap_or_default();
        let protocol = if stream.suffix.eq_ignore_ascii_case("m3u8")
            || stream.suffix.eq_ignore_ascii_case("ts")
            || path.ends_with(".m3u8")
        {
            "hls"
        } else {
            "flv"
        };
        Self {
            host,
            protocol,
            quality: stream.recording_quality.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RouteHealthUpdate {
    Unchanged,
    Failure(u32),
    Recovered,
}

/// 拉流线路健康状态。它只在直播状态确认仍为 Live 后接收终止结果，因而不会把
/// 正常下播（包括 404 + Offline）误计为 CDN 线路故障。
#[derive(Debug)]
struct RouteHealthState {
    enabled: bool,
    consecutive_transport_failures: u32,
    last_failure_at: Option<Instant>,
    stable_since: Option<Instant>,
    current_route_key: Option<RouteKey>,
}

impl RouteHealthState {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            consecutive_transport_failures: 0,
            last_failure_at: None,
            stable_since: None,
            current_route_key: None,
        }
    }

    fn begin_attempt(&mut self, stream: &LiveStream, now: Instant) {
        if !self.enabled {
            return;
        }
        let route = RouteKey::from_stream(stream);
        if self.current_route_key.as_ref() != Some(&route) {
            self.current_route_key = Some(route);
        }
        self.stable_since = Some(now);
    }

    fn observe_live_attempt(
        &mut self,
        status: Option<&DownloadStatus>,
        connected_for: Duration,
        completed_configured_segment: bool,
        now: Instant,
    ) -> RouteHealthUpdate {
        if !self.enabled || matches!(status, Some(DownloadStatus::Cancelled)) {
            return RouteHealthUpdate::Unchanged;
        }

        let was_unhealthy = self.consecutive_transport_failures > 0;
        if connected_for >= ROUTE_STABLE_THRESHOLD || completed_configured_segment {
            self.consecutive_transport_failures = 0;
            self.last_failure_at = None;
        }

        // AppResult::Err 也属于传输失败；具体错误已经由下载器日志脱敏记录。
        let is_transport_failure = status
            .map(DownloadStatus::is_transport_failure)
            .unwrap_or(true);
        if is_transport_failure {
            self.consecutive_transport_failures =
                self.consecutive_transport_failures.saturating_add(1);
            self.last_failure_at = Some(now);
            self.stable_since = None;
            RouteHealthUpdate::Failure(self.consecutive_transport_failures)
        } else if was_unhealthy && self.consecutive_transport_failures == 0 {
            RouteHealthUpdate::Recovered
        } else {
            RouteHealthUpdate::Unchanged
        }
    }

    /// 记录刷新签名后实际选中的路线。阶段 2 尚没有候选集合；当平台真实返回的
    /// Host/协议/画质已经变化时，将它视作可立即尝试的新路线。
    fn select_next_route(&mut self, stream: &LiveStream) -> bool {
        if !self.enabled {
            return false;
        }
        let next = RouteKey::from_stream(stream);
        let changed = self
            .current_route_key
            .as_ref()
            .is_some_and(|key| key != &next);
        if changed {
            self.current_route_key = Some(next);
            self.stable_since = None;
        }
        changed
    }

    fn retry_delay(&self, route_changed: bool) -> Duration {
        if route_changed {
            return Duration::ZERO;
        }
        exponential_backoff(self.consecutive_transport_failures.max(1))
    }
}

#[derive(Debug, Default)]
struct OfflineRetryState {
    offline_since: Option<Instant>,
    offline_retry_count: u32,
}

impl OfflineRetryState {
    fn record_live(&mut self) {
        self.offline_since = None;
        self.offline_retry_count = 0;
    }

    fn record_unavailable(&mut self, now: Instant, grace: Duration) -> bool {
        let since = *self.offline_since.get_or_insert(now);
        self.offline_retry_count = self.offline_retry_count.saturating_add(1);
        now.saturating_duration_since(since) >= grace
    }

    /// 保持阶段 2 之前的下播宽限复查间隔：首次不可用后等待 4 秒。
    fn retry_delay(&self) -> Duration {
        RETRY_BASE_DELAY
            .saturating_mul(2_u32.saturating_pow(self.offline_retry_count.min(5)))
            .min(RETRY_MAX_DELAY)
    }
}

fn exponential_backoff(failure_count: u32) -> Duration {
    RETRY_BASE_DELAY
        .saturating_mul(2_u32.saturating_pow(failure_count.saturating_sub(1).min(5)))
        .min(RETRY_MAX_DELAY)
}

struct DownloadAttempt {
    result: AppResult<DownloadStatus>,
    connected_for: Duration,
    completed_configured_segment: bool,
}

/// 分段事件处理器
pub struct SegmentEventProcessor {
    channel: Option<Sender<SegmentInfo>>,
    uploader: Sender<UploaderMessage>,
    ctx: Context,
    file_validator: FileValidator,
    preserve_recoverable_short_segments: bool,
    pending_short_segments: Vec<SegmentInfo>,
}

impl SegmentEventProcessor {
    /// 创建处理器
    pub fn new(uploader: Sender<UploaderMessage>, ctx: Context) -> Self {
        let config = ctx.config();
        Self {
            channel: None,
            uploader,
            file_validator: FileValidator::new(config.filtering_threshold * 1000 * 1000, true),
            preserve_recoverable_short_segments: config
                .preserve_recoverable_short_segments
                .unwrap_or(true),
            pending_short_segments: Vec::new(),
            ctx,
        }
    }

    /// 处理分段事件
    pub fn process(&mut self, event: SegmentInfo) -> AppResult<()> {
        // 删除决定只能发生在媒体内容探测之后；体积本身不再代表文件无效。
        match self.file_validator.validate(&event.prev_file_path)? {
            MediaValidation::Valid => {
                info!(
                    file = %event.prev_file_path.display(),
                    close_reason = ?event.close_reason,
                    attempt_id = event.attempt_id.as_deref().unwrap_or("untracked"),
                    "validated media segment"
                );
                self.flush_pending_short_segments()?;
                self.enqueue(event)
            }
            MediaValidation::RecoverableShort { duration } => {
                if self.preserve_recoverable_short_segments {
                    warn!(
                        file = %event.prev_file_path.display(),
                        duration = ?duration,
                        close_reason = ?event.close_reason,
                        attempt_id = event.attempt_id.as_deref().unwrap_or("untracked"),
                        "queueing recoverable short media segment"
                    );
                    self.pending_short_segments.push(event);
                    Ok(())
                } else {
                    self.remove_invalid_segment(
                        &event.prev_file_path,
                        "recoverable short segment rejected by rollback configuration",
                    );
                    Ok(())
                }
            }
            MediaValidation::Invalid { reason } => {
                warn!(
                    file = %event.prev_file_path.display(),
                    reason = ?reason,
                    close_reason = ?event.close_reason,
                    attempt_id = event.attempt_id.as_deref().unwrap_or("untracked"),
                    "discarding invalid media segment"
                );
                self.remove_invalid_segment(&event.prev_file_path, &format!("{reason:?}"));
                Ok(())
            }
        }
    }

    pub fn finish(&mut self) -> AppResult<()> {
        self.flush_pending_short_segments()
    }

    fn flush_pending_short_segments(&mut self) -> AppResult<()> {
        if self.pending_short_segments.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.pending_short_segments);
        for group in compatible_segment_groups(pending) {
            if group.len() > 1 {
                match merge_compatible_segments(&group, &self.file_validator) {
                    Ok(merged) => {
                        let original_files: Vec<_> = group
                            .iter()
                            .map(|event| event.prev_file_path.display().to_string())
                            .collect();
                        info!(
                            output = %merged.prev_file_path.display(),
                            originals = ?original_files,
                            "merged compatible recoverable short segments; originals retained"
                        );
                        self.enqueue(merged)?;
                        continue;
                    }
                    Err(error) => warn!(
                        error = ?error,
                        files = ?group.iter().map(|event| &event.prev_file_path).collect::<Vec<_>>(),
                        "failed to merge recoverable segments; preserving and uploading originals"
                    ),
                }
            }
            for event in group {
                self.enqueue(event)?;
            }
        }
        Ok(())
    }

    fn enqueue(&mut self, event: SegmentInfo) -> AppResult<()> {
        // 上一轮 process_with_upload 可能因上传失败提前返回，UActor 已 drop rx，
        // 这里挂着的 tx 是死的；丢弃后下面会重建一条新的管道。
        if let Some(tx) = &self.channel
            && tx.is_closed()
        {
            warn!(
                url = self.ctx.live_streamer().url,
                "upload channel closed by uploader, reopening"
            );
            self.channel = None;
        }

        match &self.channel {
            None => {
                // 故障窗口可能一次释放几十个不可合并的短片；不能因 32 项上限静默顶掉旧事件。
                let (tx, rx) = async_channel::unbounded();

                // 发送到上传器
                let res = self
                    .uploader
                    .force_send(UploaderMessage::SegmentEvent(rx.clone(), self.ctx.clone()))
                    .change_context(AppError::Custom("Failed to send to uploader".to_string()))?;
                if let Some(prev) = res {
                    warn!(SegmentEvent = ?prev, "replace an existing message in the channel");
                }

                // 发送到缓冲区
                let res = tx
                    .force_send(event)
                    .change_context(AppError::Custom("Failed to send to buffer".to_string()))?;
                if let Some(prev) = res {
                    warn!(SegmentEvent = ?prev, "replace an existing message in the channel");
                }
                self.channel = Some(tx);
            }
            Some(tx) => {
                // 发送到缓冲区
                let res = tx
                    .force_send(event)
                    .change_context(AppError::Custom("Failed to send to buffer".to_string()))?;
                if let Some(prev) = res {
                    warn!(SegmentEvent = ?prev, "replace an existing message in the channel");
                }
            }
        }

        Ok(())
    }

    fn remove_invalid_segment(&self, path: &std::path::Path, reason: &str) {
        let path = path.to_owned();
        let reason = reason.to_string();
        tokio::spawn(async move {
            match crate::server::infrastructure::models::hook_step::HookStep::remove_file(&[&path])
                .await
            {
                Ok(()) => info!(file = %path.display(), reason, "removed invalid media segment"),
                Err(error) => error!(
                    file = %path.display(),
                    reason,
                    error = ?error,
                    "failed to remove invalid media segment; original preserved"
                ),
            }
        });
    }
}

fn compatible_segment_groups(events: Vec<SegmentInfo>) -> Vec<Vec<SegmentInfo>> {
    let mut groups: Vec<(Option<u64>, Vec<SegmentInfo>)> = Vec::new();
    for event in events {
        let key = media_compatibility_key(&event);
        let joins_previous = key.is_some()
            && groups
                .last()
                .is_some_and(|(previous_key, _)| previous_key == &key);
        if joins_previous {
            groups.last_mut().expect("group exists").1.push(event);
        } else {
            groups.push((key, vec![event]));
        }
    }
    groups.into_iter().map(|(_, group)| group).collect()
}

fn media_compatibility_key(event: &SegmentInfo) -> Option<u64> {
    if event.danmaku_file_path.is_some()
        || !event
            .prev_file_path
            .extension()?
            .to_str()?
            .eq_ignore_ascii_case("flv")
    {
        return None;
    }
    let bytes = std::fs::read(&event.prev_file_path).ok()?;
    if bytes.len() <= 13 || bytes.get(..3) != Some(b"FLV") {
        return None;
    }
    let mut offset = u32::from_be_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]) as usize + 4;
    let mut headers: Vec<(u8, Vec<u8>)> = Vec::new();
    while offset < bytes.len() {
        if bytes.len().saturating_sub(offset) < 15 {
            return None;
        }
        let tag_type = bytes[offset];
        let size = ((bytes[offset + 1] as usize) << 16)
            | ((bytes[offset + 2] as usize) << 8)
            | bytes[offset + 3] as usize;
        let body_start = offset + 11;
        let body_end = body_start.checked_add(size)?;
        let next = body_end.checked_add(4)?;
        if next > bytes.len() {
            return None;
        }
        let body = &bytes[body_start..body_end];
        let is_sequence_header = match tag_type {
            8 => body.len() > 2 && (body[0] >> 4) == 10 && body[1] == 0,
            9 => body.len() > 5 && (body[0] & 0x0f) == 7 && body[1] == 0,
            _ => false,
        };
        if is_sequence_header {
            headers.push((tag_type, body.to_vec()));
        }
        offset = next;
    }
    if headers.is_empty() {
        return None;
    }
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    headers.hash(&mut hasher);
    Some(hasher.finish())
}

fn merge_compatible_segments(
    events: &[SegmentInfo],
    validator: &FileValidator,
) -> AppResult<SegmentInfo> {
    let first = events
        .first()
        .ok_or_else(|| AppError::Custom("cannot merge an empty segment group".into()))?;
    let last = events.last().expect("non-empty segment group");
    let parent = first
        .prev_file_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let stem = first
        .prev_file_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("segment");
    let suffix = format!("{}-{}", std::process::id(), first.segment_index);
    let list_path = parent.join(format!(".{stem}.{suffix}.concat.txt"));
    let output_path = parent.join(format!("{stem}.{suffix}.recovered.flv"));
    let mut concat_list = String::new();
    for event in events {
        let absolute = event
            .prev_file_path
            .canonicalize()
            .change_context(AppError::Unknown)?;
        let escaped = absolute.to_string_lossy().replace('\'', "'\\''");
        concat_list.push_str(&format!("file '{escaped}'\n"));
    }
    std::fs::write(&list_path, concat_list).change_context(AppError::Unknown)?;
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "warning",
            "-y",
            "-f",
            "concat",
            "-safe",
            "0",
            "-i",
        ])
        .arg(&list_path)
        .args(["-c", "copy"])
        .arg(&output_path)
        .status()
        .change_context(AppError::Custom(
            "failed to spawn ffmpeg for short segment merge".into(),
        ));
    let _ = std::fs::remove_file(&list_path);
    let status = status?;
    if !status.success() {
        let _ = std::fs::remove_file(&output_path);
        bail!(AppError::Custom(format!(
            "ffmpeg short segment merge failed with {status}"
        )));
    }
    if matches!(
        validator.validate(&output_path)?,
        MediaValidation::Invalid { .. }
    ) {
        let _ = std::fs::remove_file(&output_path);
        bail!(AppError::Custom(
            "merged short segment failed media validation".into()
        ));
    }
    Ok(SegmentInfo {
        prev_file_path: output_path,
        danmaku_file_path: None,
        next_file_path: None,
        segment_index: first.segment_index,
        close_reason: last.close_reason,
        attempt_id: first.attempt_id.clone(),
        recovery_source_paths: events
            .iter()
            .map(|event| event.prev_file_path.clone())
            .collect(),
    })
}

/// 下载任务
pub struct DownloadTask {
    token: CancellationToken,
    done_notify: Notify,
    downloader: DownloaderRuntime,
}

impl DownloadTask {
    pub fn new(downloader: DownloaderRuntime) -> Self {
        Self {
            token: CancellationToken::new(),
            done_notify: Notify::new(),
            downloader,
        }
    }

    pub(self) async fn execute(
        &self,
        ctx: &Context,
        sender: Sender<UploaderMessage>,
        plugin: Arc<dyn LivePlugin + Send + Sync>,
        rooms_handle: Arc<Monitor>,
    ) -> AppResult<()> {
        // 下播确认 / 重连配置
        // grace = 下播宽限期（config.delay 秒）：流中断后持续复查重连，直到「确实连续离线
        // 超过 grace」才判定真下播 → 投稿。避免抖音等 flv 短暂中断（CDN/签名轮换）被当成
        // 下播，结果一场直播被切成多个稿件。grace=0（默认）→ 一离线立即结束，保持老行为。
        let grace = Duration::from_secs(ctx.config().delay);
        let route_health_enabled = ctx.config().route_health_enabled.unwrap_or(true);
        let mut route_health = RouteHealthState::new(route_health_enabled);
        let mut offline_retry = OfflineRetryState::default();
        let url = ctx.live_streamer().url.clone();
        // cookie 健康监测：录制中断流复查时同样喂给健康统计（录制时检测）
        let platform = plugin.name();
        let cookie_webhook = ctx.config().cookie_health_webhook.clone();
        let mut stream = ctx.live_stream().clone();
        let filename_prefix = ctx
            .live_streamer()
            .filename_prefix
            .clone()
            .or_else(|| ctx.config().filename_prefix.clone());
        let danmaku_client = danmaku_client(
            stream.danmaku.as_ref(),
            filename_prefix.as_deref(),
            &stream.name,
        );
        // 启动弹幕客户端
        if let Some(ref client) = danmaku_client {
            // 启动弹幕下载逻辑
            info!("Starting danmaku client for stream: {}", url);
            client.download().await?;
        }

        // 初始化组件
        let mut processor = SegmentEventProcessor::new(sender, ctx.clone());
        let result = loop {
            // 创建守卫确保清理
            // 创建事件处理器
            // 执行下载
            route_health.begin_attempt(&stream, Instant::now());
            let attempt = self
                .download(&mut processor, ctx.clone(), danmaku_client.clone(), &stream)
                .await;
            let components = attempt.result;

            info!("initialize_components completed: {url}");

            if self.token.is_cancelled() {
                info!(url = url, "task is cancelled");
                break components;
            }
            // 检查流状态
            let check_started = std::time::Instant::now();
            let check_result = plugin.check_stream(live_request(ctx.worker())).await;
            let check_elapsed = check_started.elapsed();
            let backoff = match check_result {
                Ok(LiveStatus::Live {
                    stream: next_stream,
                }) => {
                    cookie_health::record_success(platform, cookie_webhook.as_deref());
                    offline_retry.record_live();
                    let health_update = route_health.observe_live_attempt(
                        components.as_ref().ok(),
                        attempt.connected_for,
                        attempt.completed_configured_segment,
                        Instant::now(),
                    );
                    match health_update {
                        RouteHealthUpdate::Failure(failures) => warn!(
                            url = url,
                            failures,
                            host = route_health
                                .current_route_key
                                .as_ref()
                                .and_then(|key| key.host.as_deref())
                                .unwrap_or("unknown"),
                            protocol = route_health
                                .current_route_key
                                .as_ref()
                                .map(|key| key.protocol)
                                .unwrap_or("unknown"),
                            quality = route_health
                                .current_route_key
                                .as_ref()
                                .and_then(|key| key.quality.as_deref())
                                .unwrap_or("unknown"),
                            "stream is live but route transport failed"
                        ),
                        RouteHealthUpdate::Recovered => info!(
                            url = url,
                            connected_for = ?attempt.connected_for,
                            "stream route recovered after stable download"
                        ),
                        RouteHealthUpdate::Unchanged => {}
                    }
                    stream = *next_stream;
                    ctx.worker()
                        .set_recording_quality(stream.recording_quality.clone());
                    // Live 只重置下播状态；线路失败历史由 RouteHealthState 独立维护。
                    let route_changed = route_health.select_next_route(&stream);
                    info!(url = url, check_elapsed = ?check_elapsed, "Stream is still live, continuing same session");
                    if route_changed {
                        info!(
                            url = url,
                            host = route_health
                                .current_route_key
                                .as_ref()
                                .and_then(|key| key.host.as_deref())
                                .unwrap_or("unknown"),
                            protocol = route_health
                                .current_route_key
                                .as_ref()
                                .map(|key| key.protocol)
                                .unwrap_or("unknown"),
                            quality = route_health
                                .current_route_key
                                .as_ref()
                                .and_then(|key| key.quality.as_deref())
                                .unwrap_or("unknown"),
                            "refreshed stream selected a new route; retrying immediately"
                        );
                    }
                    if route_health_enabled {
                        route_health.retry_delay(route_changed)
                    } else {
                        // 回滚开关：保留旧流程中 Live 后固定 2 秒重试的行为。
                        RETRY_BASE_DELAY
                    }
                }
                Ok(LiveStatus::Offline) => {
                    // 下播是一次成功的检查（cookie 正常）
                    cookie_health::record_success(platform, cookie_webhook.as_deref());
                    let now = Instant::now();
                    if offline_retry.record_unavailable(now, grace) {
                        info!(
                            url = url,
                            check_elapsed = ?check_elapsed,
                            "连续离线超过宽限期 {:?}，确认下播，结束本场", grace
                        );
                        break components;
                    }
                    let since = offline_retry.offline_since.expect("offline timestamp set");
                    info!(
                        url = url,
                        check_elapsed = ?check_elapsed,
                        "Stream went offline，宽限期内继续复查 ({:?}/{:?})",
                        now.saturating_duration_since(since),
                        grace
                    );
                    offline_retry.retry_delay()
                }
                Err(e) => {
                    // 检查出错 = cookie 可能失效（去抖后累计，达阈值才提示）
                    cookie_health::record_error(
                        platform,
                        &format!("{e:?}"),
                        cookie_webhook.as_deref(),
                    );
                    let now = Instant::now();
                    if offline_retry.record_unavailable(now, grace) {
                        warn!(
                            url = url,
                            check_elapsed = ?check_elapsed,
                            "检查直播间持续失败超过宽限期 {:?}，结束本场: {:?}", grace, e
                        );
                        break components;
                    }
                    let since = offline_retry.offline_since.expect("offline timestamp set");
                    warn!(
                        url = url,
                        check_elapsed = ?check_elapsed,
                        "Failed to check stream status: {:?}，宽限期内继续复查 ({:?}/{:?})",
                        e,
                        now.saturating_duration_since(since),
                        grace
                    );
                    offline_retry.retry_delay()
                }
            };

            info!("Retrying download in {:?}...", backoff);
            if !backoff.is_zero() {
                tokio::time::sleep(backoff).await;
            }
        };
        if let Err(error) = processor.finish() {
            error!(
                error = ?error,
                "failed to flush queued recoverable segments; original files preserved"
            );
        }
        // 异步清理任务
        if let Some(client) = danmaku_client.clone()
            && let Err(e) = client.stop().await
        {
            error!("Error stopping danmaku client: {}", e);
        }
        // 清理资源
        // 确保状态更新和资源清理
        rooms_handle.wake_waker(ctx.worker_id()).await;
        info!("Download task completed: {:?}", result);
        self.done_notify.notify_one();
        Ok(())
    }

    async fn download(
        &self,
        processor: &mut SegmentEventProcessor,
        ctx: Context,
        danmaku_client: Option<Arc<dyn DanmakuClient + Send + Sync>>,
        stream: &LiveStream,
    ) -> DownloadAttempt {
        // 获取配置和主播信息
        let streamer = ctx.live_streamer();

        // 执行下载
        // let hook = processor.create_hook(danmaku_client.clone());
        let completed_configured_segment = Arc::new(AtomicBool::new(false));
        let completed_configured_segment_for_hook = completed_configured_segment.clone();
        let hook = move |event| {
            match event {
                SegmentEvent::Start { .. } => {
                    warn!("Ignoring unexpected segment start event");
                }
                SegmentEvent::Segment(mut event) => {
                    if matches!(
                        event.close_reason,
                        biliup::downloader::util::SegmentCloseReason::TimedSplit
                            | biliup::downloader::util::SegmentCloseReason::SizeSplit
                    ) {
                        completed_configured_segment_for_hook.store(true, Ordering::Relaxed);
                    }
                    // 分段时，获取到的是已下载的文件名
                    // 触发弹幕滚动保存
                    if let Some(ref client) = danmaku_client {
                        let danmaku_file_path = event.prev_file_path.with_extension("xml");
                        match client.rolling(&danmaku_file_path.display().to_string()) {
                            Ok(true) => event.danmaku_file_path = Some(danmaku_file_path),
                            Ok(false) => {}
                            Err(e) => error!("Danmaku rolling error: {}", e),
                        }
                    }
                    // 异步处理事件
                    // let processor = processor.clone();
                    if let Err(e) = processor.process(event) {
                        error!("Failed to process segment event: {}", e);
                    }
                }
            }
        };

        let started_at = Instant::now();
        let result = self
            .downloader
            .download(Box::new(hook), ctx.download_config(stream))
            .await
            .change_context(AppError::Custom("Failed to download segment".into()));
        let connected_for = started_at.elapsed();
        let completed_configured_segment = completed_configured_segment.load(Ordering::Relaxed)
            || matches!(result.as_ref().ok(), Some(DownloadStatus::SegmentCompleted));

        // 处理结果
        info!(url=streamer.url,result=?result, "finished downloading");
        DownloadAttempt {
            result,
            connected_for,
            completed_configured_segment,
        }
    }

    pub(crate) async fn stop(&self) -> AppResult<()> {
        // 仅发出取消信号并更新状态
        // 如果底层下载函数不支持取消，这里不能真正中断正在进行的下载
        self.token.cancel();
        self.downloader.stop().await?;
        self.done_notify.notified().await;
        Ok(())
    }
}

/// 启动完整下载流程。
///
/// 只能由 `Monitor` 在取得下载池许可后调用；调用方必须把许可移动到同一个任务中，
/// 并持有到本函数返回，保证 `pool1_size` 是下载并发的唯一限制。
pub async fn start_download_workflow(
    downloader: Arc<dyn LivePlugin + Send + Sync>,
    ctx: Context,
    sender: Sender<UploaderMessage>,
    rooms_handle: Arc<Monitor>,
) {
    let task = Arc::new(DownloadTask::new(downloader_runtime(
        ctx.config().downloader,
        ctx.live_stream(),
    )));
    ctx.change_status(Stage::Download, WorkerStatus::Working(task.clone()))
        .await;

    // 记录实际画质供前端 tag 显示
    let recording_quality = ctx.live_stream().recording_quality.clone();
    ctx.worker()
        .set_recording_quality(recording_quality.clone());

    // 抖音画质降级告警：实际画质低于阈值则推送（每场开播仅此一次）
    if ctx.live_stream().platform == "douyin"
        && let Some(actual) = recording_quality.as_deref()
    {
        let cfg = ctx.config();
        if crate::server::common::cookie_health::quality_below_alert(
            actual,
            cfg.douyin_quality_alert.as_deref(),
        ) {
            let threshold = crate::server::common::cookie_health::effective_quality_alert(
                cfg.douyin_quality_alert.as_deref(),
            );
            let actual_disp = crate::server::common::cookie_health::quality_display(actual);
            let threshold_disp = crate::server::common::cookie_health::quality_display(threshold);
            crate::server::common::cookie_health::notify_alert(
                cfg.cookie_health_webhook.as_deref(),
                "⚠️ 抖音 未录到蓝光画质",
                &format!(
                    "{}：当前录制画质为 {}({})，低于告警阈值 {}({})，可能是 cookie（sessionid）失效，建议检查更换。",
                    ctx.live_streamer().remark,
                    actual_disp,
                    actual,
                    threshold_disp,
                    threshold,
                ),
            );
        }
    }

    tokio::spawn({
        let streamer_info = ctx.streamer_info();
        let live_cover_url = streamer_info.live_cover_path.clone();
        let format_filename = ctx.recorder(streamer_info.clone()).format_filename();
        let client = ctx.stateless_client().client.clone();
        let enabled = ctx
            .config()
            .use_live_cover
            .map(|u| u && !live_cover_url.is_empty())
            .unwrap_or(false);
        async move {
            cover_downloader::download_cover_with(
                &live_cover_url,
                enabled,
                &format_filename,
                client,
            )
            .await
        }
    });

    process(&[], &ctx.live_streamer().preprocessor).await;

    let _ = task.execute(&ctx, sender, downloader, rooms_handle).await;

    ctx.worker().set_recording_quality(None);

    process(&[], &ctx.live_streamer().downloaded_processor).await;

    info!(
        "Download workflow completed {} => {:?}",
        ctx.live_streamer().url,
        ctx.status(Stage::Download)
    );
}

#[cfg(test)]
mod route_health_tests {
    use super::{
        OfflineRetryState, ROUTE_STABLE_THRESHOLD, RouteHealthState, RouteHealthUpdate,
        exponential_backoff,
    };
    use crate::server::core::downloader::DownloadStatus;
    use biliup::downloader::live::{DownloaderHint, LiveStream};
    use chrono::Utc;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    fn stream(host: &str, suffix: &str) -> LiveStream {
        LiveStream {
            name: "fixture".to_string(),
            url: "https://live.douyin.com/fixture".to_string(),
            title: "fixture".to_string(),
            date: Utc::now(),
            live_cover_url: String::new(),
            raw_stream_url: format!("https://{host}/live/fixture.{suffix}?sign=secret"),
            platform: "douyin".to_string(),
            stream_headers: HashMap::new(),
            suffix: suffix.to_string(),
            danmaku: None,
            downloader_hint: DownloaderHint::StreamGears,
            runtime_options: None,
            stream_candidates: Vec::new(),
            recording_quality: Some("origin".to_string()),
            attempt_id: Some("attempt-fixture".to_string()),
        }
    }

    #[test]
    fn consecutive_error_plus_live_keeps_failure_history() {
        let stream = stream("pull-flv.example.com", "flv");
        let start = Instant::now();
        let mut health = RouteHealthState::new(true);

        health.begin_attempt(&stream, start);
        assert_eq!(
            health.observe_live_attempt(
                Some(&DownloadStatus::Error("broken body".to_string())),
                Duration::from_secs(20),
                false,
                start + Duration::from_secs(20),
            ),
            RouteHealthUpdate::Failure(1)
        );
        assert!(!health.select_next_route(&stream));

        health.begin_attempt(&stream, start + Duration::from_secs(22));
        assert_eq!(
            health.observe_live_attempt(
                Some(&DownloadStatus::ReadTimeout { buffered: 128 }),
                Duration::from_secs(15),
                false,
                start + Duration::from_secs(37),
            ),
            RouteHealthUpdate::Failure(2)
        );
        assert_eq!(health.consecutive_transport_failures, 2);
        assert_eq!(health.retry_delay(false), Duration::from_secs(4));
    }

    #[test]
    fn backoff_is_two_four_eight_sixteen_then_capped_at_thirty() {
        let delays: Vec<_> = (1..=8).map(exponential_backoff).collect();
        assert_eq!(
            delays,
            vec![
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(16),
                Duration::from_secs(30),
                Duration::from_secs(30),
                Duration::from_secs(30),
                Duration::from_secs(30),
            ]
        );
    }

    #[test]
    fn stable_connection_recovers_route_health() {
        let stream = stream("pull-flv.example.com", "flv");
        let start = Instant::now();
        let mut health = RouteHealthState::new(true);
        health.begin_attempt(&stream, start);
        let _ = health.observe_live_attempt(
            Some(&DownloadStatus::Error("reset".to_string())),
            Duration::from_secs(10),
            false,
            start + Duration::from_secs(10),
        );

        let stable_start = start + Duration::from_secs(12);
        health.begin_attempt(&stream, stable_start);
        assert_eq!(
            health.observe_live_attempt(
                Some(&DownloadStatus::Downloading),
                ROUTE_STABLE_THRESHOLD,
                false,
                stable_start + ROUTE_STABLE_THRESHOLD,
            ),
            RouteHealthUpdate::Recovered
        );
        assert_eq!(health.consecutive_transport_failures, 0);
        assert!(health.last_failure_at.is_none());
    }

    #[test]
    fn configured_segment_completion_recovers_route_health() {
        let stream = stream("pull-flv.example.com", "flv");
        let start = Instant::now();
        let mut health = RouteHealthState::new(true);
        health.begin_attempt(&stream, start);
        let _ = health.observe_live_attempt(
            Some(&DownloadStatus::Error("reset".to_string())),
            Duration::from_secs(10),
            false,
            start + Duration::from_secs(10),
        );
        health.begin_attempt(&stream, start + Duration::from_secs(12));
        assert_eq!(
            health.observe_live_attempt(
                Some(&DownloadStatus::SegmentCompleted),
                Duration::from_secs(30),
                true,
                start + Duration::from_secs(42),
            ),
            RouteHealthUpdate::Recovered
        );
        assert_eq!(health.consecutive_transport_failures, 0);
    }

    #[test]
    fn cancellation_does_not_count_as_failure() {
        let stream = stream("pull-flv.example.com", "flv");
        let start = Instant::now();
        let mut health = RouteHealthState::new(true);
        health.begin_attempt(&stream, start);
        assert_eq!(
            health.observe_live_attempt(
                Some(&DownloadStatus::Cancelled),
                Duration::from_secs(1),
                false,
                start + Duration::from_secs(1),
            ),
            RouteHealthUpdate::Unchanged
        );
        assert_eq!(health.consecutive_transport_failures, 0);
        assert!(health.last_failure_at.is_none());
    }

    #[test]
    fn a_real_route_change_retries_immediately() {
        let first = stream("pull-flv-a.example.com", "flv");
        let second = stream("pull-hls-b.example.com", "m3u8");
        let mut health = RouteHealthState::new(true);
        health.begin_attempt(&first, Instant::now());
        health.consecutive_transport_failures = 4;

        assert!(health.select_next_route(&second));
        assert_eq!(health.retry_delay(true), Duration::ZERO);
        assert_eq!(health.consecutive_transport_failures, 4);
    }

    #[test]
    fn offline_grace_state_keeps_existing_semantics() {
        let start = Instant::now();
        let grace = Duration::from_secs(10);
        let mut offline = OfflineRetryState::default();

        assert!(!offline.record_unavailable(start, grace));
        assert_eq!(offline.retry_delay(), Duration::from_secs(4));
        assert!(!offline.record_unavailable(start + Duration::from_secs(9), grace));
        assert!(offline.record_unavailable(start + Duration::from_secs(10), grace));

        offline.record_live();
        assert!(offline.offline_since.is_none());
        assert_eq!(offline.offline_retry_count, 0);
        assert!(!offline.record_unavailable(start + Duration::from_secs(30), grace));
    }

    #[test]
    fn rollback_switch_disables_route_failure_tracking() {
        let stream = stream("pull-flv.example.com", "flv");
        let start = Instant::now();
        let mut health = RouteHealthState::new(false);
        health.begin_attempt(&stream, start);
        assert_eq!(
            health.observe_live_attempt(
                Some(&DownloadStatus::Error("failure".to_string())),
                Duration::ZERO,
                false,
                start,
            ),
            RouteHealthUpdate::Unchanged
        );
        assert_eq!(health.consecutive_transport_failures, 0);
    }
}

#[cfg(test)]
mod short_segment_group_tests {
    use super::compatible_segment_groups;
    use crate::server::core::downloader::SegmentInfo;
    use biliup::downloader::util::SegmentCloseReason;
    use std::fs;
    use tempfile::tempdir;

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

    fn flv_fixture(codec_marker: u8) -> Vec<u8> {
        let mut bytes = vec![b'F', b'L', b'V', 1, 5, 0, 0, 0, 9, 0, 0, 0, 0];
        append_tag(&mut bytes, 9, &[0x17, 0, 0, 0, 0, 1, 0x64, codec_marker], 0);
        append_tag(&mut bytes, 8, &[0xaf, 0, 0x12, 0x10], 0);
        append_tag(&mut bytes, 9, &[0x17, 1, 0, 0, 0, 0x65], 1000);
        bytes
    }

    fn event(path: std::path::PathBuf, index: usize) -> SegmentInfo {
        SegmentInfo {
            prev_file_path: path,
            danmaku_file_path: None,
            next_file_path: None,
            segment_index: index,
            close_reason: SegmentCloseReason::TransportError,
            attempt_id: Some(format!("attempt-{index}")),
            recovery_source_paths: Vec::new(),
        }
    }

    #[test]
    fn thirty_seven_compatible_segments_form_one_merge_group() {
        let dir = tempdir().unwrap();
        let mut events = Vec::new();
        for index in 0..37 {
            let path = dir.path().join(format!("segment-{index}.flv"));
            fs::write(&path, flv_fixture(1)).unwrap();
            events.push(event(path, index));
        }
        let groups = compatible_segment_groups(events);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 37);
    }

    #[test]
    fn incompatible_codec_parameters_remain_independent() {
        let dir = tempdir().unwrap();
        let first = dir.path().join("first.flv");
        let second = dir.path().join("second.flv");
        fs::write(&first, flv_fixture(1)).unwrap();
        fs::write(&second, flv_fixture(2)).unwrap();
        let groups = compatible_segment_groups(vec![event(first, 0), event(second, 1)]);
        assert_eq!(groups.len(), 2);
        assert!(groups.iter().all(|group| group.len() == 1));
    }
}
