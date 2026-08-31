use crate::server::core::downloader;
use crate::server::core::downloader::{
    DownloadConfig, DownloadStatus, DownloaderType, SegmentEvent, SegmentInfo,
};
use crate::server::errors::{AppError, AppResult};
use biliup::downloader::util::{
    SegmentCloseReason, SegmentIdentity, allocate_segment_id, segment_close_failed, segment_closed,
    segment_created,
};
use biliup_observability::DiagnosticCapture;
use error_stack::{ResultExt, bail};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::info;

/// FFmpeg下载器实现
/// 使用FFmpeg进行直播流下载，支持内部和外部分段
pub struct FfmpegDownloader {
    /// 进程句柄
    process_handle: Arc<RwLock<Option<tokio::process::Child>>>,

    /// 额外的FFmpeg参数
    pub extra_args: Vec<String>,

    /// 下载器类型
    pub downloader_type: DownloaderType,

    /// `stop()` 请求过取消。外部进程被信号结束时没有退出码，只有这个标记能把「主动取消」
    /// 和「进程异常死亡」区分开，不靠猜测把取消写成失败。
    cancelled: Arc<AtomicBool>,
}

impl FfmpegDownloader {
    /// 创建新的FFmpeg下载器实例
    ///
    /// # 参数
    /// * `url` - 流URL
    /// * `config` - 下载配置
    /// * `extra_args` - 额外的FFmpeg参数
    /// * `downloader_type` - 下载器类型
    pub fn new(extra_args: Vec<String>, downloader_type: DownloaderType) -> Self {
        Self {
            process_handle: Arc::new(RwLock::new(None)),
            extra_args,
            downloader_type,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 构建内部分段模式的FFmpeg命令参数
    /// 使用FFmpeg的segment muxer进行自动分段
    fn build_ffmpeg_args_internal_segment(&self, download_config: &DownloadConfig) -> Vec<String> {
        let mut args = Vec::new();

        // 内部分段使用info级别日志以获取分段信息
        args.extend(["-loglevel".to_string(), "info".to_string()]);

        // 添加通用输入参数
        self.append_common_input_args(&mut args, download_config);

        // 内部分段特定的输出参数
        // -f segment: 使用segment muxer进行自动分段
        args.extend(["-f".to_string(), "segment".to_string()]);
        args.extend([
            "-segment_format".to_string(),
            download_config.suffix.to_string(),
        ]);
        // -segment_list pipe:1: 将分段文件名输出到stdout
        // 这样我们可以实时获取新生成的分段文件
        args.extend(["-segment_list".to_string(), "pipe:1".to_string()]);
        args.extend(["-map".to_string(), "0".to_string()]);

        // -segment_list_type flat: 输出格式为纯文件名列表
        args.extend(["-segment_list_type".to_string(), "flat".to_string()]);

        // -reset_timestamps 1: 每个分段重置时间戳从0开始
        // 确保每个分段文件可以独立播放
        args.extend(["-reset_timestamps".to_string(), "1".to_string()]);
        // %Y-%m-%dT%H_%M_%S 是 strftime 的时间占位符（需要配合 -strftime 1）
        // %d 是序号占位符（printf 风格，默认模式）
        // segment 复用器不能同时用这两种
        args.extend(["-strftime".to_string(), "1".to_string()]);

        // -segment_time: 分段时长（秒）
        if let Some(segment_time) = &download_config.segment_time {
            let seconds = downloader::parse_duration(segment_time);
            args.extend(["-segment_time".to_string(), seconds.to_string()]);
        }

        // 添加通用输出参数
        self.append_common_output_args(&mut args, "segment");

        args
    }

    /// 构建外部分段模式的FFmpeg命令参数
    /// 通过外部控制进行分段，每次录制固定时长或大小
    fn build_ffmpeg_args_external_segment(&self, download_config: &DownloadConfig) -> Vec<String> {
        let mut args = Vec::new();

        // 外部分段只保留错误级日志：quiet 时 ffmpeg 连失败原因都不输出，退出诊断就只剩
        // 一个退出码。这里放宽到 error，旧输出因此多出 ffmpeg 自己的错误行，不含其它级别。
        args.extend(["-loglevel".to_string(), "error".to_string()]);

        // 添加通用输入参数
        self.append_common_input_args(&mut args, download_config);

        // 外部分段特定的输出参数
        // -to: 限制录制时长
        if let Some(segment_time) = &download_config.segment_time {
            args.extend(["-to".to_string(), segment_time.clone()]);
        }

        // -fs: 限制文件大小（字节）
        if let Some(file_size) = download_config.file_size {
            args.extend(["-fs".to_string(), file_size.to_string()]);
        }

        // 添加通用输出参数
        self.append_common_output_args(&mut args, &download_config.suffix);

        args
    }

    /// 添加通用的输入参数
    /// 包括覆盖文件、HTTP头、超时设置等
    fn append_common_input_args(&self, args: &mut Vec<String>, download_config: &DownloadConfig) {
        args.push("-y".to_string()); // 覆盖已存在文件

        // HTTP headers
        // -headers: 设置HTTP请求头，格式为"Key: Value\r\n"
        // 用于传递User-Agent、Cookie等信息
        if !download_config.headers.is_empty() {
            let headers_str = download_config
                .headers
                .iter()
                .map(|(k, v)| format!("{}: {}\r\n", k, v))
                .collect::<String>();
            args.extend(["-headers".to_string(), headers_str]);
        }

        // -rw_timeout: 读写超时时间（微秒）
        // 防止网络卡顿导致无限等待
        args.extend(["-rw_timeout".to_string(), "20000000".to_string()]);

        // 对于m3u8流的特殊处理
        if download_config.url.contains(".m3u8") {
            // -max_reload: HLS播放列表最大重载次数
            // 对于直播流需要设置较大值以持续获取新片段
            args.extend(["-max_reload".to_string(), "1000".to_string()]);
        }

        // 输入URL
        args.extend(["-i".to_string(), download_config.url.clone()]);
    }

    /// 添加通用的输出参数
    /// 包括编码设置、格式特定参数等
    fn append_common_output_args(&self, args: &mut Vec<String>, format: &str) {
        // -c copy: 直接复制编码，不重新编码
        // 减少CPU使用，保持原始质量
        args.extend(["-c".to_string(), "copy".to_string()]);

        // 格式特定参数
        match format {
            "mp4" => {
                // -bsf:a aac_adtstoasc: 音频比特流过滤器
                // 将ADTS格式的AAC转换为MP4容器所需的格式
                args.extend(["-bsf:a".to_string(), "aac_adtstoasc".to_string()]);

                // -movflags +faststart: 优化MP4用于流媒体播放
                // 将moov atom移到文件开头，允许边下载边播放
                args.extend(["-movflags".to_string(), "+faststart".to_string()]);

                args.extend(["-f".to_string(), "mp4".to_string()]);
            }
            "ts" => {
                args.extend(["-f".to_string(), "mpegts".to_string()]);
            }
            "mkv" => {
                args.extend(["-f".to_string(), "matroska".to_string()]);
            }
            "flv" => {
                args.extend(["-f".to_string(), "flv".to_string()]);
            }
            _ => {}
        }

        // 添加额外参数
        args.extend(self.extra_args.clone());
    }

    /// 执行外部分段下载
    /// 每次录制一个完整的分段文件
    async fn download_external<'a>(
        &self,
        mut callback: Box<dyn FnMut(SegmentEvent) + Send + Sync + 'a>,
        download_config: DownloadConfig,
    ) -> AppResult<DownloadStatus> {
        let args = self.build_ffmpeg_args_external_segment(&download_config);
        let output_file = download_config.generate_output_filename(&download_config.suffix);
        let owner = download_config
            .owner
            .owner(download_config.attempt_id.as_deref());
        // 目标文件由本进程选定，因此创建时刻是真实观测到的：身份在 ffmpeg 开始写入之前分配，
        // 之后的关闭、登记和上传都用同一个 segment_id。
        let identity = SegmentIdentity {
            segment_id: allocate_segment_id(),
            original_file: output_file.display().to_string(),
        };
        segment_created(&owner, &identity);

        let mut cmd = Command::new("ffmpeg");
        cmd.args(&args)
            .arg(format!("{}.part", output_file.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let child = cmd.spawn().change_context(AppError::Unknown)?;

        let (status, diagnostic) = spawn_log(child, &self.process_handle).await?;
        self.report_command_failure("ffmpeg_external", &download_config, &status, diagnostic);
        // 退出时，重命名文件
        let part_file = format!("{}.part", output_file.display());
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
        let close_reason = self.external_close_reason(&download_config, &status);
        segment_closed(
            &owner,
            &identity,
            close_reason,
            file_size(&output_file).await,
        );

        callback(SegmentEvent::Segment(SegmentInfo {
            prev_file_path: output_file,
            danmaku_file_path: None,
            segment_index: 0,
            next_file_path: None,
            close_reason,
            attempt_id: download_config.attempt_id.clone(),
            segment_id: Some(identity.segment_id),
            recovery_source_paths: Vec::new(),
            enrollment: None,
        }));
        // 根据退出码判断状态
        match status.code() {
            Some(0) => Ok(DownloadStatus::SegmentCompleted),
            Some(255) => Ok(DownloadStatus::StreamEnded),
            err => Ok(DownloadStatus::Error(format!("FFmpeg error: {err:?}"))),
        }
    }

    /// 执行内部分段下载
    /// 使用FFmpeg的segment muxer自动分段
    async fn download_internal<'a>(
        &self,
        mut callback: Box<dyn FnMut(SegmentEvent) + Send + Sync + 'a>,
        download_config: DownloadConfig,
    ) -> AppResult<DownloadStatus> {
        let args = self.build_ffmpeg_args_internal_segment(&download_config);
        let owner = download_config
            .owner
            .owner(download_config.attempt_id.as_deref());
        let template = download_config.output_dir.join(format!(
            "{}.{}.part",
            download_config.recorder.filename_template(),
            download_config.suffix
        ));

        let mut cmd = Command::new("ffmpeg");
        cmd.args(&args)
            .arg(template.display().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        info!("FFmpeg cmd: {:?}", cmd);
        let mut child = cmd.spawn().change_context(AppError::Unknown)?;

        // 获取stdout用于读取分段文件名
        let stdout = child
            .stdout
            .take()
            .ok_or(AppError::Custom("Failed to capture stdout".to_string()))?;

        // 异步读取stdout
        let mut reader = BufReader::new(stdout).lines();
        let mut segment_index = 0;
        let close_reason = internal_close_reason(&download_config);
        // 分段文件名只有秒级精度：同一秒内关闭的两段会拿到同一个名字，ffmpeg 直接覆盖，
        // 分段列表里也就出现重复行。记住本次已交付的目标名，重复行如实记为收尾失败。
        let mut delivered: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

        while let Some(line) = reader.next_line().await.change_context(AppError::Unknown)? {
            // 分段列表写的是相对列表文件的名字，管道输出时就只剩 basename，
            // 因此按配置的输出目录还原；行本身给出绝对路径时 join 保持原样。
            let file_path = download_config.output_dir.join(line.trim());

            // segment 复用器先关闭分段文件、再写这一行，所以拿到行时文件已经写完，
            // 不需要额外等待；分段身份也只能在这一刻分配，进程外看不到创建时刻。
            let no_ext = file_path.with_extension("");
            let identity = SegmentIdentity {
                segment_id: allocate_segment_id(),
                original_file: no_ext.display().to_string(),
            };

            if !delivered.insert(no_ext.clone()) {
                segment_close_failed(
                    &owner,
                    &identity.segment_id,
                    &identity.original_file,
                    "ffmpeg 重复使用了同一个分段文件名，同名的前一段可能已被覆盖",
                );
                continue;
            }

            // 重命名文件。单个分段收不了尾不应结束整场录制：如实记一次失败的关闭，
            // 临时文件原样保留交给补扫，循环继续处理后面的分段。
            if let Err(error) = tokio::fs::rename(&file_path, &no_ext).await {
                segment_close_failed(
                    &owner,
                    &identity.segment_id,
                    &identity.original_file,
                    &format!("{error}"),
                );
                continue;
            }
            info!("renamed file: from {file_path:?} to {no_ext:?}");
            segment_closed(&owner, &identity, close_reason, file_size(&no_ext).await);

            // 触发分段回调
            callback(SegmentEvent::Segment(SegmentInfo {
                prev_file_path: no_ext,
                danmaku_file_path: None,
                next_file_path: None,
                segment_index,
                close_reason,
                attempt_id: download_config.attempt_id.clone(),
                segment_id: Some(identity.segment_id),
                recovery_source_paths: Vec::new(),
                enrollment: None,
                // start_time: std::time::SystemTime::now(),
                // end_time: std::time::SystemTime::now(),
            }));

            segment_index += 1;
        }
        let (status, diagnostic) = spawn_log(child, &self.process_handle).await?;
        self.report_command_failure("ffmpeg_internal", &download_config, &status, diagnostic);

        // 根据退出码判断状态
        match status.code() {
            Some(0) => {
                // 正常退出
                Ok(DownloadStatus::SegmentCompleted)
            }
            Some(255) => Ok(DownloadStatus::StreamEnded),
            err => Ok(DownloadStatus::Error(format!("FFmpeg error: {err:?}"))),
        }
    }

    /// 外部分段只有一个文件，关闭原因就是本次进程的结束原因。取消优先于退出码：被信号
    /// 结束的进程没有退出码，不能因此写成传输失败。
    fn external_close_reason(
        &self,
        download_config: &DownloadConfig,
        status: &ExitStatus,
    ) -> SegmentCloseReason {
        if self.cancelled.load(Ordering::Relaxed) {
            return SegmentCloseReason::Cancelled;
        }
        match status.code() {
            // 退出码 0 只说明 ffmpeg 认为本次输出正常收尾；配置了时长/大小上限时按切片记，
            // 这与旧的 SegmentCompleted 判定一致，但区分不出「刚好同时下播」。
            Some(0) if download_config.segment_time.is_some() => SegmentCloseReason::TimedSplit,
            Some(0) if download_config.file_size.is_some() => SegmentCloseReason::SizeSplit,
            Some(0) | Some(255) => SegmentCloseReason::StreamEnded,
            Some(_) => SegmentCloseReason::TransportError,
            None => SegmentCloseReason::Unknown,
        }
    }

    /// 非零且非 255 的退出码才是外部命令失败。取消是预期结束，不记诊断。
    fn report_command_failure(
        &self,
        stage: &str,
        download_config: &DownloadConfig,
        status: &ExitStatus,
        diagnostic: biliup_observability::Diagnostic,
    ) {
        if self.cancelled.load(Ordering::Relaxed) || matches!(status.code(), Some(0) | Some(255)) {
            return;
        }
        crate::observe::external::command_failed(
            stage,
            "process_failed",
            download_config
                .owner
                .context(download_config.attempt_id.as_deref()),
            Some(diagnostic),
            status.code(),
        );
    }
}

/// 内部分段由 segment 复用器自己切片；配置了时长上限时每一行都是一次切片关闭。最后一个
/// 分段实际上是流结束时关闭的，但拿到列表行时无法与切片区分，整场结束原因由
/// `DownloadStatus` 和上层的 `recording.stopped` 说明，这里不猜。
fn internal_close_reason(download_config: &DownloadConfig) -> SegmentCloseReason {
    if download_config.segment_time.is_some() {
        SegmentCloseReason::TimedSplit
    } else {
        SegmentCloseReason::Unknown
    }
}

async fn file_size(path: &Path) -> u64 {
    tokio::fs::metadata(path)
        .await
        .map(|m| m.len())
        .unwrap_or(0)
}

impl FfmpegDownloader {
    pub(crate) async fn download<'a>(
        &self,
        callback: Box<dyn FnMut(SegmentEvent) + Send + Sync + 'a>,
        download_config: DownloadConfig,
    ) -> AppResult<DownloadStatus> {
        // 同一个实例可以被上层复用做下一次连接，取消标记只属于本次下载。
        self.cancelled.store(false, Ordering::Relaxed);
        match self.downloader_type {
            DownloaderType::FfmpegExternal => self
                .download_external(callback, download_config)
                .await
                .change_context(AppError::Unknown),
            DownloaderType::FfmpegInternal => self
                .download_internal(callback, download_config)
                .await
                .change_context(AppError::Unknown),
            _ => bail!(AppError::Custom("Unsupported downloader type".to_string())),
        }
    }

    pub(crate) async fn stop(&self) -> AppResult<()> {
        // 先写取消原因再动进程，避免下载侧看到没有退出码的死亡进程时误判为传输失败。
        self.cancelled.store(true, Ordering::Relaxed);
        let mut handle = self.process_handle.write().await;
        if let Some(child) = &mut *handle {
            child.kill().await.change_context(AppError::Unknown)?;
            Ok(())
        } else {
            Err(AppError::Custom("Process handle not found".to_string()).into())
        }
    }

    // async fn get_status(&self) -> DownloadStatus {
    //     self.status.read().await.clone()
    // }
}

/// 返回退出状态和有界的 stderr 诊断。旧的逐行 INFO 输出原样保留，采集只是并行地留下
/// 首个致命行与有界尾部，不改变旧 sink 看到的内容。
async fn spawn_log(
    mut child: tokio::process::Child,
    process_handle: &RwLock<Option<tokio::process::Child>>,
) -> AppResult<(ExitStatus, biliup_observability::Diagnostic)> {
    let stderr = child.stderr.take().ok_or(AppError::Custom(
        "failed to capture stderr pipe".to_string(),
    ))?;

    // 保存进程句柄
    {
        let mut handle = process_handle.write().await;
        *handle = Some(child);
    }

    let mut stderr_lines = BufReader::new(stderr).lines();
    // 将 stderr 打印到当前进程的 stderr
    let stderr_task = tokio::spawn(async move {
        let mut capture = DiagnosticCapture::new();
        while let Ok(Some(line)) = stderr_lines.next_line().await {
            info!("[ffmpeg] {line}");
            capture.push(line.as_bytes());
            capture.push(b"\n");
        }
        capture
    });

    // 确保读任务结束（忽略它们的返回错误以避免因提前关闭管道导致的 join 错）
    let capture = stderr_task.await.unwrap_or_default();

    // 等待进程结束
    let status = {
        let mut handle = process_handle.write().await;
        if let Some(mut child) = handle.take() {
            child.wait().await.change_context(AppError::Unknown)?
        } else {
            bail!(AppError::Custom("Process handle not found".to_string()));
        }
    };
    Ok((status, capture.finish(status.code())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::RecordingIdentity;
    use crate::server::common::util::Recorder;
    use crate::server::infrastructure::models::StreamerInfo;
    use biliup::downloader::util::close_reason_code;
    use biliup_observability::{
        CaptureKind, CaptureLayer, Commit, Consumer, Event, Options, Runtime, StorageError,
    };
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tracing_subscriber::layer::SubscriberExt;

    struct Memory(Arc<Mutex<Vec<Event>>>);
    impl Consumer for Memory {
        fn write(&mut self, batch: &[Event]) -> Result<Commit, StorageError> {
            self.0.lock().unwrap().extend_from_slice(batch);
            Ok(Commit::default())
        }
    }

    /// 本批只验证外部下载器自己的边界，媒体全部本地合成，不接触任何真实平台。
    fn synthetic_source(dir: &Path) -> PathBuf {
        let path = dir.join("source.ts");
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=160x120:rate=10",
                "-t",
                "6",
                "-c:v",
                "libx264",
                // 短 GOP 才有足够的关键帧让 segment 复用器按秒切片。
                "-g",
                "10",
                "-f",
                "mpegts",
            ])
            .arg(&path)
            .status()
            .expect("ffmpeg 不可用，无法合成受控媒体");
        assert!(status.success());
        path
    }

    /// 按块限速回放合成媒体的最小 HTTP/1.0 服务：不带 Content-Length，由连接关闭定界，
    /// 这样 ffmpeg 的读取节奏由本服务控制，秒级文件名不会互相覆盖。
    async fn paced_origin(body: Vec<u8>, chunk: usize, delay: Duration) -> (String, u16) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let body = body.clone();
                tokio::spawn(async move {
                    let mut request = [0u8; 2048];
                    let _ = socket.read(&mut request).await;
                    if socket
                        .write_all(b"HTTP/1.0 200 OK\r\nContent-Type: video/mp2t\r\n\r\n")
                        .await
                        .is_err()
                    {
                        return;
                    }
                    for part in body.chunks(chunk.max(1)) {
                        if socket.write_all(part).await.is_err() {
                            return;
                        }
                        tokio::time::sleep(delay).await;
                    }
                });
            }
        });
        (format!("http://127.0.0.1:{port}/live.ts"), port)
    }

    fn config(url: String, dir: &Path, suffix: &str, segment_time: Option<&str>) -> DownloadConfig {
        DownloadConfig {
            url,
            stream_candidates: Vec::new(),
            segment_time: segment_time.map(ToOwned::to_owned),
            file_size: None,
            headers: Default::default(),
            recorder: Recorder::new(
                Some("ffmpeg-%Y%m%dT%H%M%S".into()),
                StreamerInfo::new(
                    "受控主播",
                    "http://127.0.0.1/controlled",
                    "受控标题",
                    chrono::Utc::now(),
                    "",
                ),
            ),
            output_dir: dir.to_path_buf(),
            suffix: suffix.into(),
            owner: RecordingIdentity::server(7, 42, "受控主播"),
            reconnect: None,
            attempt_id: Some("attempt-controlled-ffmpeg".into()),
            quality: None,
            stall_timeout_secs: None,
        }
    }

    fn native<'a>(events: &'a [Event], name: &str) -> Vec<&'a Event> {
        events
            .iter()
            .filter(|e| e.data().capture_kind == CaptureKind::Native && e.data().event_name == name)
            .collect()
    }

    fn field(event: &Event, key: &str) -> String {
        event
            .data()
            .fields
            .get(key)
            .map(|v| v.as_str().map(ToOwned::to_owned).unwrap_or(v.to_string()))
            .unwrap_or_default()
    }

    /// 外部分段：目标文件由本进程选定，创建与关闭都是真实观测；关闭原因跟随本次进程的
    /// 结束方式，取消不写成失败，退出码正常时也不产生外部命令诊断。
    #[tokio::test]
    async fn external_segment_carries_observed_identity_and_close_reason() {
        let directory = tempfile::tempdir().unwrap();
        let media = std::fs::read(synthetic_source(directory.path())).unwrap();

        for (segment_time, expected) in [(Some("00:00:02"), "split_limit"), (None, "stream_end")] {
            let collected = Arc::new(Mutex::new(Vec::<Event>::new()));
            let sink = collected.clone();
            let mut runtime = Runtime::start(
                "synthetic",
                "test",
                Options {
                    enabled: true,
                    ..Options::default()
                },
                move || Ok(Memory(sink.clone())),
            )
            .unwrap();
            let _guard = tracing::subscriber::set_default(
                tracing_subscriber::registry()
                    .with(CaptureLayer::new(runtime.emitter()).filtered()),
            );
            let (url, _) = paced_origin(media.clone(), 8192, Duration::from_millis(60)).await;
            let downloader = FfmpegDownloader::new(Vec::new(), DownloaderType::FfmpegExternal);
            let segments = Arc::new(Mutex::new(Vec::new()));
            let hook = segments.clone();
            let status = tokio::time::timeout(
                Duration::from_secs(30),
                downloader.download(
                    Box::new(move |event| {
                        if let SegmentEvent::Segment(info) = event {
                            hook.lock().unwrap().push(info);
                        }
                    }),
                    config(url, directory.path(), "ts", segment_time),
                ),
            )
            .await
            .expect("外部分段下载超时")
            .unwrap();
            assert_eq!(status, DownloadStatus::SegmentCompleted);
            assert!(runtime.shutdown(Duration::from_secs(2)).closed);

            let events = collected.lock().unwrap();
            let created = native(&events, "recording.segment_created");
            let closed = native(&events, "recording.segment_closed");
            assert_eq!(created.len(), 1);
            assert_eq!(closed.len(), 1);
            // 同一个身份贯穿创建、关闭和交给上层的分段信息，没有第二次分配。
            let segment_id = field(created[0], "segment_id");
            assert!(!segment_id.is_empty());
            assert_eq!(field(closed[0], "segment_id"), segment_id);
            assert_eq!(field(closed[0], "reason_code"), expected);
            assert_eq!(field(closed[0], "outcome"), "executed");
            assert_eq!(field(closed[0], "live_streamer_id"), "7");
            assert_eq!(
                field(closed[0], "download_attempt_id"),
                "attempt-controlled-ffmpeg"
            );
            assert!(
                closed[0]
                    .data()
                    .fields
                    .get("size_bytes")
                    .and_then(|v| v.as_u64())
                    .is_some_and(|n| n > 0)
            );
            // 路径只留脱敏后的 basename，落盘位置由 output_dir 决定。
            assert!(!field(closed[0], "original_file").contains('/'));
            assert!(native(&events, "processing.command_failed").is_empty());

            let segments = segments.lock().unwrap();
            assert_eq!(segments.len(), 1);
            assert_eq!(segments[0].segment_id.as_deref(), Some(segment_id.as_str()));
            assert_eq!(close_reason_code(segments[0].close_reason), expected);
            assert!(segments[0].prev_file_path.starts_with(directory.path()));
            assert!(segments[0].prev_file_path.exists());
            for segment in segments.iter() {
                let _ = std::fs::remove_file(&segment.prev_file_path);
            }
        }
    }

    /// 内部分段：segment 复用器先关闭分段再写列表行，所以每一行恰好对应一次关闭。
    ///
    /// 这里同时回归两个缺陷：一是原先「循环里改名、循环后又对同一个 .part 改名」，最后一段
    /// 必然重复回调且整次下载返回错误；二是 ffmpeg 的秒级文件名撞车时，第二次改名 ENOENT
    /// 会直接结束整场录制。修复后单段收不了尾只记一次失败的关闭，下载继续。
    #[tokio::test]
    async fn internal_segments_close_once_each_and_keep_distinct_identity() {
        let directory = tempfile::tempdir().unwrap();
        let media = std::fs::read(synthetic_source(directory.path())).unwrap();
        let collected = Arc::new(Mutex::new(Vec::<Event>::new()));
        let sink = collected.clone();
        let mut runtime = Runtime::start(
            "synthetic",
            "test",
            Options {
                enabled: true,
                // 打开桥接才能确认旧的逐行 ffmpeg stderr 输出没有被有界采集取代。
                bridge: true,
                ..Options::default()
            },
            move || Ok(Memory(sink.clone())),
        )
        .unwrap();
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::registry().with(CaptureLayer::new(runtime.emitter()).filtered()),
        );
        // 至少按真实速率回放：分段文件名只有秒级精度，喂得比实时快会让相邻分段落在同一秒，
        // ffmpeg 直接覆盖前一个文件并写出重复的列表行（见回执记录的既有命名缺陷）。
        let (url, _) = paced_origin(media, 2048, Duration::from_millis(250)).await;
        let downloader = FfmpegDownloader::new(Vec::new(), DownloaderType::FfmpegInternal);
        let segments = Arc::new(Mutex::new(Vec::new()));
        let hook = segments.clone();
        let status = tokio::time::timeout(
            Duration::from_secs(30),
            downloader.download(
                Box::new(move |event| {
                    if let SegmentEvent::Segment(info) = event {
                        hook.lock().unwrap().push(info);
                    }
                }),
                // flv 是 segment 复用器认得的容器名；ffmpeg 9 不接受 "ts" 作为 -segment_format。
                config(url, directory.path(), "flv", Some("00:00:02")),
            ),
        )
        .await
        .expect("内部分段下载超时")
        .unwrap();
        assert_eq!(status, DownloadStatus::SegmentCompleted);
        assert!(runtime.shutdown(Duration::from_secs(2)).closed);

        let segments = segments.lock().unwrap();
        let events = collected.lock().unwrap();
        let closed: Vec<_> = native(&events, "recording.segment_closed")
            .into_iter()
            .filter(|e| field(e, "outcome") == "executed")
            .collect();
        // 秒级文件名是否撞车由 ffmpeg 的输出节奏决定，测不稳；这里断言与之无关的不变量。
        assert!(!segments.is_empty(), "受控源至少应交付一个分段");
        assert_eq!(closed.len(), segments.len());
        for failed in native(&events, "recording.segment_closed")
            .into_iter()
            .filter(|e| field(e, "outcome") == "failed")
        {
            // 收尾失败只影响那一段：原因如实记录，整场下载仍然正常结束。
            assert_eq!(field(failed, "reason_code"), "unknown");
            assert!(!field(failed, "segment_id").is_empty());
        }
        // 外部进程创建文件，进程外看不到创建时刻；这里不伪造 segment_created。
        assert!(native(&events, "recording.segment_created").is_empty());

        let mut ids: Vec<String> = Vec::new();
        for (index, segment) in segments.iter().enumerate() {
            let id = segment.segment_id.clone().expect("分段必须带身份");
            assert_eq!(field(closed[index], "segment_id"), id);
            assert_eq!(field(closed[index], "reason_code"), "split_limit");
            assert_eq!(segment.segment_index, index);
            assert!(segment.prev_file_path.exists());
            assert!(segment.prev_file_path.starts_with(directory.path()));
            ids.push(id);
        }
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), segments.len(), "每个分段各有独立身份");
        let mut paths: Vec<_> = segments.iter().map(|s| s.prev_file_path.clone()).collect();
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), segments.len(), "同一个文件不会被交付两次");
        assert!(
            events
                .iter()
                .any(|e| e.data().capture_kind == CaptureKind::LegacyBridge
                    && e.data().message.contains("[ffmpeg]")),
            "旧的逐行 stderr 输出必须保留"
        );
    }

    /// 外部命令失败：退出码与有界 stderr 尾部作为附件保存，凭据线索整值脱敏，
    /// 事件本身不携带第三方输出。
    #[tokio::test]
    async fn failed_command_reports_bounded_diagnostic_without_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let collected = Arc::new(Mutex::new(Vec::<Event>::new()));
        let sink = collected.clone();
        let mut runtime = Runtime::start(
            "synthetic",
            "test",
            Options {
                enabled: true,
                ..Options::default()
            },
            move || Ok(Memory(sink.clone())),
        )
        .unwrap();
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::registry().with(CaptureLayer::new(runtime.emitter()).filtered()),
        );
        // 关闭的端口：ffmpeg 打不开输入，以非零、非 255 的退出码结束。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let url = format!("http://127.0.0.1:{port}/live.ts?token=secret-value-should-not-appear");
        let downloader = FfmpegDownloader::new(Vec::new(), DownloaderType::FfmpegExternal);
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            downloader.download(Box::new(|_| {}), config(url, directory.path(), "ts", None)),
        )
        .await
        .expect("失败路径不应挂起");
        // 输入打不开时没有产物，重命名失败是既有行为；这里关心的是诊断本身。
        assert!(result.is_err());
        assert!(runtime.shutdown(Duration::from_secs(2)).closed);

        let events = collected.lock().unwrap();
        let failed = native(&events, "processing.command_failed");
        assert_eq!(failed.len(), 1);
        assert_eq!(field(failed[0], "stage"), "ffmpeg_external");
        assert_eq!(field(failed[0], "outcome"), "failed");
        assert_eq!(field(failed[0], "reason_code"), "process_failed");
        assert_eq!(field(failed[0], "live_streamer_id"), "7");
        let exit = failed[0].data().fields.get("exit_code").unwrap().as_i64();
        assert!(exit.is_some_and(|code| code != 0 && code != 255));
        let diagnostic = failed[0].diagnostic().expect("失败必须带有界诊断");
        assert!(diagnostic.total_bytes() > 0);
        assert!(diagnostic.first_fatal().is_some());
        assert!(!diagnostic.tail().contains("secret-value-should-not-appear"));
        assert!(diagnostic.tail().contains("[REDACTED]"));
        // 诊断正文只在附件里；事件字段不复制第三方输出，列表查询也就看不到它。
        let fatal = diagnostic.first_fatal().unwrap().to_owned();
        for (key, value) in failed[0].data().fields.iter() {
            assert!(
                !value.to_string().contains(&fatal),
                "字段 {key} 不应携带外部命令输出"
            );
        }
        // 关闭失败如实记为 failed，不冒充一个已关闭的分段。
        let closed = native(&events, "recording.segment_closed");
        assert_eq!(closed.len(), 1);
        assert_eq!(field(closed[0], "outcome"), "failed");
    }

    /// 主动取消：进程被信号结束时没有退出码，关闭原因是 user_cancel，且不记外部命令失败。
    #[tokio::test]
    async fn cancelled_download_is_not_reported_as_a_failure() {
        let directory = tempfile::tempdir().unwrap();
        let media = std::fs::read(synthetic_source(directory.path())).unwrap();
        let collected = Arc::new(Mutex::new(Vec::<Event>::new()));
        let sink = collected.clone();
        let mut runtime = Runtime::start(
            "synthetic",
            "test",
            Options {
                enabled: true,
                ..Options::default()
            },
            move || Ok(Memory(sink.clone())),
        )
        .unwrap();
        let _guard = tracing::subscriber::set_default(
            tracing_subscriber::registry().with(CaptureLayer::new(runtime.emitter()).filtered()),
        );
        // 慢速回放，取消时进程一定还活着。
        let (url, _) = paced_origin(media, 2048, Duration::from_millis(120)).await;
        let downloader = FfmpegDownloader::new(Vec::new(), DownloaderType::FfmpegExternal);
        let segments = Arc::new(Mutex::new(Vec::new()));
        let hook = segments.clone();
        let download = downloader.download(
            Box::new(move |event| {
                if let SegmentEvent::Segment(info) = event {
                    hook.lock().unwrap().push(info);
                }
            }),
            config(url, directory.path(), "ts", None),
        );
        let cancel = async {
            // 等到临时文件出现再取消，确保测到的是「进程活着时被取消」；ffmpeg 何时把
            // 缓冲刷到盘上不由本测试决定，所以只等文件存在，不等字节数。
            for _ in 0..200 {
                let started = std::fs::read_dir(directory.path())
                    .unwrap()
                    .filter_map(|e| e.ok())
                    .any(|e| e.path().extension().is_some_and(|ext| ext == "part"));
                if started {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            downloader.stop().await.unwrap();
        };
        let (result, _) = tokio::time::timeout(Duration::from_secs(30), async {
            tokio::join!(download, cancel)
        })
        .await
        .expect("取消路径不应挂起");
        result.unwrap();
        assert!(runtime.shutdown(Duration::from_secs(2)).closed);

        let events = collected.lock().unwrap();
        let closed = native(&events, "recording.segment_closed");
        assert_eq!(closed.len(), 1);
        assert_eq!(field(closed[0], "reason_code"), "user_cancel");
        assert!(
            native(&events, "processing.command_failed").is_empty(),
            "取消不是外部命令失败"
        );
        let segments = segments.lock().unwrap();
        assert_eq!(segments.len(), 1);
        assert_eq!(close_reason_code(segments[0].close_reason), "user_cancel");
        for segment in segments.iter() {
            let _ = std::fs::remove_file(&segment.prev_file_path);
        }
    }
}
