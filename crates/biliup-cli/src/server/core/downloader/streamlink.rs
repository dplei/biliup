use crate::server::core::downloader::{DownloadConfig, DownloadStatus, SegmentEvent, SegmentInfo};
use crate::server::errors::{AppError, AppResult};
use biliup::downloader::util::{
    SegmentCloseReason, SegmentIdentity, allocate_segment_id, segment_close_failed, segment_closed,
    segment_created,
};
use biliup_observability::{Diagnostic, DiagnosticCapture};
use error_stack::ResultExt;
use std::collections::HashMap;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::RwLock;
use tokio::time::Duration;
use tracing::info;
use url::Url;

#[derive(Debug, Clone)]
pub enum Platform {
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

#[derive(Debug, Clone)]
pub enum OutputMode {
    /// 管道模式：streamlink输出到stdout，由父进程读取
    Pipe,
    /// HTTP服务器模式：streamlink启动本地HTTP服务器
    HttpServer { port: u16 },
}

pub struct Streamlink {
    streamlink_downloader: StreamlinkDownloader,
    /// 进程句柄
    process_handle: Arc<RwLock<Option<Child>>>,

    /// `stop()` 请求过取消。被信号结束的 streamlink 没有退出码，只有这个标记能把
    /// 「主动停止」和「进程异常死亡」分开，不靠猜测把取消写成传输失败。
    cancelled: Arc<AtomicBool>,
}

impl Streamlink {
    pub fn new(streamlink_downloader: StreamlinkDownloader) -> Streamlink {
        Self {
            streamlink_downloader,
            process_handle: Arc::new(RwLock::new(None)),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) async fn download<'a>(
        &self,
        mut callback: Box<dyn FnMut(SegmentEvent) + Send + Sync + 'a>,
        download_config: DownloadConfig,
    ) -> AppResult<DownloadStatus> {
        // 同一个实例可以被上层复用做下一次连接，取消标记只属于本次下载。
        self.cancelled.store(false, Ordering::Relaxed);
        let output_file = download_config.generate_output_filename(&download_config.suffix);
        let part_file = format!("{}.part", output_file.display());
        let args = self
            .streamlink_downloader
            .build_file_args(&download_config, &part_file)?;
        let owner = download_config
            .owner
            .owner(download_config.attempt_id.as_deref());

        let mut cmd = Command::new("streamlink");
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        info!(cmd = ?cmd, "Starting streamlink download");
        let child = cmd.spawn().change_context(AppError::Unknown)?;
        // 目标文件由本进程用 `--output` 选定，进程起来之后写入就开始了，因此创建是真实
        // 观测：身份在这里分配一次，之后的关闭和交给上层的分段信息都用同一个 segment_id。
        let identity = SegmentIdentity {
            segment_id: allocate_segment_id(),
            original_file: output_file.display().to_string(),
        };
        segment_created(&owner, &identity);
        let (status, diagnostic) = spawn_log(child, &self.process_handle).await?;
        self.report_command_failure(&download_config, &status, diagnostic);

        if tokio::fs::try_exists(&part_file)
            .await
            .change_context(AppError::Unknown)?
        {
            if let Err(error) = tokio::fs::rename(&part_file, &output_file).await {
                segment_close_failed(
                    &owner,
                    &identity.segment_id,
                    &identity.original_file,
                    &format!("{error}"),
                );
                return Err(error)
                    .change_context(AppError::Custom(String::from("退出时，重命名文件")))?;
            }
            let close_reason = self.close_reason(&download_config, &status);
            segment_closed(
                &owner,
                &identity,
                close_reason,
                file_size(&output_file).await,
            );
            callback(SegmentEvent::Segment(SegmentInfo {
                prev_file_path: output_file,
                danmaku_file_path: None,
                next_file_path: None,
                segment_index: 0,
                close_reason,
                attempt_id: download_config.attempt_id.clone(),
                segment_id: Some(identity.segment_id),
                recovery_source_paths: Vec::new(),
                enrollment: None,
            }));
        } else {
            // 已宣告开始写入却没有临时文件：streamlink 一个字节都没落盘。如实记一次失败的
            // 关闭，不冒充一个已关闭的分段，也不改变旧的返回值。
            segment_close_failed(
                &owner,
                &identity.segment_id,
                &identity.original_file,
                "streamlink 退出后没有找到分段临时文件",
            );
        }

        match status.code() {
            Some(0) => Ok(DownloadStatus::SegmentCompleted),
            Some(130) | Some(143) | Some(255) => Ok(DownloadStatus::StreamEnded),
            err => Ok(DownloadStatus::Error(format!("Streamlink error: {err:?}"))),
        }
    }

