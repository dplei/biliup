use crate::server::common::ffmpeg_scan::run_scanning_stderr;
use crate::server::common::process_priority::background;
use crate::server::errors::{AppError, AppResult};
use async_trait::async_trait;
use error_stack::{ResultExt, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime};
use tokio::process::Command;
use tokio::sync::Semaphore;
use tracing::{info, warn};

pub const BASE_TARGET_LUFS: f64 = -16.0;
const LRA: f64 = 11.0;
const TRUE_PEAK: f64 = -1.5;
const STDERR_LIMIT: usize = 16 * 1024;
const SAMPLE_DIR: &str = "audio-normalization";
const SAMPLE_FILE: &str = "sample.m4a";
const CAPTURE_NEXT: &str = "capture-next";
static NORMALIZE_SLOTS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(1)));
/// Artifacts which belong to this process and may still be read by an upload.  A fresh process
/// has an empty set, so its first normalization pass can safely remove leftovers from a crash.
static ACTIVE_NORMALIZATION_ARTIFACTS: LazyLock<Mutex<HashSet<PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

#[derive(Debug, Clone, Copy)]
pub struct LoudnessTarget(pub f64);

#[derive(Debug, Clone, PartialEq)]
pub struct AudioProbe {
    pub duration_seconds: Option<f64>,
    pub primary_audio_stream: Option<usize>,
    /// 视频流的 codec 名，按流顺序排列。转码用 `-c copy` 搬运视频，所以产物这一项
    /// 必须与原片逐项相等；不等即说明 ffmpeg 走了意料之外的路径。
    pub video_codecs: Vec<String>,
}

