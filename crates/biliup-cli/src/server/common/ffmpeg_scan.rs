//! 全片扫描类 ffmpeg 调用的 stderr 处理：边读边判时间戳异常，只保留尾部窗口。
//!
//! 这类扫描用 `-loglevel verbose`，一段时间戳大面积异常的长录像能产出几百 MB stderr，
//! 而 `Command::output()` 会把它整个收进内存。这里改成流式：异常模式是行内模式，边读边
//! 匹配即可；常驻内存只有一个尾部窗口，loudnorm 的 JSON 在 ffmpeg 退出前才打印，正好
//! 落在窗口内。

use biliup_observability::{Context as EventContext, DiagnosticCapture, Fields};
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// 保留的 stderr 尾部字节数。够装下 loudnorm 的 JSON 摘要和结尾统计。
pub const STDERR_TAIL_LIMIT: usize = 64 * 1024;

/// stderr 中命中任一模式即判为时间戳异常（用具体模式，避免宽泛词误判）。
pub fn stderr_indicates_anomaly(stderr: &str) -> bool {
    const PATTERNS: &[&str] = &[
        "Non-monotonic DTS",
        "non monotonically increasing dts",
        "timestamp discontinuity",
        "Invalid timestamp",
        "Application provided invalid",
    ];
    PATTERNS.iter().any(|p| stderr.contains(p))
}

/// 从一行时间戳异常里解出「倒退了多少」。单位是该行数值自身的单位——`-f null` 的 copy
/// 扫描下 muxer 用输入流的 timebase，直录 FLV 是 1/1000，即毫秒。
///
/// 认两种写法，都是 muxer 报的：
///
/// ```text
/// ... non monotonically increasing dts to muxer in stream 0: 11990 >= 8356
/// Non-monotonic DTS in output stream 0:1; previous: 11990, current: 8356;
/// ```
///
/// 解不出来返回 `None`。调用方必须把 `None` 当作「回退量未知」保守处理，不能当作 0——
/// 一个认不出的新写法不是「没有回退」。
pub fn parse_backward_ms(line: &str) -> Option<i64> {
    let (previous, current) = if let Some((head, tail)) = line.split_once(" >= ") {
        // "... increasing dts to muxer in stream 0: 11990 >= 8356"
        (head.rsplit(':').next()?, tail)
    } else if let Some((_, tail)) = line.split_once("previous:") {
        // "...; previous: 11990, current: 8356;"
        let (previous, rest) = tail.split_once(',')?;
        (previous, rest.split_once("current:")?.1)
    } else {
        return None;
    };
    let delta = leading_number(previous)? - leading_number(current)?;
    (delta > 0).then_some(delta)
}

/// 取一段文本里的第一个整数，容忍前导空白和尾部的其它字符。
fn leading_number(text: &str) -> Option<i64> {
    let text = text.trim_start();
    let negative = text.starts_with('-');
    let digits: String = text
        .trim_start_matches('-')
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let value: i64 = digits.parse().ok()?;
    Some(if negative { -value } else { value })
}

pub struct StderrScan {
    /// stderr 的尾部窗口。
    pub tail: String,
    /// 扫描过程中是否命中过时间戳异常模式（覆盖全部输出，不止尾部窗口）。
    pub timestamp_anomaly: bool,
    /// 命中的异常行里解出的最大单次回退量，`None` 表示一条都没解出来。
    pub max_backward_ms: Option<i64>,
    /// 命中的异常行数。
    pub anomaly_lines: u64,
}

/// Native diagnostic metadata for one ffmpeg call.  The source file is optional because some
/// process-wide hooks do not belong to a recorded segment.  `tee_stderr` preserves call sites
/// which previously inherited ffmpeg's stderr directly.
#[derive(Debug, Clone, Copy)]
pub struct ScanObserver<'a> {
    pub stage: &'a str,
    pub original_file: Option<&'a Path>,
    pub tee_stderr: bool,
}

impl<'a> ScanObserver<'a> {
    pub fn quiet(stage: &'a str, original_file: &'a Path) -> Self {
        Self {
            stage,
            original_file: Some(original_file),
            tee_stderr: false,
        }
    }
}

fn context(original_file: Option<&Path>) -> EventContext {
    let mut fields = Fields::new();
    if let Some(path) = original_file {
        fields.insert("original_file", path.display().to_string().into());
    }
    EventContext(fields)
}

fn report_failure(
    observer: ScanObserver<'_>,
    capture: Option<DiagnosticCapture>,
    code: Option<i32>,
) {
    crate::observe::external::command_failed(
        observer.stage,
        "process_failed",
        context(observer.original_file),
        capture.map(|capture| capture.finish(code)),
        code,
    );
}

