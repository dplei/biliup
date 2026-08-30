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

/// 转码期检查可用空间的间隔。GB 级分段的转码是分钟量级，10 秒的粒度足够，也不会把
/// `statvfs` 调成热点。
const DISK_CHECK_INTERVAL: Duration = Duration::from_secs(10);

/// 产物大小相对原片的安全系数。视频 `-c copy`、音频重编到 192k，产物通常在原片 ±10%。
/// 这是编码参数的推论，不是用户该判断的东西，所以不进配置。
const OUTPUT_SIZE_FACTOR: f64 = 1.1;

/// 一次上传的响度标准化配置。
///
/// 打包传递而不是一路加参数：上传编排的函数已经在 `too_many_arguments` 的边缘，每多一个
/// 旋钮就多一处调用点要改，也更容易把某个参数传串。
#[derive(Debug, Clone, Copy)]
pub struct NormalizationSettings {
    pub enabled: bool,
    pub target_lufs: f64,
    pub keep_original: bool,
    pub budget: DiskBudget,
}

impl NormalizationSettings {
    pub fn new(enabled: bool, target_lufs: f64, keep_original: bool, reserve_gib: u64) -> Self {
        Self {
            enabled,
            target_lufs,
            keep_original,
            budget: DiskBudget::from_reserve_gib(reserve_gib),
        }
    }

    /// 补传发现原片已被就地替换时，用它关掉标准化而保留其余设置。
    pub fn with_enabled(self, enabled: bool) -> Self {
        Self { enabled, ..self }
    }
}

/// 标准化可以吃掉多少磁盘。
///
/// 两道水位都失败开放：探测不出可用空间时一律放行，宁可让标准化照常跑，也不要因为读不到
/// 一个数字就静默停掉一个功能。
#[derive(Clone, Copy)]
pub struct DiskBudget {
    /// 任何时候都要留给系统的字节数。
    reserve_bytes: u64,
    probe: fn(&Path) -> Option<u64>,
    check_interval: Duration,
}

impl std::fmt::Debug for DiskBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskBudget")
            .field("reserve_bytes", &self.reserve_bytes)
            .finish_non_exhaustive()
    }
}

impl DiskBudget {
    pub fn from_reserve_gib(reserve_gib: u64) -> Self {
        Self {
            reserve_bytes: reserve_gib.saturating_mul(1024 * 1024 * 1024),
            probe: crate::server::common::disk_space::available_bytes,
            check_interval: DISK_CHECK_INTERVAL,
        }
    }

    /// 够不够开始转码。`Ok(())` 表示放行——包括探测不出可用空间的情况。
    fn admits(&self, directory: &Path, input_bytes: u64) -> Result<(), (u64, u64)> {
        let Some(available) = (self.probe)(directory) else {
            return Ok(());
        };
        let required = (input_bytes as f64 * OUTPUT_SIZE_FACTOR) as u64 + self.reserve_bytes;
        if available < required {
            return Err((available, required));
        }
        Ok(())
    }

    #[cfg(test)]
    fn with_probe(reserve_bytes: u64, probe: fn(&Path) -> Option<u64>) -> Self {
        Self {
            reserve_bytes,
            probe,
            // 用例不该为了看一次水位触发而真的等十秒。
            check_interval: Duration::from_millis(5),
        }
    }

    /// 探测不出可用空间的 budget：两道水位都放行，用于关心其它行为的用例。
    #[cfg(test)]
    fn unlimited() -> Self {
        Self {
            reserve_bytes: 0,
            probe: |_| None,
            check_interval: Duration::from_millis(5),
        }
    }