impl AudioProbe {
    pub fn has_video(&self) -> bool {
        !self.video_codecs.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoudnessMeasurement {
    pub input_i: f64,
    pub input_lra: f64,
    pub input_tp: f64,
    pub input_thresh: f64,
    pub target_offset: f64,
}

/// 测量那一遍的产出。响度分析需要完整 demux 一遍原片，时间戳检测同样如此，
/// 于是两件事合并到同一次 ffmpeg 调用里，省掉一整遍全片读。
pub struct MeasureScan {
    /// 含 loudnorm JSON 的 stderr 尾部窗口。
    pub stderr: String,
    /// 顺带扫出的原片时间戳诊断。`None` 表示这个 runner 不做检测。
    pub timestamp_anomaly: Option<bool>,
}

#[async_trait]
pub trait AudioFfmpegRunner: Send + Sync {
    async fn probe(&self, input: &Path) -> AppResult<AudioProbe>;
    async fn measure(&self, input: &Path, target: LoudnessTarget) -> AppResult<MeasureScan>;
    async fn transcode(
        &self,
        input: &Path,
        output: &Path,
        target: LoudnessTarget,
        measured: &LoudnessMeasurement,
    ) -> AppResult<()>;
}

#[derive(Debug)]
pub struct TempArtifact {
    path: PathBuf,
    /// 产物已改名到别处，`Drop` 不该再去删原路径。
    committed: bool,
}

impl TempArtifact {
    fn normalization_output(path: PathBuf) -> Self {
        ACTIVE_NORMALIZATION_ARTIFACTS
            .lock()
            .expect("active normalization artifacts mutex poisoned")
            .insert(path.clone());
        Self {
            path,
            committed: false,
        }
    }

    /// 纯本地临时件的清理 guard：不登记进活动产物表，因为它不是某段录像的标准化产物，
    /// 不会被 `cleanup_orphaned_normalization_artifacts` 的命名规则匹配到。
    fn guard(path: PathBuf) -> Self {
        Self {
            path,
            committed: false,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub async fn cleanup(&self) {
        let _ = tokio::fs::remove_file(&self.path).await;
    }

    /// 把产物原子替换到 `target`：先 fsync 产物、再 `rename`、最后 fsync 父目录让改名落盘。
    ///
    /// `rename` 同目录同文件系统，POSIX 保证原子，因此 `target` 要么是完整的旧内容、
    /// 要么是完整的新内容，不存在中间态。失败时 `self` 在函数结束处被丢弃，`Drop` 删掉
    /// 半成品，`target` 保持原样。
    async fn commit_replacing(mut self, target: &Path) -> AppResult<()> {
        let file = tokio::fs::File::open(&self.path)
            .await
            .change_context(AppError::Custom("failed to open artifact for fsync".into()))?;
        file.sync_all()
            .await
            .change_context(AppError::Custom("failed to fsync artifact".into()))?;
        drop(file);
        tokio::fs::rename(&self.path, target)
            .await
            .change_context(AppError::Custom("failed to replace original".into()))?;
        // 改名已经生效，此后无论如何都不能再删 `self.path`——那里可能已是别的文件。
        self.committed = true;
        if let Some(directory) = target.parent()
            && let Ok(dir) = tokio::fs::File::open(directory).await
        {
            // 目录 fsync 失败只影响崩溃后改名是否可见，不影响当前进程看到的状态，
            // 因此不升级为错误。
            let _ = dir.sync_all().await;
        }
        Ok(())
    }
}

impl Drop for TempArtifact {
    fn drop(&mut self) {
        ACTIVE_NORMALIZATION_ARTIFACTS
            .lock()
            .expect("active normalization artifacts mutex poisoned")
            .remove(&self.path);
        if !self.committed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginalReason {
    MissingOrEmpty,
    NoAudio,
    ProbeFailed,
    MeasureFailed,
    InvalidMeasurement,
    TranscodeFailed,
    InvalidOutput,
}

/// 标准化成功后产物的去向。调用方只匹配这个枚举，不要靠配置项反推当前是哪种形态。
#[derive(Debug)]
pub enum NormalizedForm {
    /// 产物已原子替换原片，上传路径就是原片路径，没有临时件需要善后。
    ReplacedOriginal,
    /// 产物是独立临时件（`keep_original`），原片保持不动，上传结束后由调用方清理。
    Artifact(TempArtifact),
}

#[derive(Debug)]
pub enum NormalizationOutcome {
    Original {
        reason: OriginalReason,
    },
    Normalized {
        form: NormalizedForm,
        measurement: LoudnessMeasurement,
        /// 测量那一遍顺带扫出的原片时间戳诊断。`true` 表示原片干净，而标准化只是
        /// `-c copy` 搬运视频流 + 重编音频，产物不会凭空长出时间戳异常，于是上传前
        /// 可以跳过对产物的整片检测。拿不到诊断时保持 `false`，照常检测。
        source_timestamps_clean: bool,
    },
}

impl NormalizationOutcome {
    pub fn upload_path<'a>(&'a self, original: &'a Path) -> &'a Path {
        match self {
            Self::Original { .. } => original,
            Self::Normalized {
                form: NormalizedForm::ReplacedOriginal,
                ..
            } => original,
            Self::Normalized {
                form: NormalizedForm::Artifact(artifact),
                ..
            } => artifact.path(),
        }
    }
}

#[derive(Deserialize)]
struct RawMeasurement {
    input_i: String,
    input_lra: String,
    input_tp: String,
    input_thresh: String,
    target_offset: String,
}

pub fn parse_loudnorm_measurement(stderr: &str) -> AppResult<LoudnessMeasurement> {
    let mut depth = 0usize;
    let mut end = None;
    for (index, byte) in stderr.as_bytes().iter().enumerate().rev() {
        match byte {
            b'}' => {
                if end.is_none() {
                    end = Some(index + 1);
                }
                depth += 1;
            }
            b'{' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    let candidate = &stderr[index..end.expect("JSON end")];
                    if let Ok(raw) = serde_json::from_str::<RawMeasurement>(candidate) {
                        let value = LoudnessMeasurement {
                            input_i: number(&raw.input_i)?,
                            input_lra: number(&raw.input_lra)?,
                            input_tp: number(&raw.input_tp)?,
                            input_thresh: number(&raw.input_thresh)?,
                            target_offset: number(&raw.target_offset)?,
                        };
                        validate_measurement(&value)?;
                        return Ok(value);
                    }
                    end = None;
                }
            }
            _ => {}
        }
    }
    bail!(AppError::Custom(
        "loudnorm measurement JSON not found".into()
    ))
}

fn number(value: &str) -> AppResult<f64> {
    value
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .ok_or_else(|| {
            error_stack::Report::new(AppError::Custom(format!(
                "invalid loudnorm number: {value}"
            )))
        })
}

fn validate_measurement(v: &LoudnessMeasurement) -> AppResult<()> {
    if !((-100.0..=20.0).contains(&v.input_i)
        && (0.0..=100.0).contains(&v.input_lra)
        && (-100.0..=20.0).contains(&v.input_tp)
        && (-100.0..=20.0).contains(&v.input_thresh)
        && (-100.0..=100.0).contains(&v.target_offset))
    {
        bail!(AppError::Custom("implausible loudnorm measurement".into()));
    }
    Ok(())
}

pub fn normalized_temp_path(source: &Path) -> PathBuf {
    let stem = source
        .file_stem()
        .and_then(|v| v.to_str())
        .unwrap_or("segment");
    let ext = source.extension().and_then(|v| v.to_str()).unwrap_or("mkv");
    source
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(
            "{stem}.audio-normalized-{:016x}.part.{ext}",
            rand::random::<u64>()
        ))
}

/// 对 `source` 做双遍 loudnorm。
///
/// `keep_original` 为 `false`（默认）时产物校验通过后原子替换原片，上传路径仍是原片路径，
/// 额外磁盘占用只存在于转码窗口内；为 `true` 时保留原片、产出独立临时件，由调用方在上传
/// 结束后清理。任一步失败一律降级直传原片。
pub async fn normalize_for_upload<R: AudioFfmpegRunner>(
    source: &Path,
    target_lufs: f64,
    runner: &R,
    keep_original: bool,
) -> NormalizationOutcome {
    let started = std::time::Instant::now();
    if let Some(directory) = source.parent() {
        cleanup_orphaned_normalization_artifacts(directory).await;
    }
    let Ok(input_bytes) = tokio::fs::metadata(source).await.map(|m| m.len()) else {
        return NormalizationOutcome::Original {
            reason: OriginalReason::MissingOrEmpty,
        };
    };
    if input_bytes == 0 {
        return NormalizationOutcome::Original {
            reason: OriginalReason::MissingOrEmpty,
        };
    }
    let input = match runner.probe(source).await {
        Ok(v) => v,
        Err(error) => {
            warn!(audio_normalization = "failed", file=%source.display(), ?error, "audio normalization failed during probe");
            return NormalizationOutcome::Original {
                reason: OriginalReason::ProbeFailed,
            };
        }
    };
    if input.primary_audio_stream.is_none() {
        info!(audio_normalization = "skipped", file = %source.display(), "audio normalization skipped: no audio stream");
        return NormalizationOutcome::Original {
            reason: OriginalReason::NoAudio,
        };
    }
    let _permit = NORMALIZE_SLOTS
        .acquire()
        .await
        .expect("normalization semaphore open");
    let target = LoudnessTarget(target_lufs);
    info!(
        audio_normalization = "started",
        file = %source.display(),
        target_lufs,
        "audio normalization started"
    );
    let scan = match runner.measure(source, target).await {
        Ok(v) => v,
        Err(error) => {
            warn!(audio_normalization = "failed", file=%source.display(), ?error, "audio normalization failed during measure");
            return NormalizationOutcome::Original {
                reason: OriginalReason::MeasureFailed,
            };
        }
    };
    let source_timestamps_clean = scan.timestamp_anomaly == Some(false);
    let measurement = match parse_loudnorm_measurement(&scan.stderr) {
        Ok(v) => v,
        Err(error) => {
            warn!(audio_normalization = "failed", file=%source.display(), ?error, "audio normalization failed: invalid measurement");
            return NormalizationOutcome::Original {
                reason: OriginalReason::InvalidMeasurement,
            };
        }
    };
    // Construct the guard before starting ffmpeg: cancellation/drop then removes a partially
    // written .part file as well as a completed one.
    let artifact = TempArtifact::normalization_output(normalized_temp_path(source));
    if let Err(error) = runner
        .transcode(source, artifact.path(), target, &measurement)
        .await
    {
        artifact.cleanup().await;
        warn!(audio_normalization = "failed", file=%source.display(), ?error, "audio normalization failed during transcode");
        return NormalizationOutcome::Original {
            reason: OriginalReason::TranscodeFailed,
        };
    }
    let output_bytes = tokio::fs::metadata(artifact.path())
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    let output_probe = if output_bytes > 0 {
        runner.probe(artifact.path()).await.ok()
    } else {
        None
    };
    if let Err(reason) = output_is_faithful(&input, output_probe.as_ref(), input_bytes, output_bytes)
    {
        artifact.cleanup().await;
        warn!(audio_normalization = "failed", file=%source.display(), reason, "audio normalization produced invalid output");
        return NormalizationOutcome::Original {
            reason: OriginalReason::InvalidOutput,
        };
    }
    let form = if keep_original {
        NormalizedForm::Artifact(artifact)
    } else {
        // 替换失败时 `artifact` 已被 `commit_replacing` 消费，其 `Drop` 会删掉半成品，
        // 原片保持原样，本段直传原片。
        if let Err(error) = artifact.commit_replacing(source).await {
            warn!(audio_normalization = "failed", file=%source.display(), ?error, "audio normalization failed to replace original");
            return NormalizationOutcome::Original {
                reason: OriginalReason::TranscodeFailed,
            };
        }
        NormalizedForm::ReplacedOriginal
    };
    info!(audio_normalization="completed", file=%source.display(), target_lufs, replaced_original=!keep_original,
        input_lufs=measurement.input_i, elapsed_ms=started.elapsed().as_millis(), output_size_bytes=output_bytes,
        "audio normalization completed");
    NormalizationOutcome::Normalized {
        form,
        measurement,
        source_timestamps_clean,
    }
}

/// 产物与原片的一致性判据。
///
/// 判据对两种形态统一生效：不过一律丢弃产物、直传原片，这是安全方向，而两套判据会让
/// 「这段为什么没标准化」的排查依赖当前模式。原地替换是不可逆的，所以除了「能不能播」
/// 还要比对时长、体积与视频流构成。
fn output_is_faithful(
    input: &AudioProbe,
    output: Option<&AudioProbe>,
    input_bytes: u64,
    output_bytes: u64,
) -> Result<(), &'static str> {
    let Some(output) = output else {
        return Err("output_unreadable");
    };
    if output.primary_audio_stream.is_none() {
        return Err("output_has_no_audio");
    }
    if output.duration_seconds.unwrap_or(0.0) <= 0.0 {
        return Err("output_has_no_duration");
    }
    if input.video_codecs != output.video_codecs {
        return Err("video_streams_differ");
    }
    // 视频 `-c copy`、音频重编到 192k，产物应当接近原片。掉到一半以下说明视频流没被搬
    // 过来，这是「明显异常」阈值，不是精确预算。
    if output_bytes.saturating_mul(2) < input_bytes {
        return Err("output_too_small");
    }
    // loudnorm 不改时长，正常偏差只来自容器时间基取整。原片时长探不到时跳过这一项，
    // 不拿缺失当失败。
    if let Some(source_duration) = input.duration_seconds.filter(|v| *v > 0.0) {
        let tolerance = (source_duration * 0.005).max(1.0);
        let drift = (output.duration_seconds.unwrap_or(0.0) - source_duration).abs();
        if drift > tolerance {
            return Err("duration_drift");
        }
    }
    Ok(())
}

/// 仅扫描当前录像目录的一层。当前进程未登记的 `.part` 都来自上次失败或重启，立即清理；
/// 已完成但仍在上传的 artifact 会留在登记表中，绝不会被下一段的预处理误删。
async fn cleanup_orphaned_normalization_artifacts(directory: &Path) {
    let Ok(mut entries) = tokio::fs::read_dir(directory).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.contains(".audio-normalized-") || !name.contains(".part.") {
            continue;
        }
        let path = entry.path();
        let active = ACTIVE_NORMALIZATION_ARTIFACTS
            .lock()
            .expect("active normalization artifacts mutex poisoned")
            .contains(&path);
        if !active && tokio::fs::remove_file(&path).await.is_ok() {
            info!(file = %path.display(), "removed orphaned audio normalization temporary file");
        }
    }
}

pub struct SystemAudioFfmpeg {
    audio_bitrate: &'static str,
}

impl Default for SystemAudioFfmpeg {
    fn default() -> Self {
        Self {
            audio_bitrate: "192k",
        }
    }
}

impl SystemAudioFfmpeg {
    fn for_sample() -> Self {
        Self {
            audio_bitrate: "96k",
        }
    }
}

#[derive(Deserialize)]
struct ProbeOutput {
    streams: Vec<ProbeStream>,
    format: Option<ProbeFormat>,
}
#[derive(Deserialize)]
struct ProbeStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    index: usize,
}
#[derive(Deserialize)]
struct ProbeFormat {
    duration: Option<String>,
}

