use chrono::{DateTime, Local};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use std::time::Duration;
use tracing::{error, info, warn};

pub type CallbackFn<'a> =
    Box<dyn FnMut(&str, SegmentCloseReason, SegmentIdentity) + Send + Sync + 'a>;

/// Native structured events are routed by target, never by level or message text; the old sinks
/// filter this target out so their output is unchanged.
pub(crate) const EVENT_TARGET: &str = "biliup::event";

/// Who owns this recording, supplied by the caller: the server passes room and session ids,
/// standalone commands pass their task id. Identity is never inferred from the file name.
#[derive(Debug, Clone, Default)]
pub struct RecordingOwner {
    pub live_streamer_id: Option<String>,
    pub streamer_info_id: Option<String>,
    pub task_id: Option<String>,
    pub download_attempt_id: Option<String>,
}

impl RecordingOwner {
    pub fn live_streamer_id(&self) -> &str {
        self.live_streamer_id.as_deref().unwrap_or("")
    }
    pub fn streamer_info_id(&self) -> &str {
        self.streamer_info_id.as_deref().unwrap_or("")
    }
    pub fn task_id(&self) -> &str {
        self.task_id.as_deref().unwrap_or("")
    }
    pub fn download_attempt_id(&self) -> &str {
        self.download_attempt_id.as_deref().unwrap_or("")
    }
}

/// Assigned when the file is created, not when it closes, so mid-segment events can already name
/// the segment. The id stays with the original file through close, enrollment and upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentIdentity {
    pub segment_id: String,
    pub original_file: String,
}

static SEGMENT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Unique without a coordinator: process-local sequence plus wall clock plus randomness, so two
/// processes recording the same room cannot collide. ASCII only, well inside the 128 byte limit.
pub fn allocate_id(prefix: &str) -> String {
    let sequence = SEGMENT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let salt: u32 = rand::thread_rng().r#gen();
    format!("{prefix}-{now:x}-{sequence:x}-{salt:08x}")
}

pub fn allocate_segment_id() -> String {
    allocate_id("seg")
}

/// The frozen v1 reason vocabulary. New codes must be added to the contract first, so an unmapped
/// reason stays `unknown` instead of inventing free text.
pub fn close_reason_code(reason: SegmentCloseReason) -> &'static str {
    match reason {
        SegmentCloseReason::TimedSplit | SegmentCloseReason::SizeSplit => "split_limit",
        SegmentCloseReason::StreamEnded => "stream_end",
        SegmentCloseReason::TransportError => "transport_error",
        SegmentCloseReason::Cancelled => "user_cancel",
        SegmentCloseReason::Unknown => "unknown",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SegmentCloseReason {
    TimedSplit,
    SizeSplit,
    StreamEnded,
    TransportError,
    Cancelled,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct SegmentCloseHandle(Arc<Mutex<SegmentCloseReason>>);

impl Default for SegmentCloseHandle {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(SegmentCloseReason::Unknown)))
    }
}

impl SegmentCloseHandle {
    pub fn set(&self, reason: SegmentCloseReason) {
        if let Ok(mut current) = self.0.lock() {
            *current = reason;
        }
    }

    pub fn get(&self) -> SegmentCloseReason {
        self.0
            .lock()
            .map(|reason| *reason)
            .unwrap_or(SegmentCloseReason::Unknown)
    }
}

#[derive(Debug)]
pub enum Segment {
    Time(Duration, Duration),
    Size(u64, u64),
    Never,
}

#[derive(Debug, Clone)]
pub struct Segmentable {
    time: Time,
    size: Size,
}

#[derive(Debug, Clone)]
struct Time {
    expected: Option<Duration>,
    start: Duration,
    current: Duration,
}

#[derive(Debug, Clone)]
struct Size {
    expected: Option<u64>,
    current: u64,
}

impl Segmentable {
    pub fn new(expected_time: Option<Duration>, expected_size: Option<u64>) -> Self {
        Self {
            time: Time {
                expected: expected_time,
                start: Duration::ZERO,
                current: Duration::ZERO,
            },
            size: Size {
                expected: expected_size,
                current: 0,
            },
        }
    }

