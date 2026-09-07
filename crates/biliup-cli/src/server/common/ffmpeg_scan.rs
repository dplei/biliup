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

/// 相邻包时间戳的异常前跳阈值。与录制侧 `TimestampRebase` 的 `REBASE_MAX_STEP_MS` 讲同一套
/// 故事：段内的真实空档由停顿看门狗（默认 30s 不来字节就断连重连）兜住，超过它的前跳只可能
/// 是基准跳变。两侧用同一个数，才不会出现「录制侧认为是跳变、上传侧认为正常」的夹缝。
pub const MAX_PACKET_STEP_MS: i64 = 30_000;

/// 逐行消费 ffprobe 的 `stream_index,dts_time` 输出，找相邻包之间的异常大幅前跳。
///
/// 存在的理由是一个**确定性**漏检：源文件里的 32 位时间戳倒退可能已经被解复用器展开成一个
/// 单调的巨大前跳，展开之后 muxer 无话可说，`stderr_indicates_anomaly` 的五个串一个都不会
/// 出现（实测：video DTS 从 0 跳到 4.29e9 秒的样本，`-c copy -f null -` 报 0 条异常行）。
/// 这条判据只看数值，不看措辞。
///
/// 每条流各自比对：audio 与 video 交错送达，跨流相减必然出负数。
#[derive(Default)]
pub struct PacketJumpScan {
    /// `(stream_index, 上一个 dts 毫秒)`。流的条数是个位数，线性查找即可。
    last: Vec<(i64, i64)>,
    /// 超过阈值的最大单次前跳，`None` 表示没有一次越线。
    pub max_forward_jump_ms: Option<i64>,
    /// 读到的包数，用来区分「没有跳变」和「根本没读到包」。
    pub packets: u64,
}

impl PacketJumpScan {
    /// 喂一行 `stream_index,dts_time`。认不出的行（`N/A`、空行、ffprobe 的其它输出）直接跳过：
    /// 这条判据是给已有文本判据兜底的，宁可少判，不能因为一行怪东西就把好片判成坏片。
    pub fn push_line(&mut self, line: &str) {
        let Some((index, dts)) = line.trim().split_once(',') else {
            return;
        };
        let (Ok(index), Ok(seconds)) = (index.parse::<i64>(), dts.trim().parse::<f64>()) else {
            return;
        };
        if !seconds.is_finite() {
            return;
        }
        let dts_ms = (seconds * 1_000.0) as i64;
        self.packets += 1;
        match self.last.iter_mut().find(|(stream, _)| *stream == index) {
            Some((_, previous)) => {
                let jump = dts_ms - *previous;
                if jump > MAX_PACKET_STEP_MS {
                    self.max_forward_jump_ms =
                        Some(self.max_forward_jump_ms.map_or(jump, |seen| seen.max(jump)));
                }
                *previous = dts_ms;
            }
            None => self.last.push((index, dts_ms)),
        }
    }
}

/// 跑一个 ffprobe 包扫描并流式消费它的 stdout。
///
/// 输出是每包一行，一场长录像有几十万行，所以只维护流状态，不留全量。
pub async fn run_packet_jump_scan(
    command: &mut Command,
    observer: ScanObserver<'_>,
) -> std::io::Result<(ExitStatus, PacketJumpScan)> {
    let mut child = match command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            report_failure(observer, None, None);
            return Err(error);
        }
    };
    let stdout = child.stdout.take().expect("stdout piped");
    let mut reader = BufReader::new(stdout);
    let mut scan = PacketJumpScan::default();
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = match reader.read_until(b'\n', &mut line).await {
            Ok(read) => read,
            Err(error) => {
                report_failure(observer, None, None);
                return Err(error);
            }
        };
        if read == 0 {
            break;
        }
        scan.push_line(&String::from_utf8_lossy(&line));
    }
    let status = match child.wait().await {
        Ok(status) => status,
        Err(error) => {
            report_failure(observer, None, None);
            return Err(error);
        }
    };
    if !status.success() {
        report_failure(observer, None, status.code());
    }
    Ok((status, scan))
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
    pub context: Option<&'a EventContext>,
}