    /// 停止下载
    pub(crate) async fn stop(&self) -> AppResult<()> {
        // 先写取消原因再动进程，避免下载侧看到没有退出码的死亡进程时误判为传输失败。
        self.cancelled.store(true, Ordering::Relaxed);
        let mut handle = self.process_handle.write().await;
        if let Some(child) = &mut *handle {
            child.kill().await.change_context(AppError::Unknown)?;
        }
        Ok(())
    }

    /// 一次调用只产出一个文件，关闭原因就是本次进程的结束方式。取消优先于退出码：
    /// 被信号结束的进程没有退出码，不能因此写成传输失败。
    fn close_reason(
        &self,
        download_config: &DownloadConfig,
        status: &ExitStatus,
    ) -> SegmentCloseReason {
        if self.cancelled.load(Ordering::Relaxed) {
            return SegmentCloseReason::Cancelled;
        }
        match status.code() {
            // 配了 `--hls-duration` 时退出码 0 就是切到上限；和 ffmpeg 一样，
            // 区分不出「刚好同时下播」。
            Some(0) if download_config.segment_time.is_some() => SegmentCloseReason::TimedSplit,
            Some(0) | Some(130) | Some(143) | Some(255) => SegmentCloseReason::StreamEnded,
            Some(_) => SegmentCloseReason::TransportError,
            None => SegmentCloseReason::Unknown,
        }
    }

    /// 退出码 0 是正常收尾，130/143/255 是按请求结束；主动取消同样不是外部命令失败。
    fn should_report_failure(&self, status: &ExitStatus) -> bool {
        !self.cancelled.load(Ordering::Relaxed)
            && !matches!(status.code(), Some(0) | Some(130) | Some(143) | Some(255))
    }

    fn report_command_failure(
        &self,
        download_config: &DownloadConfig,
        status: &ExitStatus,
        diagnostic: Diagnostic,
    ) {
        if !self.should_report_failure(status) {
            return;
        }
        crate::observe::external::command_failed(
            "streamlink",
            "process_failed",
            download_config
                .owner
                .context(download_config.attempt_id.as_deref()),
            Some(diagnostic),
            status.code(),
        );
    }
}

async fn file_size(path: &Path) -> u64 {
    tokio::fs::metadata(path)
        .await
        .map(|m| m.len())
        .unwrap_or(0)
}

pub struct StreamlinkDownloader {
    platform: Platform,
    url: String,
    headers: HashMap<String, String>,
    output_mode: OutputMode,
}

impl StreamlinkDownloader {
    pub fn new(url: String, platform: Platform) -> Self {
        Self {
            platform,
            url,
            headers: HashMap::new(),
            output_mode: OutputMode::Pipe, // 默认管道模式
        }
    }

    pub fn with_headers(mut self, headers: HashMap<String, String>) -> Self {
        self.headers = headers;
        self
    }

    pub fn with_output_mode(mut self, mode: OutputMode) -> Self {
        self.output_mode = mode;
        self
    }

    fn build_base_args(&self) -> AppResult<Vec<String>> {
        let mut args = vec![
            "--stream-segment-threads".to_string(),
            "3".to_string(),
            "--hls-playlist-reload-attempts".to_string(),
            "1".to_string(),
        ];

        for (key, value) in &self.headers {
            args.push("--http-header".to_string());
            args.push(format!("{}={}", key, value));
        }

        args.extend(self.build_platform_args()?);
        Ok(args)
    }

    fn build_file_args(
        &self,
        download_config: &DownloadConfig,
        output_file: &str,
    ) -> AppResult<Vec<String>> {
        let mut args = self.build_base_args()?;
        for (key, value) in &download_config.headers {
            args.push("--http-header".to_string());
            args.push(format!("{}={}", key, value));
        }
        if let Some(segment_time) = &download_config.segment_time {
            args.push("--hls-duration".to_string());
            args.push(segment_time.clone());
        }
        args.push("--force".to_string());
        args.push("--output".to_string());
        args.push(output_file.to_string());
        args.push(self.url.clone());
        args.push("best".to_string());
        Ok(args)
    }