    /// 检查是否需要分割 - 只要时间或大小任一条件满足就返回 true
    pub fn needed(&self) -> bool {
        let time_exceeded = self.time_needed();
        let size_exceeded = self.size_needed();
        let result = time_exceeded || size_exceeded;

        // 添加调试信息
        if result {
            self.log_segmentation_reason(time_exceeded, size_exceeded);
        }

        result
    }

    fn elapsed_time(&self) -> Duration {
        self.time.current.saturating_sub(self.time.start)
    }

    /// 检查单独的时间条件
    pub fn time_needed(&self) -> bool {
        if let Some(expected_time) = self.time.expected {
            self.elapsed_time() >= expected_time
        } else {
            false
        }
    }

    /// 检查单独的大小条件
    pub fn size_needed(&self) -> bool {
        if let Some(expected_size) = self.size.expected {
            self.size.current >= expected_size
        } else {
            false
        }
    }

    /// 记录分割原因的调试信息
    fn log_segmentation_reason(&self, time_exceeded: bool, size_exceeded: bool) {
        match (time_exceeded, size_exceeded) {
            (true, true) => {
                tracing::info!(
                    "Segmentation needed: Both time ({:?} >= {:?}) and size ({} >= {}) conditions met",
                    self.elapsed_time(),
                    self.time.expected.unwrap(),
                    self.size.current,
                    self.size.expected.unwrap()
                );
            }
            (true, false) => {
                tracing::info!(
                    "Segmentation needed: Time condition met ({:?} >= {:?})",
                    self.elapsed_time(),
                    self.time.expected.unwrap()
                );
            }
            (false, true) => {
                tracing::info!(
                    "Segmentation needed: Size condition met ({} >= {})",
                    self.size.current,
                    self.size.expected.unwrap()
                );
            }
            (false, false) => {} // 不应该到达这里，因为只有在需要分割时才调用
        }
    }

    /// 获取分割原因的描述
    pub fn get_segment_reason(&self) -> String {
        let time_exceeded = self.time_needed();
        let size_exceeded = self.size_needed();

        match (time_exceeded, size_exceeded) {
            (true, true) => "Time and size limits reached".to_string(),
            (true, false) => "Time limit reached".to_string(),
            (false, true) => "Size limit reached".to_string(),
            (false, false) => "No segmentation needed".to_string(),
        }
    }

    pub fn increase_time(&mut self, number: Duration) {
        self.time.current += number
    }

    pub fn set_time_position(&mut self, number: Duration) {
        self.time.current = number
    }

    pub fn set_start_time(&mut self, number: Duration) {
        self.time.start = number
    }

    pub fn increase_size(&mut self, number: u64) {
        self.size.current += number
    }

    pub fn set_size_position(&mut self, number: u64) {
        self.size.current = number
    }

    /// 重置计数器，通常在创建新分割后调用
    pub fn reset(&mut self) {
        self.size.current = 0;
        self.time.start = self.time.current; // 保持当前时间位置，但重置起始点
    }

    /// 完全重置所有状态
    pub fn full_reset(&mut self) {
        self.size.current = 0;
        self.time.current = Duration::ZERO;
        self.time.start = Duration::ZERO;
    }

    /// 格式化进度信息的通用方法
    fn format_progress<T>(
        label: &str,
        current: T,
        expected: Option<T>,
        unit: &str,
        format_fn: impl Fn(T) -> String,
    ) -> String
    where
        T: Copy + Into<f64>,
    {
        if let Some(expected_val) = expected {
            let current_f64 = current.into();
            let expected_f64 = expected_val.into();
            let percentage = (current_f64 / expected_f64 * 100.0).min(100.0);
            format!(
                "{}: {}/{} {} ({:.1}%)",
                label,
                format_fn(current),
                format_fn(expected_val),
                unit,
                percentage
            )
        } else {
            format!("{}: No limit", label)
        }
    }

    /// 获取当前状态信息
    pub fn get_status(&self) -> String {
        let time_info = Self::format_progress(
            "Time",
            self.elapsed_time().as_secs_f64(),
            self.time.expected.map(|d| d.as_secs_f64()),
            "s",
            |t| format!("{:.1}", t),
        );

        let size_info = Self::format_progress(
            "Size",
            self.size.current as f64,
            self.size.expected.map(|s| s as f64),
            "bytes",
            |s| format!("{}", s as u64),
        );

        format!("{}, {}", time_info, size_info)
    }
}