impl<'a> ScanObserver<'a> {
    pub fn quiet(stage: &'a str, original_file: &'a Path) -> Self {
        Self {
            stage,
            original_file: Some(original_file),
            tee_stderr: false,
            context: None,
        }
    }

    pub fn with_context(mut self, context: &'a EventContext) -> Self {
        self.context = Some(context);
        self
    }
}

fn context(observer: ScanObserver<'_>) -> EventContext {
    let mut fields = Fields::new();
    if let Some(path) = observer.original_file {
        fields.insert("original_file", path.display().to_string().into());
    }
    let file_context = EventContext(fields);
    match observer.context {
        Some(context) => file_context.child(context.0.clone()),
        None => file_context,
    }
}

fn report_failure(
    observer: ScanObserver<'_>,
    capture: Option<DiagnosticCapture>,
    code: Option<i32>,
) {
    crate::observe::external::command_failed(
        observer.stage,
        "process_failed",
        context(observer),
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
    let mut unparsed_anomalies = DiagnosticCapture::new();
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
                scan.max_backward_ms = Some(
                    scan.max_backward_ms
                        .map_or(backward, |seen| seen.max(backward)),
                );
            } else {
                unparsed_anomalies.push(&line);
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
    } else if scan.timestamp_anomaly && scan.max_backward_ms.is_none() {
        crate::observe::external::timestamp_anomaly_unparsed(
            observer.stage,
            context(observer),
            unparsed_anomalies.finish(status.code()),
        );
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

    /// 单调的巨大前跳——32 位倒退被解复用器展开后的形态。文本判据在这种输入上一条都不报。
    #[test]
    fn a_monotonous_giant_jump_is_an_anomaly() {
        let mut scan = PacketJumpScan::default();
        for line in [
            "0,0.000000",
            "1,0.010000",
            "0,0.066000",
            "1,0.033000",
            // CDN 换基准：单调，但一步跨了 49.7 天
            "0,4294007.933000",
            "1,4294007.900000",
            "0,4294008.000000",
        ] {
            scan.push_line(line);
        }
        assert_eq!(scan.packets, 7);
        assert_eq!(scan.max_forward_jump_ms, Some(4_294_007_867));
    }

    /// audio/video 交错时跨流相减必然出负数，所以判据必须按流分开——这里两条流各自单调，
    /// 只是交错送达，不该报。
    #[test]
    fn interleaved_streams_are_compared_separately() {
        let mut scan = PacketJumpScan::default();
        for line in [
            "0,10.000000",
            "1,9.980000",
            "0,10.040000",
            "1,10.020000",
            "0,10.080000",
        ] {
            scan.push_line(line);
        }
        assert_eq!(scan.max_forward_jump_ms, None);
    }

    /// 真回退归文本判据管，这条判据只认前跳，不重复报也不越权。
    #[test]
    fn a_backward_step_is_not_this_judgement() {
        let mut scan = PacketJumpScan::default();
        for line in ["0,12.000000", "0,8.000000", "0,12.100000"] {
            scan.push_line(line);
        }
        assert_eq!(scan.max_forward_jump_ms, None);
    }

    /// 阈值上恰好等于 `MAX_PACKET_STEP_MS` 不算跳变，多 1 毫秒才算。
    #[test]
    fn the_threshold_is_exclusive() {
        let mut at_limit = PacketJumpScan::default();
        at_limit.push_line("0,0.000000");
        at_limit.push_line("0,30.000000");
        assert_eq!(at_limit.max_forward_jump_ms, None);

        let mut over_limit = PacketJumpScan::default();
        over_limit.push_line("0,0.000000");
        over_limit.push_line("0,30.001000");
        assert_eq!(over_limit.max_forward_jump_ms, Some(30_001));
    }

    /// 认不出的行不能把好片判坏：跳过即可。
    #[test]
    fn unreadable_lines_are_skipped() {
        let mut scan = PacketJumpScan::default();
        for line in ["0,N/A", "", "side_data|", "0,0.000000", "0,0.040000"] {
            scan.push_line(line);
        }
        assert_eq!(scan.packets, 2);
        assert_eq!(scan.max_forward_jump_ms, None);
    }

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
    async fn scans_emit_only_relevant_native_diagnostics() {
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
        let context = crate::observe::UploadIdentity {
            segment_id: Some("segment-test".into()),
            upload_attempt_id: Some("attempt-test".into()),
            original_file: Some("/private/stable-source.flv".into()),
            ..Default::default()
        }
        .context();

        let mut command = Command::new("sh");
        command.args([
            "-c",
            "printf 'ordinary token=not-captured\\nInvalid timestamp in stream 0\\nInvalid timestamp token=secret-value\\n' >&2",
        ]);
        let (status, _) = run_scanning_stderr(
            &mut command,
            ScanObserver::quiet("unknown_scan", Path::new("/private/input.flv"))
                .with_context(&context),
        )
        .await
        .unwrap();
        assert!(status.success());

        let mut command = Command::new("sh");
        command.args([
            "-c",
            "printf 'Application provided invalid, non monotonically increasing dts to muxer in stream 0: 11990 >= 8356\\n' >&2",
        ]);
        let (status, _) = run_scanning_stderr(
            &mut command,
            ScanObserver::quiet("parsed_scan", Path::new("/private/input.flv"))
                .with_context(&context),
        )
        .await
        .unwrap();
        assert!(status.success());

        let mut command = Command::new("sh");
        command.args(["-c", "printf 'frame=100 fps=25\\n' >&2"]);
        let (status, _) = run_scanning_stderr(
            &mut command,
            ScanObserver::quiet("clean_scan", Path::new("/private/input.flv"))
                .with_context(&context),
        )
        .await
        .unwrap();
        assert!(status.success());

        let mut command = Command::new("sh");
        command.args(["-c", "printf 'fatal: token=failed-secret\\n' >&2; exit 7"]);
        let (status, _) = run_scanning_stderr(
            &mut command,
            ScanObserver::quiet("failed_scan", Path::new("/private/input.flv"))
                .with_context(&context),
        )
        .await
        .unwrap();
        assert_eq!(status.code(), Some(7));
        assert!(runtime.shutdown(Duration::from_secs(2)).closed);

        let events = events.lock().unwrap();
        let diagnostics = events
            .iter()
            .filter(|event| {
                event.data().capture_kind == CaptureKind::Native
                    && event.data().event_name == "processing.diagnostic_captured"
            })
            .collect::<Vec<_>>();
        assert_eq!(diagnostics.len(), 1);
        let event = diagnostics[0];
        assert_eq!(event.data().fields.get("stage").unwrap(), "unknown_scan");
        assert_eq!(event.data().fields.get("outcome").unwrap(), "unknown");
        assert_eq!(
            event.data().fields.get("reason_code").unwrap(),
            "timestamp_anomaly_unparsed"
        );
        assert_eq!(
            event.data().fields.get("segment_id").unwrap(),
            "segment-test"
        );
        assert_eq!(
            event.data().fields.get("upload_attempt_id").unwrap(),
            "attempt-test"
        );
        assert_eq!(
            event.data().fields.get("original_file").unwrap(),
            "stable-source.flv"
        );
        let diagnostic = event
            .diagnostic()
            .expect("unparsed anomaly belongs in an attachment");
        assert!(diagnostic.total_bytes() > 0);
        assert!(!diagnostic.tail().contains("secret-value"));
        assert!(diagnostic.tail().contains("[REDACTED]"));
        assert!(diagnostic.tail().contains("Invalid timestamp"));
        assert!(!diagnostic.tail().contains("ordinary"));

        let failures = events
            .iter()
            .filter(|event| {
                event.data().capture_kind == CaptureKind::Native
                    && event.data().event_name == "processing.command_failed"
            })
            .collect::<Vec<_>>();
        assert_eq!(failures.len(), 1);
        let event = failures[0];
        assert_eq!(event.data().fields.get("stage").unwrap(), "failed_scan");
        assert_eq!(event.data().fields.get("exit_code").unwrap(), 7);
        let diagnostic = event.diagnostic().expect("stderr belongs in an attachment");
        assert!(!diagnostic.tail().contains("failed-secret"));
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

    #[test]
    fn observer_without_business_context_keeps_the_file() {
        let context = context(ScanObserver::quiet(
            "controlled_scan",
            Path::new("/private/input.flv"),
        ));
        assert_eq!(context.0.get("original_file").unwrap(), "input.flv");
    }
}
