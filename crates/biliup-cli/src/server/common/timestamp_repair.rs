use crate::server::errors::AppResult;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
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
}