    /// 启动streamlink进程
    pub fn start(&mut self) -> AppResult<StreamOutput> {
        let mut cmd = Command::new("streamlink");

        cmd.args(self.build_base_args()?);

        // 配置输出模式
        let output = match &self.output_mode {
            OutputMode::Pipe => {
                cmd.args([&self.url, "best", "-O"]);
                cmd.stdout(Stdio::piped());

                let child = cmd.spawn().change_context(AppError::Unknown)?;
                StreamOutput::Pipe(child)
            }
            OutputMode::HttpServer { port } => {
                cmd.args([
                    "--player-external-http",
                    "--player-external-http-port",
                    &port.to_string(),
                    "--player-external-http-interface",
                    "localhost",
                    &self.url,
                    "best",
                ]);

                let child = cmd.spawn().change_context(AppError::Unknown)?;

                StreamOutput::Http {
                    url: format!("http://localhost:{}", port),
                    process: child,
                }
            }
        };

        Ok(output)
    }

    /// 构建平台特定参数
    fn build_platform_args(&self) -> AppResult<Vec<String>> {
        let mut args = Vec::new();

        match &self.platform {
            Platform::Bilibili => {
                // Bilibili需要保留特定URL参数，否则segment请求会404
                args.extend(self.parse_bilibili_params()?);
            }
            Platform::Twitch {
                disable_ads,
                auth_token,
            } => {
                if *disable_ads {
                    args.push("--twitch-disable-ads".to_string());
                }

                let token = auth_token.clone().or_else(Self::get_twitch_auth_token);
                if let Some(token) = token {
                    args.push(format!("--twitch-api-header=Authorization=OAuth {}", token));
                }
            }
            Platform::Niconico {
                email,
                password,
                user_session,
                purge_credentials,
            } => {
                if let Some(email) = email.as_deref().filter(|value| !value.is_empty()) {
                    args.push("--niconico-email".to_string());
                    args.push(email.to_string());
                }
                if let Some(password) = password.as_deref().filter(|value| !value.is_empty()) {
                    args.push("--niconico-password".to_string());
                    args.push(password.to_string());
                }
                if let Some(user_session) =
                    user_session.as_deref().filter(|value| !value.is_empty())
                {
                    args.push("--niconico-user-session".to_string());
                    args.push(user_session.to_string());
                }
                if let Some(purge_credentials) = purge_credentials
                    .as_deref()
                    .filter(|value| !value.is_empty())
                {
                    args.push("--niconico-purge-credentials".to_string());
                    args.push(purge_credentials.to_string());
                }
            }
            Platform::Generic => {}
        }

        Ok(args)
    }

    /// 解析Bilibili URL参数（白名单过滤）
    fn parse_bilibili_params(&self) -> AppResult<Vec<String>> {
        let mut params = Vec::new();

        let url = Url::parse(&self.url).change_context(AppError::Unknown)?;
        // 白名单参数
        let mut whitelist = vec![
            "uparams",
            "upsig",
            "sigparams",
            "sign",
            "flvsk",
            "sk",
            "mid",
            "site",
        ];

        // 动态扩展白名单
        let query_pairs: HashMap<_, _> = url.query_pairs().collect();

        if let Some(sigparams) = query_pairs.get("sigparams") {
            whitelist.extend(sigparams.split(',').map(|s| s.trim()));
        }
        if let Some(uparams) = query_pairs.get("uparams") {
            whitelist.extend(uparams.split(',').map(|s| s.trim()));
        }

        // 过滤参数
        for (key, value) in url.query_pairs() {
            if whitelist.contains(&key.as_ref()) {
                params.push("--http-query-param".to_string());
                params.push(format!("{}={}", key, value));
            }
        }

        Ok(params)
    }

    fn get_twitch_auth_token() -> Option<String> {
        // 从配置文件或环境变量读取
        std::env::var("TWITCH_AUTH_TOKEN").ok()
    }
}

/// Streamlink输出类型
pub enum StreamOutput {
    /// 管道输出（直接读取stdout）
    Pipe(Child),
    /// HTTP服务器输出
    Http { url: String, process: Child },
}

impl StreamOutput {
    /// 获取可读的输入源（用于FFmpeg等）
    pub async fn get_input_uri(&mut self) -> String {
        match self {
            StreamOutput::Pipe(_) => "pipe:0".to_string(),
            StreamOutput::Http { url, .. } => url.clone(),
        }
    }

