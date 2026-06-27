use crate::server::common::cookie_health;
use crate::server::common::upload::UploaderMessage;
use crate::server::common::util::FileValidator;
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
use error_stack::ResultExt;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

// Configuration and retry policy
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl RetryPolicy {
    pub fn exponential(max_attempts: u32) -> Self {
        Self {
            max_attempts,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(30),
        }
    }
}

/// 分段事件处理器
pub struct SegmentEventProcessor {
    channel: Option<Sender<SegmentInfo>>,
    uploader: Sender<UploaderMessage>,
    ctx: Context,
    file_validator: FileValidator,
}

impl SegmentEventProcessor {
    /// 创建处理器
    pub fn new(uploader: Sender<UploaderMessage>, ctx: Context) -> Self {
        Self {
            channel: None,
            uploader,
            file_validator: FileValidator::new(
                ctx.config().filtering_threshold * 1000 * 1000,
                true,
            ),
            ctx,
        }
    }

    /// 处理分段事件
    pub fn process(&mut self, event: SegmentInfo) -> AppResult<()> {
        // 验证文件有效性
        self.file_validator.validate(&event.prev_file_path)?;

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
                let (tx, rx) = async_channel::bounded(32); // Use tokio channel for async

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
        let base_delay = Duration::from_secs(2); // 复查退避基数
        let max_backoff = Duration::from_secs(30); // 单次复查间隔上限，保证宽限窗口内多次复查
        let mut retry_count: u32 = 0; // 仅用于退避递增与日志
        let mut offline_since: Option<std::time::Instant> = None; // 连续离线的起点
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
            let components = self
                .download(&mut processor, ctx.clone(), danmaku_client.clone(), &stream)
                .await;

            info!("initialize_components completed: {url}");

            if self.token.is_cancelled() {
                info!(url = url, "task is cancelled");
                break components;
            }
            // 检查流状态
            let check_started = std::time::Instant::now();
            let check_result = plugin.check_stream(live_request(ctx.worker())).await;
            let check_elapsed = check_started.elapsed();
            match check_result {
                Ok(LiveStatus::Live {
                    stream: next_stream,
                }) => {
                    cookie_health::record_success(platform, cookie_webhook.as_deref());
                    stream = *next_stream;
                    ctx.worker().set_recording_quality(stream.recording_quality.clone());
                    // 流恢复：重置下播计时，继续录进同一会话（不投稿、不分稿件）
                    offline_since = None;
                    retry_count = 0;
                    info!(url = url, check_elapsed = ?check_elapsed, "Stream is still live, continuing same session");
                }
                Ok(LiveStatus::Offline) => {
                    // 下播是一次成功的检查（cookie 正常）
                    cookie_health::record_success(platform, cookie_webhook.as_deref());
                    let since = *offline_since.get_or_insert_with(std::time::Instant::now);
                    retry_count += 1;
                    if since.elapsed() >= grace {
                        info!(
                            url = url,
                            check_elapsed = ?check_elapsed,
                            "连续离线超过宽限期 {:?}，确认下播，结束本场", grace
                        );
                        break components;
                    }
                    info!(
                        url = url,
                        check_elapsed = ?check_elapsed,
                        "Stream went offline，宽限期内继续复查 ({:?}/{:?})",
                        since.elapsed(),
                        grace
                    );
                }
                Err(e) => {
                    // 检查出错 = cookie 可能失效（去抖后累计，达阈值才提示）
                    cookie_health::record_error(
                        platform,
                        &format!("{e:?}"),
                        cookie_webhook.as_deref(),
                    );
                    let since = *offline_since.get_or_insert_with(std::time::Instant::now);
                    retry_count += 1;
                    if since.elapsed() >= grace {
                        warn!(
                            url = url,
                            check_elapsed = ?check_elapsed,
                            "检查直播间持续失败超过宽限期 {:?}，结束本场: {:?}", grace, e
                        );
                        break components;
                    }
                    warn!(
                        url = url,
                        check_elapsed = ?check_elapsed,
                        "Failed to check stream status: {:?}，宽限期内继续复查 ({:?}/{:?})",
                        e,
                        since.elapsed(),
                        grace
                    );
                }
            }

            // 复查退避：指数增长但封顶 max_backoff，保证宽限窗口内多次复查
            let backoff = base_delay
                .saturating_mul(2_u32.saturating_pow(retry_count.min(5)))
                .min(max_backoff);
            info!("Retrying download in {:?}...", backoff);
            tokio::time::sleep(backoff).await;
        };
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
    ) -> AppResult<DownloadStatus> {
        // 获取配置和主播信息
        let streamer = ctx.live_streamer();

        // 执行下载
        // let hook = processor.create_hook(danmaku_client.clone());
        let hook = |event| {
            match event {
                SegmentEvent::Start { .. } => {
                    warn!("Ignoring unexpected segment start event");
                }
                SegmentEvent::Segment(mut event) => {
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

        let result = self
            .downloader
            .download(Box::new(hook), ctx.download_config(stream))
            .await
            .change_context(AppError::Custom("Failed to download segment".into()))?;

        // 处理结果
        info!(url=streamer.url,result=?result, "finished downloading");
        Ok(result)
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
    ctx.worker().set_recording_quality(recording_quality.clone());

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
                    actual_disp, actual, threshold_disp, threshold,
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
