use crate::server::common::ffmpeg_scan::{ScanObserver, run_scanning_stderr};
use crate::server::common::process_priority::background;
use crate::server::errors::{AppError, AppResult};
use async_trait::async_trait;
use error_stack::{ResultExt, bail};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;
use tracing::{error, info, warn};

/// 重编码这一级自己的上限。
///
/// 它必须明显小于 attempt 层的 `preprocess_deadline`（10 min + 10 min/GiB）：一旦让那个
/// watchdog 先到点，被杀掉的是整个 attempt——扫描结论和 remux 中间产物一起丢弃，恢复
/// 调度再从第一步重来，而重编码的成本由内容时长×分辨率×帧率决定，重来一次必然再次超时。
/// 生产上一个 40 分钟 1080p60 的分段就这样连烧了三轮各 30 分钟，一次可上传结果都没有。
///
/// 超时不是「失败」，是「这一级修不好」：按 `Unfixable` 处理，原片照常上传、本地留档、
/// 发告警，attempt 本身是成功的。在 2 vCPU 上十分钟做不完的软件编码，再给多久也做不完。
const REENCODE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, PartialEq, Eq)]
pub enum RepairOutcome {
    Clean,
    Repaired(PathBuf),
    Unfixable,
}

#[async_trait]
pub trait FfmpegRunner {
    /// 全片扫描，返回 true 表示检测到时间戳异常（非单调/跳变）。
    async fn detect_anomaly(&self, path: &Path) -> AppResult<bool>;
    /// -c copy 重封装到 dst。
    async fn remux_copy(&self, src: &Path, dst: &Path) -> AppResult<()>;
    /// 保画质重编码到 dst。
    async fn reencode(&self, src: &Path, dst: &Path) -> AppResult<()>;
}

pub fn repaired_temp_path(src: &Path) -> PathBuf {
    let stem = src
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("segment");
    let dir = src.parent().unwrap_or_else(|| Path::new("."));
    dir.join(format!("{stem}.{}.fixed.mp4", std::process::id()))
}

pub async fn normalize_timestamps<R: FfmpegRunner + Sync>(
    path: &Path,
    runner: &R,
) -> RepairOutcome {
    // 1) 检测。检测出错 → 保守降级直传原片。
    match runner.detect_anomaly(path).await {
        Ok(false) => return RepairOutcome::Clean,
        Ok(true) => info!(file = ?path, "检测到时间戳异常，尝试修复"),
        Err(e) => {
            warn!(file = ?path, "时间戳检测失败，降级直传原片: {e:?}");
            return RepairOutcome::Clean;
        }
    }

    let dst = repaired_temp_path(path);

    // 2) copy 重封装修复。
    match runner.remux_copy(path, &dst).await {
        Ok(()) => match runner.detect_anomaly(&dst).await {
            Ok(false) => {
                info!(file = ?path, "remux copy 修复成功");
                return RepairOutcome::Repaired(dst);
            }
            Ok(true) => warn!(file = ?path, "remux copy 后仍异常，尝试重编码"),
            Err(e) => {
                warn!(file = ?dst, "修复后检测失败，降级直传原片: {e:?}");
                let _ = tokio::fs::remove_file(&dst).await;
                return RepairOutcome::Clean;
            }
        },
        Err(e) => {
            warn!(file = ?path, "remux copy 进程失败: {e:?}");
            let _ = tokio::fs::remove_file(&dst).await;
        }
    }

    // 3) 重编码修复。
    let reencode = match tokio::time::timeout(REENCODE_TIMEOUT, runner.reencode(path, &dst)).await {
        Ok(result) => result,
        Err(_elapsed) => {
            // future 被 drop 时 `kill_on_drop` 会收掉 ffmpeg。
            error!(
                file = ?path,
                timeout_secs = REENCODE_TIMEOUT.as_secs(),
                "重编码超过自身上限，标记 Unfixable 并直传原片"
            );
            let _ = tokio::fs::remove_file(&dst).await;
            return RepairOutcome::Unfixable;
        }
    };
    match reencode {
        Ok(()) => match runner.detect_anomaly(&dst).await {
            Ok(false) => {
                warn!(file = ?path, "重编码修复成功");
                RepairOutcome::Repaired(dst)
            }
            Ok(true) => {
                error!(file = ?path, "重编码后仍异常，标记 Unfixable");
                let _ = tokio::fs::remove_file(&dst).await;
                RepairOutcome::Unfixable
            }
            Err(e) => {
                warn!(file = ?dst, "重编码后检测失败，降级直传原片: {e:?}");
                let _ = tokio::fs::remove_file(&dst).await;
                RepairOutcome::Clean
            }
        },
        Err(e) => {
            // 进程层面失败（如 ffmpeg 不可用）→ 不阻断上传，降级直传原片。
            warn!(file = ?path, "重编码进程失败，降级直传原片: {e:?}");
            let _ = tokio::fs::remove_file(&dst).await;
            RepairOutcome::Clean
        }
    }
}