    /// 获取stdout（仅管道模式）
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        match self {
            StreamOutput::Pipe(child) => child.stdout.take(),
            StreamOutput::Http { .. } => None,
        }
    }

    pub async fn stop(&mut self) {
        info!("准备停止stream terminated");
        let child = match self {
            StreamOutput::Pipe(c) => c,
            StreamOutput::Http { process, .. } => process,
        };

        let _ = child.kill().await; // 强制终止
        let _ = child.wait().await; // 回收资源
        info!("成功stream terminated");
    }
}

/// 返回退出状态和有界的 stderr 诊断。旧的逐行 INFO 输出原样保留，采集只是并行地留下
/// 首个致命行与有界尾部，不改变旧 sink 看到的内容。
async fn spawn_log(
    mut child: Child,
    process_handle: &RwLock<Option<Child>>,
) -> AppResult<(ExitStatus, Diagnostic)> {
    let mut stderr_task = child.stderr.take().map(|stderr| {
        let mut stderr_lines = BufReader::new(stderr).lines();
        tokio::spawn(async move {
            let mut capture = DiagnosticCapture::new();
            while let Ok(Some(line)) = stderr_lines.next_line().await {
                info!("[streamlink] {line}");
                capture.push(line.as_bytes());
                capture.push(b"\n");
            }
            capture
        })
    });

    let mut stdout_task = child.stdout.take().map(|stdout| {
        let mut stdout_lines = BufReader::new(stdout).lines();
        tokio::spawn(async move {
            while let Ok(Some(line)) = stdout_lines.next_line().await {
                info!("[streamlink] {line}");
            }
        })
    });

    {
        let mut handle = process_handle.write().await;
        *handle = Some(child);
    }

    let status = loop {
        {
            let mut handle = process_handle.write().await;
            let Some(child) = handle.as_mut() else {
                return Err(AppError::Custom("Process handle not found".to_string()).into());
            };
            if let Some(status) = child.try_wait().change_context(AppError::Unknown)? {
                *handle = None;
                break status;
            }
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    let capture = match stderr_task.take() {
        Some(task) => task.await.unwrap_or_default(),
        None => DiagnosticCapture::new(),
    };
    if let Some(task) = stdout_task.take() {
        let _ = task.await;
    }

    Ok((status, capture.finish(status.code())))
}

/// 本批不安装 streamlink，也不构造假命令：这里只验证与外部进程无关的判定口径。
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn downloader() -> Streamlink {
        Streamlink::new(StreamlinkDownloader::new(
            "http://127.0.0.1/controlled".to_string(),
            Platform::Generic,
        ))
    }

    fn exited(code: i32) -> ExitStatus {
        ExitStatus::from_raw(code << 8)
    }

    #[test]
    fn close_reason_follows_the_observed_exit() {
        let split = DownloadConfig {
            segment_time: Some("00:00:30".to_string()),
            ..Default::default()
        };
        let plain = DownloadConfig::default();
        let downloader = downloader();
        assert_eq!(
            downloader.close_reason(&split, &exited(0)),
            SegmentCloseReason::TimedSplit
        );
        assert_eq!(
            downloader.close_reason(&plain, &exited(0)),
            SegmentCloseReason::StreamEnded
        );
        assert_eq!(
            downloader.close_reason(&plain, &exited(130)),
            SegmentCloseReason::StreamEnded
        );
        assert_eq!(
            downloader.close_reason(&plain, &exited(1)),
            SegmentCloseReason::TransportError
        );
        // 被信号结束的进程没有退出码：未取消时保持 unknown，不写成传输失败。
        assert_eq!(
            downloader.close_reason(&plain, &ExitStatus::from_raw(9)),
            SegmentCloseReason::Unknown
        );
        assert!(downloader.should_report_failure(&exited(1)));
        assert!(!downloader.should_report_failure(&exited(143)));
    }

    #[test]
    fn cancellation_wins_over_the_exit_code() {
        let downloader = downloader();
        downloader.cancelled.store(true, Ordering::Relaxed);
        assert_eq!(
            downloader.close_reason(&DownloadConfig::default(), &exited(1)),
            SegmentCloseReason::Cancelled
        );
        assert!(!downloader.should_report_failure(&exited(1)));
    }
}
