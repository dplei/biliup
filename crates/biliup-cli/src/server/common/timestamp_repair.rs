use crate::server::common::ffmpeg_scan::{ScanObserver, run_scanning_stderr};
use crate::server::common::process_priority::background;
use crate::server::errors::{AppError, AppResult};
use async_trait::async_trait;
use biliup_observability::Context as EventContext;
use error_stack::{ResultExt, bail};
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tracing::{error, info, warn};

/// 单次 DTS 回退的上限：超过它就不做时间戳重写，直接判 `Unfixable`。
///
/// setts 的 `max()` 把回退的那段时间夹掉。**它的损害上限恰好等于回退量**——最坏情况下
/// 回退点之后的内容追不上，被压进「回退量 × 1ms/packet」的窗口里，被毁的内容不会超过
/// 回退的那一段。所以这条判据不需要知道文件总时长，一个绝对值就够；而总时长在这类文件上
/// 恰恰是拿不到的：`format.duration` 在有回退的 FLV 上返回的是回退点，不是真实跨度。
///
/// 取 10 秒的依据：
///
/// - 回退量的物理含义是 CDN 回放的重叠时长，边缘节点缓冲区是秒级；生产实测 2.6 秒，
///   本地构造的重叠样本 3.6 秒。10 秒留了约 4 倍余量。
/// - 万一误放行，损害上限是 10 秒内容被压成快进，对 30–60 分钟的分段是 0.5% 以下。
/// - 时间戳重置／回绕（见 #13）的回退量是分钟到小时级，被稳稳挡在外面。实测「重置到 0」
///   的样本回退 12 秒，clamp 会把之后 10.6 秒的真实内容压进 0.35 秒，而复检看不出来——
///   它只看单调性。没有这道闸门，那种输入会被当成修复成功上传。
const MAX_REPAIRABLE_BACKWARD_MS: i64 = 10_000;

