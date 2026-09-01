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
use chrono::{DateTime, Local};
use error_stack::{ResultExt, bail};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::{error, info};

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
            segment_muxer_name(&download_config.suffix).to_string(),
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
        // 这里**不开** -strftime：segment 复用器要么认 strftime 时间占位符（`-strftime 1`），
        // 要么认 printf 风格的序号 `%d`，两者不能同时用。strftime 只有秒级精度，同一秒关闭的
        // 两段会拿到同一个名字并被 ffmpeg 以 O_TRUNC 覆盖，前一段的数据就没了。序号是唯一
        // 能保证「每段一个新文件」的那种，所以时间占位符改由本进程展开（见
        // `internal_segment_pattern`），ffmpeg 只负责编号。

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
        // 分段文件名先由 ffmpeg 按序号写出，保证同一秒关闭的两段也各占一个文件；用户配置的
        // 命名在交付之前由本进程展开，见 `internal_segment_pattern` / `internal_delivery_stem`。
        let mut segment_started_at = Local::now();
        let pattern = internal_segment_pattern(&download_config, segment_started_at);

        let mut cmd = Command::new("ffmpeg");
        cmd.args(&args)
            .arg(pattern.display().to_string())
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
        // 本次已经交付过的目标名。磁盘上的同名文件也算占用，但上层随时可能把交付过的文件
        // 移走，光看磁盘不足以避免第二次用同一个名字。
        let mut delivered: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

        while let Some(line) = reader.next_line().await.change_context(AppError::Unknown)? {
            // 分段列表写的是相对列表文件的名字，管道输出时就只剩 basename，
            // 因此按配置的输出目录还原；行本身给出绝对路径时 join 保持原样。
            let file_path = download_config.output_dir.join(line.trim());
            // segment 复用器先关闭这一段、写出列表行，再打开下一段：读到行的时刻既是本段的
            // 关闭时刻，也是下一段的开始时刻。第一段没有更早的观测点，用进程启动的时刻。
            let started_at = std::mem::replace(&mut segment_started_at, Local::now());

            // 交付名按用户模板展开，取本段的开始时刻，与旧的 `-strftime 1` 同口径。撞车时
            // 顺延序号：源文件是 ffmpeg 按编号写的独立文件，顺延只改这一段叫什么，
            // 不会像以前那样让一段的数据被另一段覆盖。
            let stem = internal_delivery_stem(&download_config, started_at);
            let Some(target) = unique_delivery_path(
                &download_config.output_dir,
                &stem,
                &download_config.suffix,
                &delivered,
            ) else {
                segment_close_failed(
                    &owner,
                    &allocate_segment_id(),
                    &file_path.display().to_string(),
                    "同名的交付文件过多，无法为这一段取到唯一的文件名",
                );
                continue;
            };

            // 拿到行时分段文件已经写完，不需要额外等待；分段身份也只能在这一刻分配，
            // 进程外看不到创建时刻。
            let identity = SegmentIdentity {
                segment_id: allocate_segment_id(),
                original_file: target.display().to_string(),
            };
            delivered.insert(target.clone());

            // 重命名文件。单个分段收不了尾不应结束整场录制：如实记一次失败的关闭，
            // 临时文件原样保留交给补扫，循环继续处理后面的分段。
            if let Err(error) = tokio::fs::rename(&file_path, &target).await {
                segment_close_failed(
                    &owner,
                    &identity.segment_id,
                    &identity.original_file,
                    &format!("{error}"),
                );
                continue;
            }
            info!("renamed file: from {file_path:?} to {target:?}");
            segment_closed(&owner, &identity, close_reason, file_size(&target).await);

            // 触发分段回调
            callback(SegmentEvent::Segment(SegmentInfo {
                prev_file_path: target,
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

/// 交付名顺延的上限。撞车最多也就是同一秒里关闭的那几段，给到这个量级只是为了让循环
/// 一定有尽头，真到不了这里说明目录本身出了问题，那一段如实记为收尾失败。
const MAX_DELIVERY_NAME_ATTEMPTS: u32 = 1000;

/// 内部分段交给 ffmpeg 的输出模板：`{展开后的名字}-%05d.{后缀}.part`。
///
/// `%05d` 是 segment 复用器唯一能保证「每段一个新文件」的占位符。整条路径里其它位置的
/// 字面 `%`——展开后的名字（主播名、标题里都可能有）和输出目录本身——都必须转义成 `%%`，
/// 否则 ffmpeg 会把它当成另一个占位符，整条模板被判非法、连头都写不出来。
fn internal_segment_pattern(
    download_config: &DownloadConfig,
    started_at: DateTime<Local>,
) -> PathBuf {
    let stem = internal_delivery_stem(download_config, started_at).replace('%', "%%");
    let output_dir = download_config.output_dir.display().to_string();
    PathBuf::from(output_dir.replace('%', "%%"))
        .join(format!("{stem}-%05d.{}.part", download_config.suffix))
}

/// 按用户配置的文件名模板展开出这一段该叫什么，时间取该分段的开始时刻。
fn internal_delivery_stem(download_config: &DownloadConfig, at: DateTime<Local>) -> String {
    let template = download_config.recorder.filename_template();
    match download_config.recorder.try_format_at(&template, at) {
        Some(stem) => stem,
        None => {
            // 与 `Recorder::format` 同一口径：模板非法时占位符原样保留，不让一场录制炸掉。
            error!(template, "时间格式串不合法，占位符按原样保留");
            template
        }
    }
}

/// 交付名的候选序列：先用模板展开的名字，被占用就顺延 `-2`、`-3`……
///
/// 「被占用」既包括磁盘上已经存在，也包括本次已经交付过的名字——上层拿到分段后随时可能
/// 把文件移走，只看磁盘不足以避免第二次用同一个名字。源文件是 ffmpeg 按序号写出的独立
/// 文件，顺延只影响这一段叫什么，不会丢数据。
fn unique_delivery_path(
    output_dir: &Path,
    stem: &str,
    suffix: &str,
    delivered: &std::collections::HashSet<PathBuf>,
) -> Option<PathBuf> {
    (0..MAX_DELIVERY_NAME_ATTEMPTS).find_map(|attempt| {
        let candidate = if attempt == 0 {
            output_dir.join(format!("{stem}.{suffix}"))
        } else {
            output_dir.join(format!("{stem}-{}.{suffix}", attempt + 1))
        };
        (!delivered.contains(&candidate) && !candidate.exists()).then_some(candidate)
    })
}

/// `-segment_format` 要的是**复用器名**，不是文件扩展名：ffmpeg 9 不认 `ts`
/// （报 "Muxer not found"），内部分段配 ts 后缀会在写头时直接失败。只映射扩展名与复用器名
/// 对不上的那几个，其余原样交给 ffmpeg 判断。
fn segment_muxer_name(suffix: &str) -> &str {
    match suffix {
        "ts" => "mpegts",
        "mkv" => "matroska",
        other => other,
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
    use chrono::{Duration as ChronoDuration, TimeZone};
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

    /// 合成源的时长，秒。分段断言按它核对「有没有整段被覆盖掉」。
    const SOURCE_SECONDS: f64 = 6.0;

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
                "6", // 与 SOURCE_SECONDS 一致
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

    /// 媒体文件的时长（秒）。读不出来记 0，让断言按「数据丢了」处理。
    fn media_duration_secs(path: &Path) -> f64 {
        let output = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "default=noprint_wrappers=1:nokey=1",
            ])
            .arg(path)
            .output()
            .expect("ffprobe 不可用，无法核对分段时长");
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .unwrap_or(0.0)
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
    /// 这里回归三个缺陷：一是原先「循环里改名、循环后又对同一个 .part 改名」，最后一段必然
    /// 重复回调且整次下载返回错误；二是 `-strftime 1` 只有秒级精度，同一秒关闭的两段拿到
    /// 同一个文件名、被 ffmpeg 以 O_TRUNC 覆盖，数据真的丢了；三是 ffmpeg 9 不认 `ts` 这个
    /// `-segment_format` 值。**源不限速地喂完**，几段必然落在同一秒内关闭——这正是撞车的
    /// 复现条件，修复后每段各占一个文件，没有一次失败的收尾。
    ///
    /// 两个后缀都跑：`ts` 要经过复用器名映射（mpegts），`flv` 原样下发。
    #[tokio::test]
    async fn internal_segments_close_once_each_and_keep_distinct_identity() {
        let source_dir = tempfile::tempdir().unwrap();
        let media = std::fs::read(synthetic_source(source_dir.path())).unwrap();

        for suffix in ["ts", "flv"] {
            let directory = tempfile::tempdir().unwrap();
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
                tracing_subscriber::registry()
                    .with(CaptureLayer::new(runtime.emitter()).filtered()),
            );
            // 一次性喂完：分段全部在同一秒内关闭，交付名必然撞车，走的正是顺延分支。
            let (url, _) = paced_origin(media.clone(), usize::MAX, Duration::ZERO).await;
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
                    config(url, directory.path(), suffix, Some("00:00:02")),
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
            assert!(
                segments.len() >= 2,
                "两秒切一次的六秒源应当交付多个分段，{suffix} 实际 {}",
                segments.len()
            );
            assert_eq!(closed.len(), segments.len());
            // 每一段都收得了尾：撞车靠顺延交付名解决，不再有被覆盖后记一次失败的分段。
            assert!(
                native(&events, "recording.segment_closed")
                    .into_iter()
                    .all(|e| field(e, "outcome") == "executed"),
                "同一秒关闭的分段不应再出现失败的收尾"
            );
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
                assert_eq!(
                    segment.prev_file_path.extension().and_then(|e| e.to_str()),
                    Some(suffix),
                    "交付的仍是配置的后缀，补扫按扩展名筛选才认得"
                );
                ids.push(id);
            }
            ids.sort();
            ids.dedup();
            assert_eq!(ids.len(), segments.len(), "每个分段各有独立身份");
            let mut paths: Vec<_> = segments.iter().map(|s| s.prev_file_path.clone()).collect();
            paths.sort();
            paths.dedup();
            assert_eq!(paths.len(), segments.len(), "同一个文件不会被交付两次");
            // 数据没丢：各段时长之和应当接近整个源。比字节数可靠——不同容器的开销差得远，
            // 而撞车覆盖时这里只会剩下最后一段的长度。
            let delivered_secs: f64 = segments
                .iter()
                .map(|s| {
                    assert!(
                        std::fs::metadata(&s.prev_file_path).unwrap().len() > 0,
                        "交付的分段不应是空文件"
                    );
                    media_duration_secs(&s.prev_file_path)
                })
                .sum();
            assert!(
                delivered_secs >= SOURCE_SECONDS - 1.0,
                "{suffix} 交付 {delivered_secs:.2} 秒，源有 {SOURCE_SECONDS} 秒，\
                 差得太多说明有分段被覆盖"
            );
            assert!(
                events
                    .iter()
                    .any(|e| e.data().capture_kind == CaptureKind::LegacyBridge
                        && e.data().message.contains("[ffmpeg]")),
                "旧的逐行 stderr 输出必须保留"
            );
        }
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

    /// 内部分段的命名分工：ffmpeg 侧只有序号占位符，时间占位符由本进程展开。
    /// segment 复用器不能同时用 strftime 和 `%d`，而只有 `%d` 能保证每段落在一个新文件里。
    #[test]
    fn internal_segment_pattern_numbers_the_files_and_expands_the_time_itself() {
        let directory = tempfile::tempdir().unwrap();
        let config = config(String::new(), directory.path(), "ts", Some("00:00:02"));
        let at = Local.with_ymd_and_hms(2026, 9, 1, 10, 15, 30).unwrap();

        let pattern = internal_segment_pattern(&config, at);
        assert_eq!(
            pattern,
            directory.path().join("ffmpeg-20260901T101530-%05d.ts.part")
        );

        let downloader = FfmpegDownloader::new(Vec::new(), DownloaderType::FfmpegInternal);
        let args = downloader.build_ffmpeg_args_internal_segment(&config);
        assert!(!args.iter().any(|arg| arg == "-strftime"));
        // 后缀是文件扩展名，`-segment_format` 要的是复用器名：ffmpeg 9 不认 "ts"。
        let format = args
            .iter()
            .position(|arg| arg == "-segment_format")
            .and_then(|index| args.get(index + 1));
        assert_eq!(format.map(String::as_str), Some("mpegts"));
    }

    /// 展开后的名字里若还留着字面 `%`，交给 ffmpeg 前必须转义，否则整条模板被判非法。
    #[test]
    fn a_literal_percent_in_the_expanded_name_is_escaped_for_ffmpeg() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = config(String::new(), directory.path(), "flv", None);
        config.recorder = Recorder::new(
            Some("cut%%name".into()),
            config.recorder.streamer_info.clone(),
        );
        let at = Local.with_ymd_and_hms(2026, 9, 1, 10, 15, 30).unwrap();

        assert_eq!(internal_delivery_stem(&config, at), "cut%name");
        assert_eq!(
            internal_segment_pattern(&config, at),
            directory.path().join("cut%%name-%05d.flv.part")
        );
        // 输出目录里的字面 `%` 同样要转义，否则整条路径会被 ffmpeg 判为非法模板。
        config.output_dir = PathBuf::from("/media/100%live");
        assert_eq!(
            internal_segment_pattern(&config, at),
            PathBuf::from("/media/100%%live/cut%%name-%05d.flv.part")
        );
    }

    /// 交付名按各段自己的开始时刻展开，同一秒的两段撞车时顺延序号而不是互相覆盖。
    #[test]
    fn delivery_names_follow_the_template_and_step_aside_on_a_collision() {
        let directory = tempfile::tempdir().unwrap();
        let config = config(String::new(), directory.path(), "ts", Some("00:00:02"));
        let first = Local.with_ymd_and_hms(2026, 9, 1, 10, 15, 30).unwrap();
        let second = first + ChronoDuration::seconds(2);
        assert_ne!(
            internal_delivery_stem(&config, first),
            internal_delivery_stem(&config, second),
            "不同秒的分段各有各的名字"
        );

        let stem = internal_delivery_stem(&config, first);
        let mut delivered = std::collections::HashSet::new();
        let mut names = Vec::new();
        for _ in 0..3 {
            let path = unique_delivery_path(directory.path(), &stem, "ts", &delivered)
                .expect("应能取到唯一的交付名");
            delivered.insert(path.clone());
            names.push(path);
        }
        assert_eq!(
            names,
            vec![
                directory.path().join("ffmpeg-20260901T101530.ts"),
                directory.path().join("ffmpeg-20260901T101530-2.ts"),
                directory.path().join("ffmpeg-20260901T101530-3.ts"),
            ]
        );
        // 磁盘上已经存在的同名文件同样算被占用，补扫留下的旧文件不会被顶掉。
        std::fs::write(directory.path().join("ffmpeg-20260901T101530.ts"), b"old").unwrap();
        assert_eq!(
            unique_delivery_path(directory.path(), &stem, "ts", &Default::default()),
            Some(directory.path().join("ffmpeg-20260901T101530-2.ts"))
        );
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