pub struct SystemFfmpeg;

#[async_trait]
impl FfmpegRunner for SystemFfmpeg {
    async fn detect_anomaly(&self, path: &Path) -> AppResult<bool> {
        // 全片扫描：-c copy -f null，只读不重编码。
        // 使用 verbose 级别确保 "Invalid timestamp" / "Application provided invalid" 等
        // 低于 warning 的模式也能输出；-nostats 抑制进度行噪声。
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
            .arg(path)
            .args(["-c", "copy", "-f", "null", "-"]);
        let (status, scan) = run_scanning_stderr(
            background(&mut command),
            ScanObserver::quiet("timestamp_detect", path),
        )
        .await
        .change_context(AppError::Custom("failed to spawn ffmpeg (detect)".into()))?;
        // 模式命中优先：即使退出码非零也应尝试修复。
        if scan.timestamp_anomaly {
            return Ok(true);
        }
        // 无异常模式，但退出码非零 → 可能是路径错误等无关故障，向上报错。
        if !status.success() {
            bail!(AppError::Custom(format!(
                "ffmpeg detect exited non-zero ({status}) for {}",
                path.display()
            )));
        }
        Ok(false)
    }

    async fn remux_copy(&self, src: &Path, dst: &Path) -> AppResult<()> {
        let mut command = Command::new("ffmpeg");
        background(&mut command)
            .args([
                "-hide_banner",
                "-loglevel",
                "warning",
                "-y",
                "-fflags",
                "+genpts+igndts",
                "-i",
            ])
            .arg(src)
            .args([
                "-c",
                "copy",
                "-bsf:a",
                "aac_adtstoasc",
                "-movflags",
                "+faststart",
                "-avoid_negative_ts",
                "make_zero",
                "-muxdelay",
                "0",
                "-muxpreload",
                "0",
            ])
            .arg(dst)
            .kill_on_drop(true);
        let (status, _) = run_scanning_stderr(
            &mut command,
            ScanObserver {
                stage: "timestamp_remux",
                original_file: Some(src),
                tee_stderr: true,
            },
        )
        .await
        .change_context(AppError::Custom("failed to spawn ffmpeg (remux)".into()))?;
        if !status.success() {
            let _ = tokio::fs::remove_file(dst).await;
            bail!(AppError::Custom(format!(
                "ffmpeg remux_copy failed (status {status:?}) for {}",
                src.display()
            )));
        }
        // Guard: ffmpeg may exit 0 but produce no output (e.g. codec mismatch).
        match tokio::fs::metadata(dst).await {
            Ok(m) if m.len() > 0 => {}
            _ => {
                let _ = tokio::fs::remove_file(dst).await;
                bail!(AppError::Custom(format!(
                    "ffmpeg remux_copy produced empty output for {}",
                    src.display()
                )));
            }
        }
        Ok(())
    }

