use crate::server::errors::{AppError, AppResult};
use async_trait::async_trait;
use error_stack::{ResultExt, bail};
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tracing::{error, info, warn};

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
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("segment");
    let dir = src.parent().unwrap_or_else(|| Path::new("."));
    dir.join(format!("{stem}.fixed.mp4"))
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
            Ok(true) => info!(file = ?path, "remux copy 后仍异常，尝试重编码"),
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
    match runner.reencode(path, &dst).await {
        Ok(()) => match runner.detect_anomaly(&dst).await {
            Ok(false) => {
                info!(file = ?path, "重编码修复成功");
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

/// stderr 中命中任一模式即判为时间戳异常（用具体模式，避免宽泛词误判）。
fn stderr_indicates_anomaly(stderr: &str) -> bool {
    const PATTERNS: &[&str] = &[
        "Non-monotonic DTS",
        "non monotonically increasing dts",
        "timestamp discontinuity",
        "Invalid timestamp",
        "Application provided invalid",
    ];
    PATTERNS.iter().any(|p| stderr.contains(p))
}

#[async_trait]
impl FfmpegRunner for SystemFfmpeg {
    async fn detect_anomaly(&self, path: &Path) -> AppResult<bool> {
        // 全片扫描：-c copy -f null，只读不重编码；warning 级别即可暴露时间戳告警。
        let output = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "warning", "-fflags", "+igndts", "-i"])
            .arg(path)
            .args(["-c", "copy", "-f", "null", "-"])
            .kill_on_drop(true)
            .output()
            .await
            .change_context(AppError::Custom("failed to spawn ffmpeg (detect)".into()))?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        Ok(stderr_indicates_anomaly(&stderr))
    }

    async fn remux_copy(&self, src: &Path, dst: &Path) -> AppResult<()> {
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner", "-loglevel", "warning", "-y",
                "-fflags", "+genpts+igndts", "-i",
            ])
            .arg(src)
            .args([
                "-c", "copy",
                "-bsf:a", "aac_adtstoasc",
                "-movflags", "+faststart",
                "-avoid_negative_ts", "make_zero",
                "-muxdelay", "0", "-muxpreload", "0",
            ])
            .arg(dst)
            .kill_on_drop(true)
            .status()
            .await
            .change_context(AppError::Custom("failed to spawn ffmpeg (remux)".into()))?;
        if !status.success() {
            let _ = tokio::fs::remove_file(dst).await;
            bail!(AppError::Custom(format!(
                "ffmpeg remux_copy failed (status {status:?}) for {}",
                src.display()
            )));
        }
        Ok(())
    }

    async fn reencode(&self, src: &Path, dst: &Path) -> AppResult<()> {
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner", "-loglevel", "warning", "-y",
                "-fflags", "+genpts", "-i",
            ])
            .arg(src)
            .args([
                "-c:v", "libx264", "-crf", "18", "-preset", "veryfast",
                "-c:a", "aac",
                "-movflags", "+faststart",
                "-avoid_negative_ts", "make_zero",
            ])
            .arg(dst)
            .kill_on_drop(true)
            .status()
            .await
            .change_context(AppError::Custom("failed to spawn ffmpeg (reencode)".into()))?;
        if !status.success() {
            let _ = tokio::fs::remove_file(dst).await;
            bail!(AppError::Custom(format!(
                "ffmpeg reencode failed (status {status:?}) for {}",
                src.display()
            )));
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
                Err(error_stack::Report::new(crate::server::errors::AppError::Custom(
                    "remux fail".into(),
                )))
            }
        }
        async fn reencode(&self, _src: &Path, dst: &Path) -> AppResult<()> {
            if self.reencode_ok {
                tokio::fs::write(dst, b"x").await.ok();
                Ok(())
            } else {
                Err(error_stack::Report::new(crate::server::errors::AppError::Custom(
                    "reencode fail".into(),
                )))
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
        assert_eq!(normalize_timestamps(&path, &f).await, RepairOutcome::Unfixable);
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

    /// 需要本地 ffmpeg；手动运行：cargo test -p biliup-cli system_ffmpeg -- --ignored
    #[tokio::test]
    #[ignore]
    async fn system_ffmpeg_detect_clean_on_generated_file() {
        let dir = std::env::temp_dir();
        let good = dir.join("tsr_good.mp4");
        // 生成 2 秒正常测试视频
        let st = tokio::process::Command::new("ffmpeg")
            .args(["-y", "-f", "lavfi", "-i", "testsrc=duration=2:size=320x240:rate=10"])
            .arg(&good)
            .status()
            .await
            .expect("spawn ffmpeg");
        assert!(st.success());

        let runner = SystemFfmpeg;
        let anomaly = runner.detect_anomaly(&good).await.expect("detect");
        assert!(!anomaly, "正常文件不应报时间戳异常");

        // 正常文件走整条流程应得 Clean
        assert_eq!(normalize_timestamps(&good, &runner).await, RepairOutcome::Clean);
        let _ = tokio::fs::remove_file(&good).await;
    }
}