/// 一次全片扫描的结论。
#[derive(Debug, PartialEq, Eq)]
pub enum Detection {
    Clean,
    /// 命中时间戳异常。`max_backward_ms` 是解析到的最大单次回退量；`None` 表示一条数值
    /// 都没解出来（ffmpeg 换了措辞），此时必须保守当作「超过上限」，绝不能当作 0。
    Anomalous {
        max_backward_ms: Option<i64>,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum RepairOutcome {
    Clean,
    Repaired(PathBuf),
    Fallback(RepairFallbackReason),
    Unfixable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairFallbackReason {
    DetectFailed,
    RemuxFailed,
    VerificationFailed,
}

#[async_trait]
pub trait FfmpegRunner {
    /// 全片扫描，报告是否有时间戳异常以及最大回退量。
    async fn detect(&self, path: &Path) -> AppResult<Detection>;
    /// `-c copy` + setts 重封装到 dst。
    async fn remux_copy(&self, src: &Path, dst: &Path) -> AppResult<()>;
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
    let backward = match runner.detect(path).await {
        Ok(Detection::Clean) => return RepairOutcome::Clean,
        Ok(Detection::Anomalous { max_backward_ms }) => max_backward_ms,
        Err(e) => {
            warn!(file = ?path, "时间戳检测失败，降级直传原片: {e:?}");
            return RepairOutcome::Fallback(RepairFallbackReason::DetectFailed);
        }
    };

    // 2) 回退量闸门。见 `MAX_REPAIRABLE_BACKWARD_MS`：超限的输入被 setts 修过之后是单调的，
    //    复检也认为修好了，但内容已经被压成帧风暴——所以必须在动手之前拦住。
    match backward {
        Some(ms) if ms <= MAX_REPAIRABLE_BACKWARD_MS => {
            info!(file = ?path, backward_ms = ms, "检测到时间戳异常，尝试修复");
        }
        Some(ms) => {
            error!(
                file = ?path,
                backward_ms = ms,
                limit_ms = MAX_REPAIRABLE_BACKWARD_MS,
                "时间戳回退超过可安全重写的上限，标记 Unfixable 并直传原片"
            );
            return RepairOutcome::Unfixable;
        }
        None => {
            error!(
                file = ?path,
                "检测到时间戳异常但解析不出回退量，保守标记 Unfixable 并直传原片"
            );
            return RepairOutcome::Unfixable;
        }
    }

    let dst = repaired_temp_path(path);

    // 3) copy + setts 重封装修复，一律以复检为准。
    match runner.remux_copy(path, &dst).await {
        Ok(()) => match runner.detect(&dst).await {
            Ok(Detection::Clean) => {
                info!(file = ?path, "时间戳重写修复成功");
                RepairOutcome::Repaired(dst)
            }
            Ok(Detection::Anomalous { .. }) => {
                error!(file = ?path, "时间戳重写后仍异常，标记 Unfixable");
                let _ = tokio::fs::remove_file(&dst).await;
                RepairOutcome::Unfixable
            }
            Err(e) => {
                warn!(file = ?dst, "修复后检测失败，降级直传原片: {e:?}");
                let _ = tokio::fs::remove_file(&dst).await;
                RepairOutcome::Fallback(RepairFallbackReason::VerificationFailed)
            }
        },
        Err(e) => {
            // 进程层面失败（如 ffmpeg 不可用）是环境问题不是媒体问题，不阻断上传。
            warn!(file = ?path, "时间戳重写进程失败，降级直传原片: {e:?}");
            let _ = tokio::fs::remove_file(&dst).await;
            RepairOutcome::Fallback(RepairFallbackReason::RemuxFailed)
        }
    }
}

/// 把 packet 时间戳夹成单调递增。见 `remux_copy` 里的注释说明为什么是这个写法。
const SETTS_MONOTONIC: &str = r"setts=pts=max(PTS\,PREV_OUTPTS+1):dts=max(DTS\,PREV_OUTDTS+1)";
const SETTS_MONOTONIC_AFTER_ADTSTOASC: &str =
    r"aac_adtstoasc,setts=pts=max(PTS\,PREV_OUTPTS+1):dts=max(DTS\,PREV_OUTDTS+1)";

#[derive(Default)]
pub struct SystemFfmpeg {
    context: EventContext,
}

impl SystemFfmpeg {
    pub fn with_context(context: EventContext) -> Self {
        Self { context }
    }
}

#[async_trait]
impl FfmpegRunner for SystemFfmpeg {
    async fn detect(&self, path: &Path) -> AppResult<Detection> {
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
            ScanObserver::quiet("timestamp_detect", path).with_context(&self.context),
        )
        .await
        .change_context(AppError::Custom("failed to spawn ffmpeg (detect)".into()))?;
        // 模式命中优先：即使退出码非零也应尝试修复。
        if scan.timestamp_anomaly {
            return Ok(Detection::Anomalous {
                max_backward_ms: scan.max_backward_ms,
            });
        }
        // 无异常模式，但退出码非零 → 可能是路径错误等无关故障，向上报错。
        if !status.success() {
            bail!(AppError::Custom(format!(
                "ffmpeg detect exited non-zero ({status}) for {}",
                path.display()
            )));
        }
        Ok(Detection::Clean)
    }

    async fn remux_copy(&self, src: &Path, dst: &Path) -> AppResult<()> {
        let mut command = Command::new("ffmpeg");
        background(&mut command)
            .args([
                "-hide_banner",
                "-loglevel",
                "warning",
                "-y",
                // `+genpts` 只是给缺 PTS 的源兜底，好让下面的 setts 不会拿到 NOPTS 去比大小；
                // 源本来就有 PTS 时它没有任何作用（实测三种 fflags 组合产物一致）。
                //
                // 这里曾经还有 `+igndts`，它对本类故障结构上无效：语义是丢弃 DTS 改用 PTS
                // 推导，而直播重连造成的回退是 PTS 和 DTS 一起倒退，所以这一级必然修不好，
                // 每次都掉到第 3 级的整段 x264 重编码。
                "-fflags",
                "+genpts",
                "-i",
            ])
            .arg(src)
            .args([
                "-c",
                "copy",
                // 时间戳是容器/packet 元数据，修它不该动 H.264/AAC payload。setts 把每个
                // packet 的时间戳夹到「不小于上一个已输出的时间戳」，回退的那一段时间被压掉，
                // 成本是一次顺序读写而不是一次视频编码。
                //
                // 两处细节，改之前先看这里：
                // 1. 变量名是 PREV_OUTPTS / PREV_OUTDTS。没有 PREV_OUTTS，写错会直接
                //    "Error initializing bitstream filter: setts"。
                // 2. 分开写 pts=/dts= 而不是省事的 ts=：ts= 会把两者设成同一个值，有 B 帧
                //    的源会被破坏。直播 FLV 通常没有 B 帧，但不值得赌。
                "-bsf:v",
                SETTS_MONOTONIC,
                // 音频这条是链式：aac_adtstoasc 之后再跑 setts。不能写成第二个 -bsf:a，
                // 那是覆盖而不是追加。
                "-bsf:a",
                SETTS_MONOTONIC_AFTER_ADTSTOASC,
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
                context: Some(&self.context),
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 脚本化 fake：按预设依次返回 detect 结果，并决定 remux 成败。
    struct FakeFfmpeg {
        detect_results: Mutex<std::collections::VecDeque<AppResult<Detection>>>,
        remux_ok: bool,
    }

    impl FakeFfmpeg {
        fn new(detect: Vec<AppResult<Detection>>, remux_ok: bool) -> Self {
            Self {
                detect_results: Mutex::new(detect.into_iter().collect()),
                remux_ok,
            }
        }
    }

    fn anomalous(backward_ms: i64) -> AppResult<Detection> {
        Ok(Detection::Anomalous {
            max_backward_ms: Some(backward_ms),
        })
    }

    fn failed(message: &str) -> AppResult<Detection> {
        Err(error_stack::Report::new(AppError::Custom(message.into())))
    }

    #[async_trait]
    impl FfmpegRunner for FakeFfmpeg {
        async fn detect(&self, _path: &Path) -> AppResult<Detection> {
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
                Err(error_stack::Report::new(AppError::Custom(
                    "remux fail".into(),
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
        let f = FakeFfmpeg::new(vec![Ok(Detection::Clean)], true);
        assert_eq!(normalize_timestamps(&path, &f).await, RepairOutcome::Clean);
    }

    #[tokio::test]
    async fn repaired_when_small_backward_is_rewritten() {
        let path = p("repaired_when_small_backward_is_rewritten");
        let f = FakeFfmpeg::new(vec![anomalous(2_599), Ok(Detection::Clean)], true);
        assert_eq!(
            normalize_timestamps(&path, &f).await,
            RepairOutcome::Repaired(repaired_temp_path(&path))
        );
    }

    /// 闸门：回退量超过上限时**一次 remux 都不能发起**。setts 会把这种输入压成帧风暴，
    /// 而复检只看单调性、会认为修好了，于是坏片被当成成功产物上传、原片被删。
    #[tokio::test]
    async fn unfixable_when_backward_exceeds_limit() {
        let path = p("unfixable_when_backward_exceeds_limit");
        // detect 只排了一次结果：如果闸门放行去 remux，第二次 detect 会 panic。
        let f = FakeFfmpeg::new(vec![anomalous(MAX_REPAIRABLE_BACKWARD_MS + 1)], true);
        assert_eq!(
            normalize_timestamps(&path, &f).await,
            RepairOutcome::Unfixable
        );
        assert!(!repaired_temp_path(&path).exists());
    }

    #[tokio::test]
    async fn repaired_exactly_at_the_limit() {
        let path = p("repaired_exactly_at_the_limit");
        let f = FakeFfmpeg::new(
            vec![anomalous(MAX_REPAIRABLE_BACKWARD_MS), Ok(Detection::Clean)],
            true,
        );
        assert_eq!(
            normalize_timestamps(&path, &f).await,
            RepairOutcome::Repaired(repaired_temp_path(&path))
        );
    }

    /// 解析不出回退量不等于「没有回退」：ffmpeg 换了措辞时必须保守，不能盲目 clamp。
    #[tokio::test]
    async fn unfixable_when_backward_is_unparsed() {
        let path = p("unfixable_when_backward_is_unparsed");
        let f = FakeFfmpeg::new(
            vec![Ok(Detection::Anomalous {
                max_backward_ms: None,
            })],
            true,
        );
        assert_eq!(
            normalize_timestamps(&path, &f).await,
            RepairOutcome::Unfixable
        );
    }

    #[tokio::test]
    async fn unfixable_when_rewrite_leaves_anomaly() {
        let path = p("unfixable_when_rewrite_leaves_anomaly");
        let f = FakeFfmpeg::new(vec![anomalous(1_000), anomalous(1_000)], true);
        assert_eq!(
            normalize_timestamps(&path, &f).await,
            RepairOutcome::Unfixable
        );
        assert!(!repaired_temp_path(&path).exists());
    }

    #[tokio::test]
    async fn falls_back_when_detect_errors() {
        let path = p("falls_back_when_detect_errors");
        let f = FakeFfmpeg::new(vec![failed("ffmpeg missing")], true);
        assert_eq!(
            normalize_timestamps(&path, &f).await,
            RepairOutcome::Fallback(RepairFallbackReason::DetectFailed)
        );
    }

    #[tokio::test]
    async fn falls_back_when_remux_process_fails() {
        // 进程层面失败是环境问题，不阻断上传。
        let path = p("falls_back_when_remux_process_fails");
        let f = FakeFfmpeg::new(vec![anomalous(1_000)], false);
        assert_eq!(
            normalize_timestamps(&path, &f).await,
            RepairOutcome::Fallback(RepairFallbackReason::RemuxFailed)
        );
    }

    #[tokio::test]
    async fn falls_back_when_verification_errors() {
        let path = p("falls_back_when_verification_errors");
        let f = FakeFfmpeg::new(vec![anomalous(1_000), failed("verify fail")], true);
        assert_eq!(
            normalize_timestamps(&path, &f).await,
            RepairOutcome::Fallback(RepairFallbackReason::VerificationFailed)
        );
        assert!(!repaired_temp_path(&path).exists());
    }

    /// 造一段 `seconds` 秒的测试素材。
    async fn make_source(path: &Path, seconds: u32) {
        let status = tokio::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=duration={seconds}:size=320x240:rate=30"),
                "-f",
                "lavfi",
                "-i",
                &format!("sine=d={seconds}"),
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-c:a",
                "aac",
                "-shortest",
            ])
            .arg(path)
            .status()
            .await
            .expect("spawn ffmpeg");
        assert!(status.success());
    }

    /// 把 `src` 的 `[0, head_secs)` 和「从 `resume_secs` 起的剩余部分」拼成一个文件。
    ///
    /// FLV 允许裸拼 tag 流，去掉第二份的 13 字节头即可——录制器遇到 CDN 回放时落盘的
    /// 就是这个形状（`flv_writer` 按 tag 原样写回，不做任何偏移重基）。
    /// `keep_timestamps` 决定第二段保留原时间戳（CDN 回放重叠）还是从零重来（时间戳重置）。
    async fn splice(
        src: &Path,
        head_secs: &str,
        resume_secs: &str,
        keep_timestamps: bool,
        dst: &Path,
    ) {
        // 中间件名字必须跟着 dst 走：两个集成测试是并发跑的，共用固定名字会互相覆盖。
        let dir = dst.parent().unwrap();
        let stem = dst.file_stem().unwrap().to_str().unwrap();
        let head = dir.join(format!("{stem}.head.flv"));
        let tail = dir.join(format!("{stem}.tail.flv"));
        let status = tokio::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(src)
            .args(["-t", head_secs, "-c", "copy"])
            .arg(&head)
            .status()
            .await
            .expect("spawn ffmpeg");
        assert!(status.success());

        let mut command = tokio::process::Command::new("ffmpeg");
        command
            .args(["-hide_banner", "-loglevel", "error", "-y", "-ss", resume_secs, "-i"])
            .arg(src)
            .args(["-c", "copy"]);
        if keep_timestamps {
            command.args(["-copyts", "-avoid_negative_ts", "disabled"]);
        }
        let status = command
            .args(["-muxdelay", "0", "-muxpreload", "0"])
            .arg(&tail)
            .status()
            .await
            .expect("spawn ffmpeg");
        assert!(status.success());

        let mut spliced = tokio::fs::read(&head).await.expect("read head");
        let tail_bytes = tokio::fs::read(&tail).await.expect("read tail");
        spliced.extend_from_slice(&tail_bytes[13..]);
        tokio::fs::write(dst, spliced).await.expect("write splice");
        let _ = tokio::fs::remove_file(&head).await;
        let _ = tokio::fs::remove_file(&tail).await;
    }

    /// 需要本地 ffmpeg；手动运行：cargo test -p biliup-cli system_ffmpeg -- --ignored
    ///
    /// CDN 回放重叠：回退量远小于剩余内容，setts 能追上，必须修好。
    #[tokio::test]
    #[ignore]
    async fn system_ffmpeg_repairs_a_cdn_replay_overlap() {
        let dir = std::env::temp_dir();
        let good = dir.join("tsr_replay_source.flv");
        let replay = dir.join("tsr_replay.flv");
        make_source(&good, 30).await;
        splice(&good, "25", "21.4", true, &replay).await;

        assert!(
            matches!(
                SystemFfmpeg::default()
                    .detect(&replay)
                    .await
                    .expect("detect"),
                Detection::Anomalous { .. }
            ),
            "拼出来的样本应当检测到时间戳异常，否则这个测试什么也没验"
        );
        let outcome = normalize_timestamps(&replay, &SystemFfmpeg::default()).await;
        assert!(
            matches!(outcome, RepairOutcome::Repaired(_)),
            "回放重叠应当由 setts 修好，实际 {outcome:?}"
        );
        if let RepairOutcome::Repaired(fixed) = outcome {
            let _ = tokio::fs::remove_file(&fixed).await;
        }
        for path in [&good, &replay] {
            let _ = tokio::fs::remove_file(path).await;
        }
    }

    /// 时间戳重置（#13 的形态）：回退量吃掉了剩余内容，setts 会把后半段压成帧风暴，
    /// 而复检看不出来。闸门必须在动手之前拦住它。
    #[tokio::test]
    #[ignore]
    async fn system_ffmpeg_refuses_to_rewrite_a_timestamp_reset() {
        let dir = std::env::temp_dir();
        let good = dir.join("tsr_reset_source.flv");
        let reset = dir.join("tsr_reset.flv");
        make_source(&good, 30).await;
        splice(&good, "25", "21.4", false, &reset).await;

        let outcome = normalize_timestamps(&reset, &SystemFfmpeg::default()).await;
        assert_eq!(
            outcome,
            RepairOutcome::Unfixable,
            "时间戳重置必须被闸门拦住，绝不能产出被压扁的修复件"
        );
        assert!(!repaired_temp_path(&reset).exists());
        for path in [&good, &reset] {
            let _ = tokio::fs::remove_file(path).await;
        }
    }

    /// 干净文件走整条流程应得 Clean。
    #[tokio::test]
    #[ignore]
    async fn system_ffmpeg_detect_clean_on_generated_file() {
        let good = std::env::temp_dir().join("tsr_good.mp4");
        make_source(&good, 2).await;
        assert_eq!(
            SystemFfmpeg::default().detect(&good).await.expect("detect"),
            Detection::Clean,
            "正常文件不应报时间戳异常"
        );
        assert_eq!(
            normalize_timestamps(&good, &SystemFfmpeg::default()).await,
            RepairOutcome::Clean
        );
        let _ = tokio::fs::remove_file(&good).await;
    }
}