    async fn reencode(&self, src: &Path, dst: &Path) -> AppResult<()> {
        let mut command = Command::new("ffmpeg");
        background(&mut command)
            .args([
                "-hide_banner",
                "-loglevel",
                "warning",
                "-y",
                "-fflags",
                "+genpts",
                "-i",
            ])
            .arg(src)
            .args([
                "-c:v",
                "libx264",
                "-crf",
                "18",
                "-preset",
                "veryfast",
                "-c:a",
                "aac",
                "-movflags",
                "+faststart",
                "-avoid_negative_ts",
                "make_zero",
            ])
            .arg(dst)
            .kill_on_drop(true);
        let (status, _) = run_scanning_stderr(
            &mut command,
            ScanObserver {
                stage: "timestamp_reencode",
                original_file: Some(src),
                tee_stderr: true,
            },
        )
        .await
        .change_context(AppError::Custom("failed to spawn ffmpeg (reencode)".into()))?;
        if !status.success() {
            let _ = tokio::fs::remove_file(dst).await;
            bail!(AppError::Custom(format!(
                "ffmpeg reencode failed (status {status:?}) for {}",
                src.display()
            )));
        }
        // Guard: ffmpeg may exit 0 but produce no output (e.g. codec unavailable).
        match tokio::fs::metadata(dst).await {
            Ok(m) if m.len() > 0 => {}
            _ => {
                let _ = tokio::fs::remove_file(dst).await;
                bail!(AppError::Custom(format!(
                    "ffmpeg reencode produced empty output for {}",
                    src.display()
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 脚本化 fake：按预设返回 detect 各阶段结果与 remux/reencode 成败。
    struct FakeFfmpeg {
        // detect 调用依次返回的结果队列（true=异常）
        detect_results: Mutex<std::collections::VecDeque<AppResult<bool>>>,
        remux_ok: bool,
        reencode_ok: bool,
    }

    impl FakeFfmpeg {
        fn new(detect: Vec<AppResult<bool>>, remux_ok: bool, reencode_ok: bool) -> Self {
            Self {
                detect_results: Mutex::new(detect.into_iter().collect()),
                remux_ok,
                reencode_ok,
            }
        }
    }

    #[async_trait]
    impl FfmpegRunner for FakeFfmpeg {
        async fn detect_anomaly(&self, _path: &Path) -> AppResult<bool> {
            self.detect_results
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected extra detect call")
        }
        async fn remux_copy(&self, _src: &Path, dst: &Path) -> AppResult<()> {
            if self.remux_ok {
                tokio::fs::write(dst, b"x").await.ok();
                Ok(())
            } else {
                Err(error_stack::Report::new(
                    crate::server::errors::AppError::Custom("remux fail".into()),
                ))
            }
        }
        async fn reencode(&self, _src: &Path, dst: &Path) -> AppResult<()> {
            if self.reencode_ok {
                tokio::fs::write(dst, b"x").await.ok();
                Ok(())
            } else {
                Err(error_stack::Report::new(
                    crate::server::errors::AppError::Custom("reencode fail".into()),
                ))
            }
        }
    }

    fn p(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("tsr_{name}.flv"))
    }

    #[tokio::test]
    async fn clean_when_no_anomaly() {
        let path = p("clean_when_no_anomaly");
        let f = FakeFfmpeg::new(vec![Ok(false)], true, true);
        assert_eq!(normalize_timestamps(&path, &f).await, RepairOutcome::Clean);
    }

    #[tokio::test]
    async fn repaired_by_copy_when_remux_fixes() {
        // detect: 原片异常 → copy 后干净
        let path = p("repaired_by_copy_when_remux_fixes");
        let f = FakeFfmpeg::new(vec![Ok(true), Ok(false)], true, true);
        assert_eq!(
            normalize_timestamps(&path, &f).await,
            RepairOutcome::Repaired(repaired_temp_path(&path))
        );
    }

    #[tokio::test]
    async fn repaired_by_reencode_when_copy_insufficient() {
        // detect: 原异常 → copy 后仍异常 → reencode 后干净
        let path = p("repaired_by_reencode_when_copy_insufficient");
        let f = FakeFfmpeg::new(vec![Ok(true), Ok(true), Ok(false)], true, true);
        assert_eq!(
            normalize_timestamps(&path, &f).await,
            RepairOutcome::Repaired(repaired_temp_path(&path))
        );
    }

    #[tokio::test]
    async fn unfixable_when_reencode_still_anomalous() {
        // detect: 原异常 → copy 仍异常 → reencode 仍异常
        let path = p("unfixable_when_reencode_still_anomalous");
        let f = FakeFfmpeg::new(vec![Ok(true), Ok(true), Ok(true)], true, true);
        assert_eq!(
            normalize_timestamps(&path, &f).await,
            RepairOutcome::Unfixable
        );
    }

    #[tokio::test]
    async fn degrades_to_clean_when_detect_errors() {
        // 检测阶段进程出错 → 保守降级为 Clean（直传原片）
        let path = p("degrades_to_clean_when_detect_errors");
        let f = FakeFfmpeg::new(
            vec![Err(error_stack::Report::new(
                crate::server::errors::AppError::Custom("ffmpeg missing".into()),
            ))],
            true,
            true,
        );
        assert_eq!(normalize_timestamps(&path, &f).await, RepairOutcome::Clean);
    }

    #[tokio::test]
    async fn degrades_to_clean_when_remux_process_fails() {
        // 原片异常但 remux 进程报错，且 reencode 也报错 → 无法修复进程层面 → 降级 Clean（不阻断上传）
        let path = p("degrades_to_clean_when_remux_process_fails");
        let f = FakeFfmpeg::new(vec![Ok(true)], false, false);
        assert_eq!(normalize_timestamps(&path, &f).await, RepairOutcome::Clean);
    }

    /// reencode 永不返回的 fake，用来验证第 3 级自己的超时。
    struct HangingReencode;

    #[async_trait]
    impl FfmpegRunner for HangingReencode {
        async fn detect_anomaly(&self, _path: &Path) -> AppResult<bool> {
            Ok(true)
        }
        async fn remux_copy(&self, _src: &Path, dst: &Path) -> AppResult<()> {
            tokio::fs::write(dst, b"x").await.ok();
            Ok(())
        }
        async fn reencode(&self, _src: &Path, _dst: &Path) -> AppResult<()> {
            std::future::pending().await
        }
    }

    /// 卡住的重编码必须由本模块自己收口成 `Unfixable`（原片直传 + 本地留档 + 告警），
    /// 而不是拖到 attempt 层的 preprocess watchdog 把整个 attempt 杀掉。
    /// `start_paused` 让 tokio 在运行时空闲时把时钟直接推到超时点，测试不真的等十分钟。
    #[tokio::test(start_paused = true)]
    async fn unfixable_when_reencode_exceeds_its_own_timeout() {
        let path = p("unfixable_when_reencode_exceeds_its_own_timeout");
        assert_eq!(
            normalize_timestamps(&path, &HangingReencode).await,
            RepairOutcome::Unfixable
        );
        // 超时路径也要清掉半成品，不然每次重试都留一份垃圾。
        assert!(!repaired_temp_path(&path).exists());
    }

    /// 需要本地 ffmpeg；手动运行：cargo test -p biliup-cli system_ffmpeg -- --ignored
    #[tokio::test]
    #[ignore]
    async fn system_ffmpeg_detect_clean_on_generated_file() {
        let dir = std::env::temp_dir();
        let good = dir.join("tsr_good.mp4");
        // 生成 2 秒正常测试视频
        let st = tokio::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=2:size=320x240:rate=10",
            ])
            .arg(&good)
            .status()
            .await
            .expect("spawn ffmpeg");
        assert!(st.success());

        let runner = SystemFfmpeg;
        let anomaly = runner.detect_anomaly(&good).await.expect("detect");
        assert!(!anomaly, "正常文件不应报时间戳异常");

        // 正常文件走整条流程应得 Clean
        assert_eq!(
            normalize_timestamps(&good, &runner).await,
            RepairOutcome::Clean
        );
        let _ = tokio::fs::remove_file(&good).await;
    }
}