fn stderr_text(bytes: &[u8]) -> String {
    let start = bytes.len().saturating_sub(STDERR_LIMIT);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

#[async_trait]
impl AudioFfmpegRunner for SystemAudioFfmpeg {
    async fn probe(&self, input: &Path) -> AppResult<AudioProbe> {
        let mut command = Command::new("ffprobe");
        let output = background(&mut command)
            .args([
                "-v",
                "error",
                "-show_streams",
                "-show_format",
                "-of",
                "json",
            ])
            .arg(input)
            .kill_on_drop(true)
            .output()
            .await
            .change_context(AppError::Custom("failed to spawn ffprobe".into()))?;
        if !output.status.success() {
            bail!(AppError::Custom(format!(
                "ffprobe failed: {}",
                stderr_text(&output.stderr)
            )));
        }
        let parsed: ProbeOutput = serde_json::from_slice(&output.stdout)
            .change_context(AppError::Custom("invalid ffprobe JSON".into()))?;
        Ok(AudioProbe {
            duration_seconds: parsed
                .format
                .and_then(|v| v.duration)
                .and_then(|v| v.parse().ok()),
            primary_audio_stream: parsed
                .streams
                .iter()
                .find(|v| v.codec_type.as_deref() == Some("audio"))
                .map(|v| v.index),
            video_codecs: parsed
                .streams
                .iter()
                .filter(|v| v.codec_type.as_deref() == Some("video"))
                .map(|v| v.codec_name.clone().unwrap_or_default())
                .collect(),
        })
    }

    async fn measure(&self, input: &Path, target: LoudnessTarget) -> AppResult<MeasureScan> {
        let filter = format!(
            "loudnorm=I={}:LRA={LRA}:TP={TRUE_PEAK}:print_format=json",
            target.0
        );
        // 两路输出共用一次 demux：第一路 `-c copy` 到 null 只为拿时间戳诊断（包直接丢弃，
        // 几乎不额外耗 CPU），第二路才是响度分析。`-loglevel verbose` 是诊断所必需的，
        // 低于 warning 的 "Invalid timestamp" 之类只有这一级才会输出。
        let mut command = Command::new("ffmpeg");
        command
            .args([
                "-hide_banner",
                "-loglevel",
                "verbose",
                "-nostats",
                "-fflags",
                "+igndts",
                "-i",
            ])
            .arg(input)
            .args(["-c", "copy", "-f", "null", "-"])
            .args(["-map", "0:a:0", "-af", &filter, "-f", "null", "-"]);
        let (status, scan) = run_scanning_stderr(background(&mut command))
            .await
            .change_context(AppError::Custom(
                "failed to spawn ffmpeg (loudnorm measure)".into(),
            ))?;
        if !status.success() {
            bail!(AppError::Custom(format!(
                "ffmpeg loudnorm measure failed: {}",
                scan.tail
            )));
        }
        Ok(MeasureScan {
            stderr: scan.tail,
            timestamp_anomaly: Some(scan.timestamp_anomaly),
        })
    }

    async fn transcode(
        &self,
        input: &Path,
        output: &Path,
        target: LoudnessTarget,
        m: &LoudnessMeasurement,
    ) -> AppResult<()> {
        let filter = format!(
            "loudnorm=I={}:LRA={LRA}:TP={TRUE_PEAK}:measured_I={}:measured_LRA={}:measured_TP={}:measured_thresh={}:offset={}:linear=true:print_format=summary",
            target.0, m.input_i, m.input_lra, m.input_tp, m.input_thresh, m.target_offset
        );
        let mut command = Command::new("ffmpeg");
        background(&mut command)
            .args(["-hide_banner", "-nostats", "-y", "-i"])
            .arg(input)
            .args([
                "-map",
                "0",
                "-c",
                "copy",
                "-c:a:0",
                "aac",
                "-b:a:0",
                self.audio_bitrate,
                "-ar:a:0",
                "48000",
                "-filter:a:0",
                &filter,
            ]);
        if matches!(
            output.extension().and_then(|value| value.to_str()),
            Some("mp4" | "mov" | "m4v")
        ) {
            command.args(["-movflags", "+faststart"]);
        }
        let result = command
            .arg(output)
            .kill_on_drop(true)
            .output()
            .await
            .change_context(AppError::Custom(
                "failed to spawn ffmpeg (loudnorm transcode)".into(),
            ))?;
        if !result.status.success() {
            bail!(AppError::Custom(format!(
                "ffmpeg loudnorm transcode failed: {}",
                stderr_text(&result.stderr)
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AudioSampleStatus {
    pub sample_ready: bool,
    pub capture_pending: bool,
    pub updated_at: Option<String>,
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct AudioSampleStore {
    root: PathBuf,
}
#[derive(Debug)]
pub struct CaptureClaim {
    path: PathBuf,
}

impl AudioSampleStore {
    pub fn for_working_directory(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().join(SAMPLE_DIR),
        }
    }
    pub fn sample_path(&self) -> PathBuf {
        self.root.join(SAMPLE_FILE)
    }
    async fn ensure_root(&self) -> AppResult<()> {
        tokio::fs::create_dir_all(&self.root)
            .await
            .change_context(AppError::Unknown)
    }

    pub async fn status(&self) -> AppResult<AudioSampleStatus> {
        self.ensure_root().await?;
        self.restore_stale_claims().await?;
        let meta = tokio::fs::metadata(self.sample_path()).await.ok();
        let updated_at = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .map(|v| chrono::DateTime::<chrono::Utc>::from(v).to_rfc3339());
        let capture_pending = tokio::fs::try_exists(self.root.join(CAPTURE_NEXT))
            .await
            .unwrap_or(false)
            || !self.claims().await?.is_empty();
        Ok(AudioSampleStatus {
            sample_ready: meta.is_some(),
            capture_pending,
            updated_at,
            size_bytes: meta.map(|m| m.len()),
        })
    }

    pub async fn arm_capture(&self) -> AppResult<()> {
        self.ensure_root().await?;
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.root.join(CAPTURE_NEXT))
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(e).change_context(AppError::Unknown),
        }
    }
    pub async fn cancel_capture(&self) -> AppResult<()> {
        self.ensure_root().await?;
        remove_if_exists(&self.root.join(CAPTURE_NEXT)).await
    }
    pub async fn delete_sample(&self) -> AppResult<()> {
        self.ensure_root().await?;
        remove_if_exists(&self.sample_path()).await
    }

    pub async fn try_claim_capture(&self) -> AppResult<Option<CaptureClaim>> {
        self.ensure_root().await?;
        self.restore_stale_claims().await?;
        let claim = self.root.join(format!(
            "capture-in-progress-{:016x}",
            rand::random::<u64>()
        ));
        match tokio::fs::rename(self.root.join(CAPTURE_NEXT), &claim).await {
            Ok(()) => Ok(Some(CaptureClaim { path: claim })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).change_context(AppError::Unknown),
        }
    }
    pub async fn commit_sample(&self, claim: CaptureClaim, temp: &Path) -> AppResult<()> {
        tokio::fs::rename(temp, self.sample_path())
            .await
            .change_context(AppError::Unknown)?;
        remove_if_exists(&claim.path).await
    }
    pub async fn retry_later(&self, claim: CaptureClaim) -> AppResult<()> {
        if tokio::fs::try_exists(self.root.join(CAPTURE_NEXT))
            .await
            .unwrap_or(false)
        {
            return remove_if_exists(&claim.path).await;
        }
        tokio::fs::rename(claim.path, self.root.join(CAPTURE_NEXT))
            .await
            .change_context(AppError::Unknown)
    }
    async fn claims(&self) -> AppResult<Vec<tokio::fs::DirEntry>> {
        let mut dir = tokio::fs::read_dir(&self.root)
            .await
            .change_context(AppError::Unknown)?;
        let mut found = Vec::new();
        while let Some(entry) = dir.next_entry().await.change_context(AppError::Unknown)? {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("capture-in-progress-")
            {
                found.push(entry);
            }
        }
        Ok(found)
    }
    async fn restore_stale_claims(&self) -> AppResult<()> {
        let now = SystemTime::now();
        for entry in self.claims().await? {
            let stale = entry
                .metadata()
                .await
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|v| now.duration_since(v).ok())
                .is_some_and(|v| v > Duration::from_secs(3600));
            if stale {
                if tokio::fs::try_exists(self.root.join(CAPTURE_NEXT))
                    .await
                    .unwrap_or(false)
                {
                    remove_if_exists(&entry.path()).await?;
                } else {
                    tokio::fs::rename(entry.path(), self.root.join(CAPTURE_NEXT))
                        .await
                        .change_context(AppError::Unknown)?;
                }
            }
        }
        Ok(())
    }
}

async fn remove_if_exists(path: &Path) -> AppResult<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).change_context(AppError::Unknown),
    }
}

pub async fn maybe_capture_reference_sample(source: &Path, store: &AudioSampleStore) {
    let claim = match store.try_claim_capture().await {
        Ok(Some(v)) => v,
        Ok(None) => return,
        Err(error) => {
            warn!(?error, "failed to claim sample capture");
            return;
        }
    };
    if let Err(error) = create_reference_sample(source, store, &claim).await {
        warn!(file=%source.display(), ?error, "sample capture failed; will retry next segment");
        let _ = store.retry_later(claim).await;
    }
}

async fn create_reference_sample(
    source: &Path,
    store: &AudioSampleStore,
    claim: &CaptureClaim,
) -> AppResult<()> {
    let runner = SystemAudioFfmpeg::for_sample();
    let probe = runner.probe(source).await?;
    if probe.primary_audio_stream.is_none() {
        bail!(AppError::Custom("segment has no audio".into()));
    }
    let duration = probe
        .duration_seconds
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(30.0);
    let length = duration.clamp(0.1, 30.0);
    let start = ((duration - length) / 2.0).max(0.0);
    let raw = store
        .root
        .join(format!("sample-raw-{:016x}.m4a", rand::random::<u64>()));
    let mut command = Command::new("ffmpeg");
    let result = background(&mut command)
        .args([
            "-hide_banner",
            "-nostats",
            "-y",
            "-ss",
            &start.to_string(),
            "-t",
            &length.to_string(),
            "-i",
        ])
        .arg(source)
        .args([
            "-map", "0:a:0", "-vn", "-c:a", "aac", "-b:a", "96k", "-ar", "48000",
        ])
        .arg(&raw)
        .kill_on_drop(true)
        .output()
        .await
        .change_context(AppError::Custom("failed to spawn ffmpeg (sample)".into()))?;
    if !result.status.success() {
        let _ = remove_if_exists(&raw).await;
        bail!(AppError::Custom(format!(
            "sample extraction failed: {}",
            stderr_text(&result.stderr)
        )));
    }
    // 从此处开始任何 `?` 提前返回都会由 guard 清掉截取临时件。
    let raw_artifact = TempArtifact::guard(raw);
    // 样片走 `keep_original`：这里要的是产物本身，截取出来的 raw 反而是要丢掉的中间件。
    let outcome =
        normalize_for_upload(raw_artifact.path(), BASE_TARGET_LUFS, &runner, true).await;
    match outcome {
        NormalizationOutcome::Normalized {
            form: NormalizedForm::Artifact(artifact),
            ..
        } => {
            let sample_temp = store
                .root
                .join(format!("sample-new-{:016x}.m4a", rand::random::<u64>()));
            tokio::fs::rename(artifact.path(), &sample_temp)
                .await
                .change_context(AppError::Unknown)?;
            let sample_artifact = TempArtifact::guard(sample_temp);
            let check = runner.probe(sample_artifact.path()).await?;
            if check.primary_audio_stream.is_none() || check.duration_seconds.unwrap_or(0.0) <= 0.0
            {
                bail!(AppError::Custom("invalid generated sample".into()));
            }
            store
                .commit_sample(
                    CaptureClaim {
                        path: claim.path.clone(),
                    },
                    sample_artifact.path(),
                )
                .await?;
        }
        NormalizationOutcome::Original { reason } => bail!(AppError::Custom(format!(
            "sample normalization failed: {reason:?}"
        ))),
        // 上面显式传了 `keep_original = true`，不会走到就地替换。
        NormalizationOutcome::Normalized {
            form: NormalizedForm::ReplacedOriginal,
            ..
        } => bail!(AppError::Custom(
            "sample normalization unexpectedly replaced its source".into()
        )),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE_BYTES: &[u8] = b"source-recording";
    const SOURCE_DURATION: f64 = 600.0;

    struct FakeRunner {
        measure_ok: bool,
        timestamp_anomaly: Option<bool>,
        /// 产物内容，决定产物字节数。
        output_bytes: Vec<u8>,
        /// 产物时长；`None` 表示与原片一致。
        output_duration: Option<f64>,
        /// 产物视频流构成；`None` 表示与原片一致。
        output_video_codecs: Option<Vec<String>>,
        output_has_audio: bool,
    }

    impl Default for FakeRunner {
        fn default() -> Self {
            Self {
                measure_ok: true,
                timestamp_anomaly: Some(false),
                output_bytes: b"normalized-output".to_vec(),
                output_duration: None,
                output_video_codecs: None,
                output_has_audio: true,
            }
        }
    }

    impl FakeRunner {
        /// 测量成功，且顺带扫出原片时间戳干净。
        fn clean() -> Self {
            Self::default()
        }

        fn failing() -> Self {
            Self {
                measure_ok: false,
                timestamp_anomaly: None,
                ..Self::default()
            }
        }

        fn is_artifact(path: &Path) -> bool {
            path.file_name()
                .and_then(|v| v.to_str())
                .is_some_and(|v| v.contains(".audio-normalized-"))
        }
    }

    #[async_trait]
    impl AudioFfmpegRunner for FakeRunner {
        async fn probe(&self, input: &Path) -> AppResult<AudioProbe> {
            if !Self::is_artifact(input) {
                return Ok(AudioProbe {
                    duration_seconds: Some(SOURCE_DURATION),
                    primary_audio_stream: Some(1),
                    video_codecs: vec!["h264".into()],
                });
            }
            Ok(AudioProbe {
                duration_seconds: Some(self.output_duration.unwrap_or(SOURCE_DURATION)),
                primary_audio_stream: self.output_has_audio.then_some(1),
                video_codecs: self
                    .output_video_codecs
                    .clone()
                    .unwrap_or_else(|| vec!["h264".into()]),
            })
        }

        async fn measure(&self, _input: &Path, _target: LoudnessTarget) -> AppResult<MeasureScan> {
            if self.measure_ok {
                Ok(MeasureScan {
                    stderr: "{\"input_i\":\"-27.4\",\"input_tp\":\"-6.1\",\"input_lra\":\"3.2\",\"input_thresh\":\"-38\",\"target_offset\":\"0.1\"}".into(),
                    timestamp_anomaly: self.timestamp_anomaly,
                })
            } else {
                Err(error_stack::Report::new(AppError::Custom(
                    "measure failed".into(),
                )))
            }
        }

        async fn transcode(
            &self,
            _input: &Path,
            output: &Path,
            _target: LoudnessTarget,
            _measured: &LoudnessMeasurement,
        ) -> AppResult<()> {
            tokio::fs::write(output, &self.output_bytes).await.unwrap();
            Ok(())
        }
    }

    /// 目录里除了原片以外还剩下的东西——用来断言 `.part` 没有残留。
    async fn leftover_artifacts(dir: &Path) -> Vec<String> {
        let mut found = Vec::new();
        let mut entries = tokio::fs::read_dir(dir).await.unwrap();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.contains(".audio-normalized-") {
                found.push(name);
            }
        }
        found
    }

    /// 活动产物表是全局的，测试并行时不能断言它整体为空；只看本用例自己的目录。
    fn active_artifacts_under(dir: &Path) -> usize {
        ACTIVE_NORMALIZATION_ARTIFACTS
            .lock()
            .expect("active normalization artifacts mutex poisoned")
            .iter()
            .filter(|path| path.starts_with(dir))
            .count()
    }

    #[test]
    fn parses_loudnorm_json_among_logs() {
        let stderr = "log {\"other\":1}\n{\"input_i\":\"-27.40\",\"input_tp\":\"-6.10\",\"input_lra\":\"3.20\",\"input_thresh\":\"-38.00\",\"target_offset\":\"0.10\"}\nlog";
        assert_eq!(parse_loudnorm_measurement(stderr).unwrap().input_i, -27.4);
    }
    #[test]
    fn rejects_non_finite() {
        let stderr = "{\"input_i\":\"-inf\",\"input_tp\":\"-2\",\"input_lra\":\"1\",\"input_thresh\":\"-30\",\"target_offset\":\"0\"}";
        assert!(parse_loudnorm_measurement(stderr).is_err());
    }
    #[tokio::test]
    async fn store_is_idempotent_and_claims_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = AudioSampleStore::for_working_directory(dir.path());
        store.arm_capture().await.unwrap();
        store.arm_capture().await.unwrap();
        assert!(store.try_claim_capture().await.unwrap().is_some());
        assert!(store.try_claim_capture().await.unwrap().is_none());
        store.cancel_capture().await.unwrap();
        store.delete_sample().await.unwrap();
    }

    #[tokio::test]
    async fn keep_original_returns_temporary_artifact_and_drop_cleans_it() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("有 空格.flv");
        tokio::fs::write(&source, SOURCE_BYTES).await.unwrap();
        let outcome = normalize_for_upload(&source, -16.0, &FakeRunner::clean(), true).await;
        let output = match outcome {
            NormalizationOutcome::Normalized {
                form: NormalizedForm::Artifact(artifact),
                measurement,
                source_timestamps_clean,
            } => {
                assert_eq!(measurement.input_i, -27.4);
                assert!(source_timestamps_clean);
                let path = artifact.path().to_path_buf();
                assert!(tokio::fs::try_exists(&path).await.unwrap());
                drop(artifact);
                path
            }
            other => panic!("unexpected outcome: {other:?}"),
        };
        assert!(!tokio::fs::try_exists(output).await.unwrap());
        // 原片必须原封不动。
        assert_eq!(tokio::fs::read(&source).await.unwrap(), SOURCE_BYTES);
    }

    #[tokio::test]
    async fn missing_timestamp_diagnosis_does_not_claim_a_clean_source() {
        // runner 不提供诊断时必须保守：上传前照常跑整片时间戳扫描。
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("segment.flv");
        tokio::fs::write(&source, SOURCE_BYTES).await.unwrap();
        let runner = FakeRunner {
            timestamp_anomaly: None,
            ..FakeRunner::default()
        };
        assert!(matches!(
            normalize_for_upload(&source, -16.0, &runner, false).await,
            NormalizationOutcome::Normalized {
                source_timestamps_clean: false,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn anomalous_source_does_not_claim_a_clean_source() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("segment.flv");
        tokio::fs::write(&source, SOURCE_BYTES).await.unwrap();
        let runner = FakeRunner {
            timestamp_anomaly: Some(true),
            ..FakeRunner::default()
        };
        assert!(matches!(
            normalize_for_upload(&source, -16.0, &runner, false).await,
            NormalizationOutcome::Normalized {
                source_timestamps_clean: false,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn measurement_failure_falls_back_to_original() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("segment.flv");
        tokio::fs::write(&source, SOURCE_BYTES).await.unwrap();
        assert!(matches!(
            normalize_for_upload(&source, -16.0, &FakeRunner::failing(), false).await,
            NormalizationOutcome::Original {
                reason: OriginalReason::MeasureFailed
            }
        ));
    }

    #[tokio::test]
    async fn successful_normalization_replaces_the_original_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("有 空格.flv");
        tokio::fs::write(&source, SOURCE_BYTES).await.unwrap();
        let runner = FakeRunner::clean();
        let expected = runner.output_bytes.clone();

        let outcome = normalize_for_upload(&source, -16.0, &runner, false).await;

        assert!(matches!(
            outcome,
            NormalizationOutcome::Normalized {
                form: NormalizedForm::ReplacedOriginal,
                ..
            }
        ));
        // 上传路径仍是原片路径，内容已经是产物。
        assert_eq!(outcome.upload_path(&source), source.as_path());
        assert_eq!(tokio::fs::read(&source).await.unwrap(), expected);
        assert!(leftover_artifacts(dir.path()).await.is_empty());
        assert_eq!(active_artifacts_under(dir.path()), 0);
    }

    /// 时长、体积、视频流三项判据各自都要能挡住坏产物，且一律不动原片。
    #[tokio::test]
    async fn unfaithful_output_is_discarded_and_the_original_survives() {
        let cases: [(&str, FakeRunner); 4] = [
            (
                "duration_drift",
                FakeRunner {
                    output_duration: Some(SOURCE_DURATION - 10.0),
                    ..FakeRunner::default()
                },
            ),
            (
                "output_too_small",
                FakeRunner {
                    output_bytes: b"x".to_vec(),
                    ..FakeRunner::default()
                },
            ),
            (
                "video_streams_differ",
                FakeRunner {
                    output_video_codecs: Some(Vec::new()),
                    ..FakeRunner::default()
                },
            ),
            (
                "output_has_no_audio",
                FakeRunner {
                    output_has_audio: false,
                    ..FakeRunner::default()
                },
            ),
        ];
        for (label, runner) in cases {
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("segment.flv");
            tokio::fs::write(&source, SOURCE_BYTES).await.unwrap();

            let outcome = normalize_for_upload(&source, -16.0, &runner, false).await;

            assert!(
                matches!(
                    outcome,
                    NormalizationOutcome::Original {
                        reason: OriginalReason::InvalidOutput
                    }
                ),
                "{label} should have been rejected"
            );
            assert_eq!(
                tokio::fs::read(&source).await.unwrap(),
                SOURCE_BYTES,
                "{label} must not touch the original"
            );
            assert!(
                leftover_artifacts(dir.path()).await.is_empty(),
                "{label} left a partial artifact behind"
            );
            assert_eq!(active_artifacts_under(dir.path()), 0, "{label}");
        }
    }

    /// 时长偏差在容差内（0.5% 或 1 秒，取大者）不算失败。
    #[tokio::test]
    async fn duration_drift_within_tolerance_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("segment.flv");
        tokio::fs::write(&source, SOURCE_BYTES).await.unwrap();
        let runner = FakeRunner {
            output_duration: Some(SOURCE_DURATION + 2.0),
            ..FakeRunner::default()
        };

        assert!(matches!(
            normalize_for_upload(&source, -16.0, &runner, false).await,
            NormalizationOutcome::Normalized {
                form: NormalizedForm::ReplacedOriginal,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn failed_replacement_leaves_the_original_intact() {
        let dir = tempfile::tempdir().unwrap();
        // 用一个非空目录冒充原片：`rename` 到非空目录必然失败，这是本机上唯一稳定可复现
        // 的替换失败方式（只读目录在以 root 运行的容器里挡不住）。
        let source = dir.path().join("segment.flv");
        tokio::fs::create_dir(&source).await.unwrap();
        tokio::fs::write(source.join("occupant"), b"busy").await.unwrap();
        // 目录的 `len()` 是几十到几百字节，产物要够大才能越过体积判据、走到 rename 那一步。
        let runner = FakeRunner {
            output_bytes: vec![b'n'; 4096],
            ..FakeRunner::default()
        };

        let outcome = normalize_for_upload(&source, -16.0, &runner, false).await;

        assert!(matches!(
            outcome,
            NormalizationOutcome::Original {
                reason: OriginalReason::TranscodeFailed
            }
        ));
        assert!(tokio::fs::try_exists(source.join("occupant")).await.unwrap());
        assert!(leftover_artifacts(dir.path()).await.is_empty());
        assert_eq!(active_artifacts_under(dir.path()), 0);
    }

    #[tokio::test]
    async fn orphaned_partial_artifacts_are_removed_without_touching_active_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let orphan = dir.path().join("old.audio-normalized-deadbeef.part.flv");
        let active = dir.path().join("active.audio-normalized-deadbeef.part.flv");
        tokio::fs::write(&orphan, b"orphan").await.unwrap();
        tokio::fs::write(&active, b"active").await.unwrap();
        let active_artifact = TempArtifact::normalization_output(active.clone());

        cleanup_orphaned_normalization_artifacts(dir.path()).await;

        assert!(!tokio::fs::try_exists(orphan).await.unwrap());
        assert!(tokio::fs::try_exists(&active).await.unwrap());
        drop(active_artifact);
    }
}