    /// 在可用空间跌破保留线时返回。探测不出来就永远挂起，让转码自然跑完。
    ///
    /// 判据里不含安全系数：此刻产物已经在写，要守的是最后那道保留线。
    async fn wait_for_pressure(&self, directory: &Path) -> u64 {
        loop {
            tokio::time::sleep(self.check_interval).await;
            if let Some(available) = (self.probe)(directory)
                && available < self.reserve_bytes
            {
                return available;
            }
        }
    }
}

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
    /// 准入时可用空间就不够放下产物，没有启动 ffmpeg。
    DiskAdmissionDenied,
    /// 转码途中可用空间跌破保留线，ffmpeg 已被取消、半成品已删。
    DiskPressureAborted,
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
    budget: DiskBudget,
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
    // 准入判断放在拿到 permit 之后：排队期间可用空间会变，排队前算出来的结论到执行时
    // 已经过期。
    let directory = source.parent().unwrap_or_else(|| Path::new("."));
    if let Err((available_bytes, required_bytes)) = budget.admits(directory, input_bytes) {
        warn!(
            audio_normalization = "skipped",
            reason = "disk_admission_denied",
            file = %source.display(),
            available_bytes,
            required_bytes,
            "not enough free space for a normalization output; uploading the original"
        );
        return NormalizationOutcome::Original {
            reason: OriginalReason::DiskAdmissionDenied,
        };
    }
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
    // 磁盘可能在转码途中被别的东西吃光。`select!` 落地即 drop 掉转码 future，
    // `kill_on_drop(true)` 随之杀掉 ffmpeg，`.part` 由下面的 cleanup 删掉。
    let transcoded = tokio::select! {
        result = runner.transcode(source, artifact.path(), target, &measurement) => result.map_err(Some),
        available_bytes = budget.wait_for_pressure(directory) => {
            warn!(
                audio_normalization = "aborted",
                reason = "disk_pressure",
                file = %source.display(),
                available_bytes,
                reserve_bytes = budget.reserve_bytes,
                "free space fell below the reserve mid-transcode; cancelling and uploading the original"
            );
            Err(None)
        }
    };
    if let Err(error) = transcoded {
        artifact.cleanup().await;
        let Some(error) = error else {
            return NormalizationOutcome::Original {
                reason: OriginalReason::DiskPressureAborted,
            };
        };
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

pub async fn maybe_capture_reference_sample(
    source: &Path,
    store: &AudioSampleStore,
    budget: DiskBudget,
) {
    let claim = match store.try_claim_capture().await {
        Ok(Some(v)) => v,
        Ok(None) => return,
        Err(error) => {
            warn!(?error, "failed to claim sample capture");
            return;
        }
    };
    if let Err(error) = create_reference_sample(source, store, &claim, budget).await {
        warn!(file=%source.display(), ?error, "sample capture failed; will retry next segment");
        let _ = store.retry_later(claim).await;
    }
}

async fn create_reference_sample(
    source: &Path,
    store: &AudioSampleStore,
    claim: &CaptureClaim,
    budget: DiskBudget,
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
        normalize_for_upload(raw_artifact.path(), BASE_TARGET_LUFS, &runner, true, budget).await;
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
        /// 转码永不结束，用来观察硬水位能不能把它掐掉。
        transcode_hangs: bool,
        measured: Arc<Mutex<bool>>,
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
                transcode_hangs: false,
                measured: Arc::new(Mutex::new(false)),
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
            *self.measured.lock().unwrap() = true;
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
            if self.transcode_hangs {
                std::future::pending::<()>().await;
            }
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
        let outcome = normalize_for_upload(&source, -16.0, &FakeRunner::clean(), true, DiskBudget::unlimited()).await;
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
            normalize_for_upload(&source, -16.0, &runner, false, DiskBudget::unlimited()).await,
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
            normalize_for_upload(&source, -16.0, &runner, false, DiskBudget::unlimited()).await,
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
            normalize_for_upload(&source, -16.0, &FakeRunner::failing(), false, DiskBudget::unlimited()).await,
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

        let outcome = normalize_for_upload(&source, -16.0, &runner, false, DiskBudget::unlimited()).await;

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

            let outcome = normalize_for_upload(&source, -16.0, &runner, false, DiskBudget::unlimited()).await;

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
            normalize_for_upload(&source, -16.0, &runner, false, DiskBudget::unlimited()).await,
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

        let outcome = normalize_for_upload(&source, -16.0, &runner, false, DiskBudget::unlimited()).await;

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
    async fn admission_refuses_to_start_when_the_output_would_not_fit() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("segment.flv");
        tokio::fs::write(&source, SOURCE_BYTES).await.unwrap();
        let runner = FakeRunner::clean();
        let measured = runner.measured.clone();
        // 原片 × 1.1 还差得远，更别说保留线。
        let budget = DiskBudget::with_probe(1024 * 1024 * 1024, |_| Some(1024));

        let outcome = normalize_for_upload(&source, -16.0, &runner, false, budget).await;

        assert!(matches!(
            outcome,
            NormalizationOutcome::Original {
                reason: OriginalReason::DiskAdmissionDenied
            }
        ));
        assert!(
            !*measured.lock().unwrap(),
            "admission must run before the measure pass, not after paying for it"
        );
        assert_eq!(tokio::fs::read(&source).await.unwrap(), SOURCE_BYTES);
        assert!(leftover_artifacts(dir.path()).await.is_empty());
    }

    #[tokio::test]
    async fn admission_passes_when_space_is_plentiful() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("segment.flv");
        tokio::fs::write(&source, SOURCE_BYTES).await.unwrap();
        let budget = DiskBudget::with_probe(1024, |_| Some(64 * 1024 * 1024 * 1024));

        assert!(matches!(
            normalize_for_upload(&source, -16.0, &FakeRunner::clean(), false, budget).await,
            NormalizationOutcome::Normalized { .. }
        ));
    }

    /// 探测不出可用空间时放行——平台能力缺失不该静默停掉一个功能。
    #[tokio::test]
    async fn an_unreadable_filesystem_does_not_block_normalization() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("segment.flv");
        tokio::fs::write(&source, SOURCE_BYTES).await.unwrap();
        let runner = FakeRunner::clean();
        let measured = runner.measured.clone();
        let budget = DiskBudget::with_probe(u64::MAX, |_| None);

        let outcome = normalize_for_upload(&source, -16.0, &runner, false, budget).await;

        assert!(matches!(
            outcome,
            NormalizationOutcome::Normalized { .. }
        ));
        assert!(*measured.lock().unwrap());
    }

    #[tokio::test]
    async fn disk_pressure_mid_transcode_cancels_and_falls_back_to_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("segment.flv");
        tokio::fs::write(&source, SOURCE_BYTES).await.unwrap();
        let runner = FakeRunner {
            transcode_hangs: true,
            ..FakeRunner::default()
        };
        // 准入那一次读到宽裕，转码开始后磁盘被别的东西吃光。函数指针存不下状态，用一个
        // 只属于本用例的计数器区分这两个阶段。
        static READS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        fn probe(_: &Path) -> Option<u64> {
            if READS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                Some(64 * 1024 * 1024 * 1024)
            } else {
                Some(1024)
            }
        }
        let budget = DiskBudget::with_probe(4096, probe);

        let outcome = normalize_for_upload(&source, -16.0, &runner, false, budget).await;

        assert!(matches!(
            outcome,
            NormalizationOutcome::Original {
                reason: OriginalReason::DiskPressureAborted
            }
        ));
        assert_eq!(
            tokio::fs::read(&source).await.unwrap(),
            SOURCE_BYTES,
            "an aborted transcode must not touch the original"
        );
        assert!(
            leftover_artifacts(dir.path()).await.is_empty(),
            "the cancelled transcode left its .part behind"
        );
        assert_eq!(active_artifacts_under(dir.path()), 0);
    }

    /// 需要本地 ffmpeg；手动运行：
    /// `cargo test -p biliup-cli system_ffmpeg_replaces -- --ignored --nocapture`
    ///
    /// 单测都用 `FakeRunner`，证明不了真实 ffmpeg 的产物能过校验、能被替换上去。这条用
    /// 一段合成的低响度素材跑完整条链路，并核对替换后的原片路径确实是 48 kHz AAC、整段
    /// 响度落在目标附近。
    #[tokio::test]
    #[ignore]
    async fn system_ffmpeg_replaces_the_original_with_a_normalized_recording() {
        // FLV 才是服务端录制的默认容器（`StreamGears` 自研解析逐 tag 写 FLV），MP4 只是
        // 其它下载器的路径。两个都测，否则就是拿一条没人走的路当验收。
        for container in ["flv", "mp4"] {
            smoke_one_container(container).await;
        }
    }

    async fn smoke_one_container(container: &str) {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join(format!("smoke.{container}"));
        let status = tokio::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=6:size=320x240:rate=10",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=6",
                "-filter:a",
                // 明显偏小的输入，好看出标准化确实抬了响度。
                "volume=-24dB",
                "-c:v",
                "libx264",
                "-c:a",
                "aac",
                "-b:a",
                "64k",
                "-ar",
                "44100",
                "-shortest",
            ])
            .arg(&source)
            .status()
            .await
            .expect("spawn ffmpeg");
        assert!(status.success(), "{container}: fixture generation failed");
        let original_bytes = tokio::fs::metadata(&source).await.unwrap().len();

        let outcome = normalize_for_upload(
            &source,
            BASE_TARGET_LUFS,
            &SystemAudioFfmpeg::default(),
            false,
            DiskBudget::from_reserve_gib(1),
        )
        .await;

        assert!(
            matches!(
                outcome,
                NormalizationOutcome::Normalized {
                    form: NormalizedForm::ReplacedOriginal,
                    ..
                }
            ),
            "{container}: unexpected outcome: {outcome:?}"
        );
        assert!(
            leftover_artifacts(dir.path()).await.is_empty(),
            "{container}: a .part survived the replacement"
        );

        let probe = tokio::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "a:0",
                "-show_entries",
                "stream=codec_name,sample_rate",
                "-of",
                "csv=p=0",
            ])
            .arg(&source)
            .output()
            .await
            .unwrap();
        let audio = String::from_utf8_lossy(&probe.stdout);
        assert!(
            audio.contains("aac") && audio.contains("48000"),
            "{container}: replaced file should carry the normalized audio, got {audio:?}"
        );

        // 复测替换后的文件，确认整段响度已经收敛到目标附近。
        let scan = SystemAudioFfmpeg::default()
            .measure(&source, LoudnessTarget(BASE_TARGET_LUFS))
            .await
            .unwrap();
        let after = parse_loudnorm_measurement(&scan.stderr).unwrap();
        println!(
            "smoke[{container}]: {original_bytes} bytes -> {} bytes, measured {} LUFS",
            tokio::fs::metadata(&source).await.unwrap().len(),
            after.input_i
        );
        assert!(
            (after.input_i - BASE_TARGET_LUFS).abs() <= 1.5,
            "{container}: normalized loudness {} is not near {BASE_TARGET_LUFS}",
            after.input_i
        );
    }

    /// 06 号验收里本机可跑的那一半：多路并发下，标准化带来的额外磁盘占用任何时刻不超过
    /// 一份分段。这条是跨管道的整体性质，`NORMALIZE_SLOTS` 与就地替换缺一不可，单测证明
    /// 不了——必须真的并发跑起来盯着目录。
    ///
    /// 需要本地 ffmpeg；手动运行：
    /// `cargo test -p biliup-cli concurrent_normalization -- --ignored --nocapture`
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore]
    async fn concurrent_normalization_never_keeps_more_than_one_artifact() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        // 四种音频配置，用来看产物/原片比值随音频码率怎么变——准入水位的
        // `OUTPUT_SIZE_FACTOR` 就是按这个比值定的。
        let variants = [("64k", "44100"), ("128k", "48000"), ("192k", "48000"), ("320k", "48000")];
        let mut sources = Vec::new();
        for (index, (bitrate, rate)) in variants.iter().enumerate() {
            let source = dir.path().join(format!("segment-{index}.mp4"));
            let status = tokio::process::Command::new("ffmpeg")
                .args([
                    "-y", "-f", "lavfi", "-i",
                    "testsrc=duration=15:size=640x480:rate=15",
                    "-f", "lavfi", "-i", "sine=frequency=440:duration=15",
                    "-filter:a", "volume=-24dB",
                    "-c:a", "aac", "-b:a", bitrate, "-ar", rate, "-shortest",
                ])
                .arg(&source)
                .status()
                .await
                .expect("spawn ffmpeg");
            assert!(status.success());
            let bytes = tokio::fs::metadata(&source).await.unwrap().len();
            sources.push((source, bytes, *bitrate));
        }

        // 采样器：并发跑起来之后，目录里同时存在的 `.part` 最多有几份。
        let peak = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let sampler = {
            let (peak, stop, directory) = (peak.clone(), stop.clone(), dir.path().to_path_buf());
            tokio::spawn(async move {
                while !stop.load(Ordering::Relaxed) {
                    let count = leftover_artifacts(&directory).await.len();
                    peak.fetch_max(count, Ordering::Relaxed);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
        };

        let started = std::time::Instant::now();
        let mut running = Vec::new();
        for (source, _, _) in &sources {
            let source = source.clone();
            running.push(tokio::spawn(async move {
                normalize_for_upload(
                    &source,
                    BASE_TARGET_LUFS,
                    &SystemAudioFfmpeg::default(),
                    false,
                    DiskBudget::from_reserve_gib(1),
                )
                .await
            }));
        }
        let outcomes = futures::future::join_all(running).await;
        let elapsed = started.elapsed();
        stop.store(true, Ordering::Relaxed);
        sampler.await.unwrap();

        for (outcome, (source, _, _)) in outcomes.into_iter().zip(&sources) {
            let outcome = outcome.unwrap();
            assert!(
                matches!(
                    outcome,
                    NormalizationOutcome::Normalized {
                        form: NormalizedForm::ReplacedOriginal,
                        ..
                    }
                ),
                "{} got {outcome:?}",
                source.display()
            );
        }

        println!("\n并发 {} 段，历时 {:.1}s", sources.len(), elapsed.as_secs_f64());
        println!("{:<8} {:>12} {:>12} {:>8}", "音频", "原片", "产物", "倍率");
        let mut worst_ratio = 0.0_f64;
        for (source, before, bitrate) in &sources {
            let after = tokio::fs::metadata(source).await.unwrap().len();
            let ratio = after as f64 / *before as f64;
            worst_ratio = worst_ratio.max(ratio);
            println!("{bitrate:<8} {before:>12} {after:>12} {ratio:>8.2}");
        }
        println!(
            "最大倍率 {worst_ratio:.2}，准入用的 OUTPUT_SIZE_FACTOR = {OUTPUT_SIZE_FACTOR}"
        );
        println!(
            "⚠️ 这组倍率不能外推到真实录像：合成素材的视频只有几十 kbps，音频占了大头，\n\
             而真实直播录像是 Mbps 级视频 + 192k 音频，音频占比 <2%，重编再怎么变都动不了\n\
             总大小几个百分点。这里能看的是趋势——音频占比越高，固定系数越会低估。"
        );

        let peak = peak.load(Ordering::Relaxed);
        println!("并发期间同时存在的中间件峰值：{peak} 份");
        assert!(
            peak <= 1,
            "额外磁盘占用超过一份分段：峰值 {peak} 份中间件同时存在"
        );
        assert!(
            leftover_artifacts(dir.path()).await.is_empty(),
            "跑完之后还有 .part 残留"
        );
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
