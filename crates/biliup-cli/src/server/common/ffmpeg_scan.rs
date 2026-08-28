//! 全片扫描类 ffmpeg 调用的 stderr 处理：边读边判时间戳异常，只保留尾部窗口。
//!
//! 这类扫描用 `-loglevel verbose`，一段时间戳大面积异常的长录像能产出几百 MB stderr，
//! 而 `Command::output()` 会把它整个收进内存。这里改成流式：异常模式是行内模式，边读边
//! 匹配即可；常驻内存只有一个尾部窗口，loudnorm 的 JSON 在 ffmpeg 退出前才打印，正好
//! 落在窗口内。

use std::process::{ExitStatus, Stdio};
use tokio::io::{AsyncBufReadExt, BufReader};
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

pub struct StderrScan {
    /// stderr 的尾部窗口。
    pub tail: String,
    /// 扫描过程中是否命中过时间戳异常模式（覆盖全部输出，不止尾部窗口）。
    pub timestamp_anomaly: bool,
}

/// 跑一个全片扫描 ffmpeg 并流式消费它的 stderr。
pub async fn run_scanning_stderr(command: &mut Command) -> std::io::Result<(ExitStatus, StderrScan)> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stderr = child.stderr.take().expect("stderr piped");
    let mut reader = BufReader::new(stderr);
    let mut line = Vec::new();
    let mut scan = StderrScan {
        tail: String::new(),
        timestamp_anomaly: false,
    };
    loop {
        line.clear();
        // 按字节读：ffmpeg 偶尔会在日志里吐出非 UTF-8 片段，`lines()` 会因此直接报错。
        if reader.read_until(b'\n', &mut line).await? == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&line);
        if !scan.timestamp_anomaly && stderr_indicates_anomaly(&text) {
            scan.timestamp_anomaly = true;
        }
        scan.tail.push_str(&text);
        if scan.tail.len() > STDERR_TAIL_LIMIT * 2 {
            trim_to_tail(&mut scan.tail);
        }
    }
    trim_to_tail(&mut scan.tail);
    let status = child.wait().await?;
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
        assert!(stderr_indicates_anomaly("[flv @ 0x1] Non-monotonic DTS in output"));
        assert!(!stderr_indicates_anomaly("frame= 100 fps=25 time=00:00:04.00"));
    }
}