impl Default for Segmentable {
    fn default() -> Self {
        Segmentable {
            time: Time {
                expected: None,
                start: Duration::ZERO,
                current: Duration::ZERO,
            },
            size: Size {
                expected: None,
                current: 0,
            },
        }
    }
}

pub struct LifecycleFile<'a> {
    pub fmt_file_name: String,
    pub file_name: String,
    pub path: PathBuf,
    pub hook: CallbackFn<'a>,
    pub extension: &'static str,
    active: bool,
    close_handle: SegmentCloseHandle,
    owner: RecordingOwner,
    identity: Option<SegmentIdentity>,
}

impl<'a> LifecycleFile<'a> {
    pub fn new(fmt_file_name: &str, extension: &'static str) -> Self {
        Self::with_hook(fmt_file_name, extension, |_, _, _| {})
    }

    pub fn with_hook<F>(fmt_file_name: &str, extension: &'static str, hook: F) -> Self
    where
        F: FnMut(&str, SegmentCloseReason, SegmentIdentity) + Send + Sync + 'a,
    {
        Self::with_hook_and_close_handle(
            fmt_file_name,
            extension,
            SegmentCloseHandle::default(),
            hook,
        )
    }

    pub fn with_hook_and_close_handle<F>(
        fmt_file_name: &str,
        extension: &'static str,
        close_handle: SegmentCloseHandle,
        hook: F,
    ) -> Self
    where
        F: FnMut(&str, SegmentCloseReason, SegmentIdentity) + Send + Sync + 'a,
    {
        Self {
            fmt_file_name: fmt_file_name.to_string(),
            file_name: "".to_string(),
            path: Default::default(),
            hook: Box::new(hook),
            extension,
            active: false,
            close_handle,
            owner: RecordingOwner::default(),
            identity: None,
        }
    }

    /// The caller owns the business identity; the file layer only carries it onto its own events.
    pub fn with_owner(mut self, owner: RecordingOwner) -> Self {
        self.owner = owner;
        self
    }

    /// Identity of the file currently open, available from the moment it is created.
    pub fn identity(&self) -> Option<&SegmentIdentity> {
        self.identity.as_ref()
    }

    pub fn owner(&self) -> &RecordingOwner {
        &self.owner
    }

    pub fn create(&mut self) -> Result<&Path, std::io::Error> {
        // 构建最终文件名
        self.file_name = format!(
            "{}.{}",
            format_filename(&self.fmt_file_name),
            self.extension
        );

        // 构建临时文件路径（带 .part 后缀）
        self.path = PathBuf::from(&self.file_name);
        self.path.set_extension(format!("{}.part", self.extension));

        // 确保父目录存在
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        info!("Save to {}", self.path.display());
        self.active = true;
        let identity = SegmentIdentity {
            segment_id: allocate_segment_id(),
            original_file: self.file_name.clone(),
        };
        tracing::info!(
            target: EVENT_TARGET,
            event_name = "recording.segment_created",
            outcome = "executed",
            segment_id = identity.segment_id,
            original_file = identity.original_file,
            live_streamer_id = self.owner.live_streamer_id(),
            streamer_info_id = self.owner.streamer_info_id(),
            task_id = self.owner.task_id(),
            download_attempt_id = self.owner.download_attempt_id(),
            "开始写入新的录制分段"
        );
        self.identity = Some(identity);
        Ok(self.path.as_path())
    }

    pub fn finalize(&mut self, reason: SegmentCloseReason) -> Result<(), std::io::Error> {
        if !self.active {
            return Ok(());
        }
        // 去掉 .part 后缀
        match fs::rename(&self.path, &self.file_name) {
            Ok(_) => {
                self.active = false;
                // Identity is allocated by create(); a missing one means the file was never
                // opened through this type, so the event says unknown instead of guessing.
                let identity = self.identity.take().unwrap_or_else(|| SegmentIdentity {
                    segment_id: String::new(),
                    original_file: self.file_name.clone(),
                });
                let size_bytes = fs::metadata(&self.file_name).map(|m| m.len()).unwrap_or(0);
                tracing::info!(
                    target: EVENT_TARGET,
                    event_name = "recording.segment_closed",
                    outcome = "executed",
                    reason_code = close_reason_code(reason),
                    segment_id = identity.segment_id,
                    original_file = identity.original_file,
                    size_bytes,
                    live_streamer_id = self.owner.live_streamer_id(),
                    streamer_info_id = self.owner.streamer_info_id(),
                    task_id = self.owner.task_id(),
                    download_attempt_id = self.owner.download_attempt_id(),
                    "录制分段已关闭"
                );
                (self.hook)(&self.file_name, reason, identity);
                Ok(())
            }
            Err(e) => {
                // The file stays active: identity is kept so a later retry still names it.
                let identity = self.identity.as_ref();
                warn!(
                    target: EVENT_TARGET,
                    event_name = "recording.segment_closed",
                    outcome = "failed",
                    reason_code = "unknown",
                    segment_id = identity.map(|i| i.segment_id.as_str()).unwrap_or(""),
                    original_file = self.file_name,
                    error = format!("{e}"),
                    live_streamer_id = self.owner.live_streamer_id(),
                    streamer_info_id = self.owner.streamer_info_id(),
                    task_id = self.owner.task_id(),
                    download_attempt_id = self.owner.download_attempt_id(),
                    "录制分段收尾失败，临时文件保留"
                );
                error!("finalize {} {e}", self.path.display());
                Err(e)
            }
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn fallback_reason(&self, default: SegmentCloseReason) -> SegmentCloseReason {
        match self.close_handle.get() {
            SegmentCloseReason::Unknown => default,
            reason => reason,
        }
    }
}

pub fn format_filename(file_name: &str) -> String {
    let local: DateTime<Local> = Local::now();
    // let time_str = local.format("%Y-%m-%dT%H_%M_%S");
    let time_str = local.format(file_name);
    // format!("{file_name}{time_str}")
    time_str.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn it_works() -> Result<(), Box<dyn std::error::Error>> {
        let mut p = PathBuf::from("/feel/the");

        p.set_extension("force");
        assert_eq!(Path::new("/feel/the.force"), p.as_path());

        p.set_extension("");
        assert_eq!(Path::new("/feel/the"), p.as_path());

        Ok(())
    }

    #[test]
    fn test_segmentation_logic() -> Result<(), Box<dyn std::error::Error>> {
        // 测试时间分割
        let mut seg = Segmentable::new(Some(Duration::from_secs(10)), None);
        assert!(!seg.needed());

        seg.increase_time(Duration::from_secs(15));
        assert!(seg.needed());
        assert!(seg.time_needed());
        assert!(!seg.size_needed());

        // 测试大小分割
        let mut seg = Segmentable::new(None, Some(1024));
        assert!(!seg.needed());

        seg.increase_size(2048);
        assert!(seg.needed());
        assert!(!seg.time_needed());
        assert!(seg.size_needed());

        // 测试双重条件
        let mut seg = Segmentable::new(Some(Duration::from_secs(10)), Some(1024));
        assert!(!seg.needed());

        // 只满足时间条件
        seg.increase_time(Duration::from_secs(15));
        assert!(seg.needed());

        // 重置并只满足大小条件
        seg.full_reset();
        seg.increase_size(2048);
        assert!(seg.needed());

        // 同时满足两个条件
        seg.increase_time(Duration::from_secs(15));
        assert!(seg.needed());
        assert!(seg.time_needed());
        assert!(seg.size_needed());

        Ok(())
    }

    #[test]
    fn cancellation_close_reason_is_shared_with_active_file() {
        let handle = SegmentCloseHandle::default();
        let file = LifecycleFile::with_hook_and_close_handle(
            "unused",
            "flv",
            handle.clone(),
            |_, _, _| {},
        );
        handle.set(SegmentCloseReason::Cancelled);
        assert_eq!(
            file.fallback_reason(SegmentCloseReason::TransportError),
            SegmentCloseReason::Cancelled
        );
    }
}