/// 跑一个全片扫描 ffmpeg 并流式消费它的 stderr。
pub async fn run_scanning_stderr(
    command: &mut Command,
    observer: ScanObserver<'_>,
) -> std::io::Result<(ExitStatus, StderrScan)> {
    let mut child = match command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            report_failure(observer, None, None);
            return Err(error);
        }
    };
    let stderr = child.stderr.take().expect("stderr piped");
    let mut reader = BufReader::new(stderr);
    let mut diagnostic = DiagnosticCapture::new();
    let mut legacy_stderr = tokio::io::stderr();
    let mut line = Vec::new();
    let mut scan = StderrScan {
        tail: String::new(),
        timestamp_anomaly: false,
        max_backward_ms: None,
        anomaly_lines: 0,
    };
    loop {
        line.clear();
        // 按字节读：ffmpeg 偶尔会在日志里吐出非 UTF-8 片段，`lines()` 会因此直接报错。
        let read = match reader.read_until(b'\n', &mut line).await {
            Ok(read) => read,
            Err(error) => {
                report_failure(observer, Some(diagnostic), None);
                return Err(error);
            }
        };
        if read == 0 {
            break;
        }
        diagnostic.push(&line);
        if observer.tee_stderr {
            // Inherited stderr was best effort as well; a closed terminal must not change the
            // command's business result.
            let _ = legacy_stderr.write_all(&line).await;
        }
        let text = String::from_utf8_lossy(&line);
        if stderr_indicates_anomaly(&text) {
            scan.timestamp_anomaly = true;
            scan.anomaly_lines += 1;
            if let Some(backward) = parse_backward_ms(&text) {
                scan.max_backward_ms =
                    Some(scan.max_backward_ms.map_or(backward, |seen| seen.max(backward)));
            }
        }
        scan.tail.push_str(&text);
        if scan.tail.len() > STDERR_TAIL_LIMIT * 2 {
            trim_to_tail(&mut scan.tail);
        }
    }
    trim_to_tail(&mut scan.tail);
    let status = match child.wait().await {
        Ok(status) => status,
        Err(error) => {
            report_failure(observer, Some(diagnostic), None);
            return Err(error);
        }
    };
    if !status.success() {
        report_failure(observer, Some(diagnostic), status.code());
    }
    Ok((status, scan))
}

fn trim_to_tail(buffer: &mut String) {
    if buffer.len() <= STDERR_TAIL_LIMIT {
        return;
    }
    let mut cut = buffer.len() - STDERR_TAIL_LIMIT;
    while cut < buffer.len() && !buffer.is_char_boundary(cut) {
        cut += 1;
    }
    buffer.drain(..cut);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_muxer_form() {
        assert_eq!(
            parse_backward_ms(
                "[null @ 0x1] Application provided invalid, non monotonically increasing dts to muxer in stream 0: 11990 >= 8356"
            ),
            Some(3634)
        );
    }

    #[test]
    fn parses_the_previous_current_form() {
        assert_eq!(
            parse_backward_ms("Non-monotonic DTS in output stream 0:1; previous: 11990, current: 8356; changing to 11991"),
            Some(3635 - 1)
        );
    }

    #[test]
    fn parses_a_reset_to_zero() {
        assert_eq!(
            parse_backward_ms(
                "[null @ 0x1] Application provided invalid, non monotonically increasing dts to muxer in stream 0: 24990 >= 0"
            ),
            Some(24990)
        );
    }

    /// 认不出的行返回 None，调用方据此保守拒绝——绝不能退化成「回退量 0」。
    #[test]
    fn unknown_wording_yields_none() {
        assert_eq!(parse_backward_ms("Invalid timestamp in stream 0"), None);
        assert_eq!(parse_backward_ms(""), None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nonzero_scan_emits_bounded_native_diagnostic() {
        use biliup_observability::{
            CaptureKind, CaptureLayer, Commit, Consumer, Event, Options, Runtime, StorageError,
        };
        use std::sync::{Arc, Mutex};
        use std::time::Duration;
        use tracing_subscriber::prelude::*;

        struct Memory(Arc<Mutex<Vec<Event>>>);
        impl Consumer for Memory {
            fn write(&mut self, batch: &[Event]) -> Result<Commit, StorageError> {
                self.0.lock().unwrap().extend_from_slice(batch);
                Ok(Commit::default())
            }
        }

        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let mut runtime = Runtime::start(
            "scan-test",
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
        let mut command = Command::new("sh");
        command.args(["-c", "printf 'fatal: token=secret-value\\n' >&2; exit 7"]);
        let (status, _) = run_scanning_stderr(
            &mut command,
            ScanObserver::quiet("controlled_scan", Path::new("/private/input.flv")),
        )
        .await
        .unwrap();
        assert_eq!(status.code(), Some(7));
        assert!(runtime.shutdown(Duration::from_secs(2)).closed);

        let events = events.lock().unwrap();
        let event = events
            .iter()
            .find(|event| {
                event.data().capture_kind == CaptureKind::Native
                    && event.data().event_name == "processing.command_failed"
            })
            .expect("nonzero external command must emit one native event");
        assert_eq!(event.data().fields.get("stage").unwrap(), "controlled_scan");
        assert_eq!(event.data().fields.get("exit_code").unwrap(), 7);
        assert_eq!(
            event.data().fields.get("original_file").unwrap(),
            "input.flv"
        );
        let diagnostic = event.diagnostic().expect("stderr belongs in an attachment");
        assert!(diagnostic.total_bytes() > 0);
        assert!(!diagnostic.tail().contains("secret-value"));
        assert!(diagnostic.tail().contains("[REDACTED]"));
    }

    #[test]
    fn keeps_only_the_tail_window() {
        let mut buffer = "a".repeat(STDERR_TAIL_LIMIT * 3);
        trim_to_tail(&mut buffer);
        assert_eq!(buffer.len(), STDERR_TAIL_LIMIT);
    }

    #[test]
    fn trimming_never_splits_a_character() {
        // 多字节字符恰好跨越切点时，切点必须往后挪到字符边界。
        let mut buffer = "中".repeat(STDERR_TAIL_LIMIT);
        trim_to_tail(&mut buffer);
        assert!(buffer.len() <= STDERR_TAIL_LIMIT);
        assert!(buffer.chars().all(|c| c == '中'));
    }

    #[test]
    fn detects_anomaly_patterns_only() {
        assert!(stderr_indicates_anomaly(
            "[flv @ 0x1] Non-monotonic DTS in output"
        ));
        assert!(!stderr_indicates_anomaly(
            "frame= 100 fps=25 time=00:00:04.00"
        ));
    }
}
