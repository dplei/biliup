use crate::server::common::ffmpeg_scan::{
    MAX_PACKET_STEP_MS, PacketJumpScan, ScanObserver, run_packet_jump_scan, run_scanning_stderr,
};
use crate::server::common::process_priority::background;
use crate::server::errors::{AppError, AppResult};
use async_trait::async_trait;
use biliup_observability::Context as EventContext;
use error_stack::{ResultExt, bail};
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tracing::{error, info, warn};

/// 时间戳重写用的 setts 表达式：**按流累加增量**，而不是把时间戳夹成单调。
///
/// 旧写法 `max(TS, PREV_OUT+1)` 只对「回退后追得上」的 CDN 回放重叠有效：回退量一大，回退点
/// 之后的内容全被压进 1ms/packet 的窗口（帧风暴），所以曾经有一道 10 秒闸门把时间戳重置
/// 拦成 `Unfixable`；对前跳（#62 的形态，以及 32 位回绕被解复用器展开后的单调大跳变）它
/// 结构上无效——输入本来就单调。
///
/// 增量模型两种都修：每个 packet 的输出 = 上一个输出 + 本流相邻两个输入的增量；增量为负
/// （回退）或超过 `MAX_PACKET_STEP_MS`（前跳、回绕）都压成 1ms，之后按真实增量继续。内容一帧
/// 不丢，文件时长等于真实内容时长，闸门不再需要。A/V 各自压缩，跳变处相对偏移改变不超过一帧。
/// `PREV_OUTDTS` 在第一个 packet 上是 NOPTS（一个极大的负数），用 `lt(…, -1e15)` 识别。
/// 首包**不能原样放行**：各流只压自己流内的跳变，首包就落在跳变之后的那条流会带着原始
/// 时间戳，音视频起点错开跳变的全长（issue #75：视频从 0 开始、音频从 1616s 开始）。
/// ffmpeg 在 bsf 之前已经减掉了文件起点（所有流里最早的首包），所以首包 DTS 超过
/// `MAX_PACKET_STEP_MS` 就等于「本流起点比最早的流晚了一次跳变」，把它拉回 0；没超过的
/// 原样保留，正常文件的音画起点差一个毫秒都不动。表达式在输入 timebase 上求值（FLV 为
/// 1ms），30000 与 `MAX_PACKET_STEP_MS` 同源，录制侧与上传侧对「什么算跳变」保持一个口径。
///
/// ponytail: 被拉回 0 的流丢掉了它与兄弟流原本几十毫秒以内的起点差；一条流真的晚 30s 以上
/// 才有第一个包（前 30s 纯视频）也会被拉齐。都没在直播录像里见过，见到再按兄弟流的首包对齐。
///
/// ponytail: 一个文件里若 CDN 逐帧交替重发 timestamp=0 的垃圾 tag（`[B, 0, B+33, 0, …]`），
/// 每个垃圾 tag 会吃掉紧随其后那个真 tag 的增量，时间轴按 tag 数被压缩。录制侧的
/// `TimestampRebase` 已经不会把这种形态写进文件，这里不再为它加 NEXT_DTS 前瞻。
fn delta_setts(chain: &str) -> String {
    const STEP: &str = r"if(gt(DTS-PREV_INDTS\,30000)\,1\,max(DTS-PREV_INDTS\,1))";
    const FIRST: &str = r"if(gt(DTS\,30000)\,0\,DTS)";
    // pts 表达式不能引用 dts 表达式的结果，只能把同一段算式再写一遍，再补回原来的 PTS−DTS
    // （composition time），有 B 帧的源不会被破坏。
    format!(
        "{chain}setts=pts=if(lt(PREV_OUTDTS\\,-1e15)\\,{FIRST}+PTS-DTS\\,PREV_OUTDTS+{STEP}+PTS-DTS)\
         :dts=if(lt(PREV_OUTDTS\\,-1e15)\\,{FIRST}\\,PREV_OUTDTS+{STEP})"
    )
}

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
    // 1) 检测。检测出错是环境问题，保守降级直传原片。
    let backward = match runner.detect(path).await {
        Ok(Detection::Clean) => return RepairOutcome::Clean,
        Ok(Detection::Anomalous { max_backward_ms }) => max_backward_ms,
        Err(e) => {
            warn!(file = ?path, "时间戳检测失败，降级直传原片: {e:?}");
            return RepairOutcome::Fallback(RepairFallbackReason::DetectFailed);
        }
    };

    // 2) 增量模型对回退与前跳一视同仁，回退量只用来留痕；解析不出数值（前跳、措辞变化）
    //    也照样修，一律以复检为准。
    match backward {
        Some(ms) => info!(file = ?path, backward_ms = ms, "检测到时间戳回退，尝试修复"),
        None => info!(file = ?path, "检测到时间戳异常（前跳或回退量未解析），尝试修复"),
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

#[derive(Default)]
pub struct SystemFfmpeg {
    context: EventContext,
}

impl SystemFfmpeg {
    pub fn with_context(context: EventContext) -> Self {
        Self { context }
    }

    /// 用 ffprobe 的包时间戳做数值判据：流内前跳与跨流起点差。
    ///
    /// 只在文本判据判干净之后才跑：命中文本判据时已经知道文件有问题，也已经有回退量，
    /// 没必要再读一遍整片。健康文件因此多付一遍解复用（实测与现有 `-c copy -f null -`
    /// 同量级）。
    ///
    /// ffprobe 本身跑不起来时**返回 `None` 而不是报错**：这一层是给文本判据兜底的加法，
    /// 把它的环境故障升级成 `DetectFailed` 会让没装 ffprobe 的机器上每个文件都降级，
    /// 比漏掉这条判据更糟。
    async fn packet_scan(&self, path: &Path) -> Option<PacketJumpScan> {
        let mut command = Command::new("ffprobe");
        command
            .args([
                "-v",
                "error",
                "-show_entries",
                "packet=stream_index,dts_time",
                "-of",
                "csv=p=0",
            ])
            .arg(path);
        match run_packet_jump_scan(
            background(&mut command),
            ScanObserver::quiet("timestamp_packet_scan", path).with_context(&self.context),
        )
        .await
        {
            Ok((status, scan)) if status.success() => Some(scan),
            Ok((status, _)) => {
                warn!(file = ?path, ?status, "ffprobe 包扫描非零退出，本条判据跳过");
                None
            }
            Err(e) => {
                warn!(file = ?path, "ffprobe 包扫描无法执行，本条判据跳过: {e:?}");
                None
            }
        }
    }
}

#[async_trait]
impl FfmpegRunner for SystemFfmpeg {
    async fn detect(&self, path: &Path) -> AppResult<Detection> {
        // 全片扫描：-c copy -f null，只读不重编码。
        // 使用 verbose 级别确保 "Invalid timestamp" / "Application provided invalid" 等
        // 低于 warning 的模式也能输出；-nostats 抑制进度行噪声。
        //
        // 不能加 `-fflags +igndts`：它丢掉文件里的 DTS 让解复用器从 PTS 反推。B 帧源的 PTS
        // 本来就不单调，反推只在「每个 PTS 只出现一次」时成立——CDN 重发一帧（同 PTS，录制侧
        // 给它 DTS+1）就让反推出的 DTS 相等或倒退，muxer 报 "non monotonically increasing
        // dts"，而文件自己的 DTS 严格单调。这种假阳性 setts 修不掉（没东西可修），复检照样
        // 命中，整段被判 Unfixable 扣住不传（2026-09-21 腾讯云两段 40 分钟录像）。
        let mut command = Command::new("ffmpeg");
        command
            .args(["-hide_banner", "-loglevel", "verbose", "-nostats", "-i"])
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
        // 文本判据没话说不等于文件干净：源里的 32 位倒退可能已经被解复用器展开成一个单调的
        // 巨大前跳，展开之后 muxer 一条警告都不会打（实测 0 条）。这是确定性漏检，再用包
        // 时间戳的数值兜一层。
        let Some(scan) = self.packet_scan(path).await else {
            return Ok(Detection::Clean);
        };
        if let Some(jump_ms) = scan.max_forward_jump_ms {
            warn!(
                file = ?path,
                jump_ms,
                limit_ms = MAX_PACKET_STEP_MS,
                "相邻包时间戳异常大幅前跳，判为时间戳异常"
            );
            // 前跳不是回退，回退量无从谈起：按既有约定交给 `None` 分支。
            return Ok(Detection::Anomalous {
                max_backward_ms: None,
            });
        }
        // 各流各自单调、只是起点错开，流内判据全都看不见；这正是修复产物与未重基的 script
        // 数据包被拒稿的形态（issue #75）。修复要能把它对齐，复检也靠这一条把关。
        let skew_ms = scan.start_skew_ms();
        if skew_ms > MAX_PACKET_STEP_MS {
            warn!(
                file = ?path,
                skew_ms,
                limit_ms = MAX_PACKET_STEP_MS,
                "各流起点相差过大，判为时间戳异常"
            );
            return Ok(Detection::Anomalous {
                max_backward_ms: None,
            });
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
                // 只要一路视频、一路音频。FLV 的 script 数据包（ffprobe 里是一条 text 流）不跟着
                // 录制侧重基时会比音视频早一整段（issue #75），留着它产物照样起点错位；投稿也
                // 用不上它。
                "-map",
                "0:v:0?",
                "-map",
                "0:a:0?",
                "-c",
                "copy",
                // 时间戳是容器/packet 元数据，修它不该动 H.264/AAC payload。setts 按流累加
                // 增量重写时间戳（见 `delta_setts`），成本是一次顺序读写而不是一次视频编码。
                //
                // 两处细节，改之前先看这里：
                // 1. 变量名是 PREV_OUTPTS / PREV_OUTDTS / PREV_INDTS。写错会直接
                //    "Error initializing bitstream filter: setts"。
                // 2. 分开写 pts=/dts= 而不是省事的 ts=：ts= 会把两者设成同一个值，有 B 帧
                //    的源会被破坏。直播 FLV 通常没有 B 帧，但不值得赌。
                "-bsf:v",
                &delta_setts(""),
                // 音频这条是链式：aac_adtstoasc 之后再跑 setts。不能写成第二个 -bsf:a，
                // 那是覆盖而不是追加。
                "-bsf:a",
                &delta_setts("aac_adtstoasc,"),
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

    /// 增量模型没有回退量上限：时间戳重置（回退分钟级）也是一次 remux + 复检。
    #[tokio::test]
    async fn repaired_when_backward_is_large() {
        let path = p("repaired_when_backward_is_large");
        let f = FakeFfmpeg::new(vec![anomalous(600_000), Ok(Detection::Clean)], true);
        assert_eq!(
            normalize_timestamps(&path, &f).await,
            RepairOutcome::Repaired(repaired_temp_path(&path))
        );
    }

    /// 解析不出回退量（前跳、或 ffmpeg 换了措辞）不再是「保守放弃」：增量模型不需要知道
    /// 回退量，修完以复检为准。
    #[tokio::test]
    async fn repaired_when_backward_is_unparsed() {
        let path = p("repaired_when_backward_is_unparsed");
        let f = FakeFfmpeg::new(
            vec![
                Ok(Detection::Anomalous {
                    max_backward_ms: None,
                }),
                Ok(Detection::Clean),
            ],
            true,
        );
        assert_eq!(
            normalize_timestamps(&path, &f).await,
            RepairOutcome::Repaired(repaired_temp_path(&path))
        );
    }

    /// 表达式里的步长必须与包扫描的判据同源，否则会出现「扫描判异常、重写却放过」的夹缝。
    #[test]
    fn delta_setts_uses_the_packet_scan_step() {
        assert!(delta_setts("").contains(&format!("\\,{MAX_PACKET_STEP_MS})")));
        assert!(delta_setts("aac_adtstoasc,").starts_with("aac_adtstoasc,setts=pts="));
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
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-ss",
                resume_secs,
                "-i",
            ])
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

    /// 时间戳重置（#13 的形态）：回退量吃掉了剩余内容。旧的夹取写法会把后半段压成帧风暴，
    /// 增量模型把后半段接在前半段之后，产物时长 = 前段 25s + 后段（`-ss 21.4 -c copy` 会退到
    /// 前一个关键帧 16.7s，所以后段实际 13.3s）≈ 38s。
    #[tokio::test]
    #[ignore]
    async fn system_ffmpeg_repairs_a_timestamp_reset() {
        let dir = std::env::temp_dir();
        let good = dir.join("tsr_reset_source.flv");
        let reset = dir.join("tsr_reset.flv");
        make_source(&good, 30).await;
        splice(&good, "25", "21.4", false, &reset).await;

        let outcome = normalize_timestamps(&reset, &SystemFfmpeg::default()).await;
        let RepairOutcome::Repaired(fixed) = outcome else {
            panic!("时间戳重置应当被增量模型修好，实际 {outcome:?}");
        };
        let duration = probe_duration(&fixed).await;
        assert!(
            (36.0..40.0).contains(&duration),
            "后半段必须接在前半段之后而不是被压扁，实测 {duration}s"
        );
        for path in [&good, &reset, &fixed] {
            let _ = tokio::fs::remove_file(path).await;
        }
    }

    /// `ffprobe` 读产物的容器时长。
    async fn probe_duration(path: &Path) -> f64 {
        let output = tokio::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "csv=p=0",
            ])
            .arg(path)
            .output()
            .await
            .expect("spawn ffprobe");
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .expect("parse duration")
    }

    /// 把 `from_ms` 之后的所有 FLV tag 时间戳整体前移 `jump_ms`。
    ///
    /// 产出的是**单调**时间轴，只是一步跨了几十天——32 位倒退被解复用器展开之后就是这个
    /// 形状。样本用代码生成而不是入库一个二进制片，改判据时能直接调参数复现。
    async fn jump_timestamps(src: &Path, from_ms: u32, jump_ms: u32, dst: &Path) {
        let mut data = tokio::fs::read(src).await.expect("read source");
        let mut offset = 13; // FLV 头 9 字节 + 第一个 previous tag size 4 字节
        let mut moved = 0;
        while offset + 11 <= data.len() {
            let size = u32::from_be_bytes([0, data[offset + 1], data[offset + 2], data[offset + 3]])
                as usize;
            let timestamp = u32::from_be_bytes([
                data[offset + 7],
                data[offset + 4],
                data[offset + 5],
                data[offset + 6],
            ]);
            if timestamp >= from_ms {
                let jumped = timestamp.wrapping_add(jump_ms);
                data[offset + 4] = ((jumped >> 16) & 0xff) as u8;
                data[offset + 5] = ((jumped >> 8) & 0xff) as u8;
                data[offset + 6] = (jumped & 0xff) as u8;
                data[offset + 7] = ((jumped >> 24) & 0xff) as u8;
                moved += 1;
            }
            offset += 11 + size + 4;
        }
        assert!(moved > 0, "样本里没有可前移的 tag，这个用例什么也没验");
        tokio::fs::write(dst, data).await.expect("write jumped");
    }

    /// issue #13 评论指出的漏检形态：时间轴单调，muxer 一条警告都不打（实测 0 条），
    /// 旧的纯文本判据会判 Clean 让坏片原样上传。数值判据必须把它认出来。
    #[tokio::test]
    #[ignore]
    async fn system_ffmpeg_detects_a_monotonous_giant_jump() {
        let dir = std::env::temp_dir();
        let good = dir.join("tsr_jump_source.flv");
        let jumped = dir.join("tsr_jump.flv");
        make_source(&good, 8).await;
        jump_timestamps(&good, 4_000, 4_294_000_000, &jumped).await;

        assert_eq!(
            SystemFfmpeg::default()
                .detect(&jumped)
                .await
                .expect("detect"),
            Detection::Anomalous {
                max_backward_ms: None
            },
            "单调大跳变必须被判为异常；前跳没有回退量，按约定交给 None 分支保守处理"
        );
        // #62 的形态：前跳被增量模型压成 1ms，产物回到真实的 8s。
        let outcome = normalize_timestamps(&jumped, &SystemFfmpeg::default()).await;
        let RepairOutcome::Repaired(fixed) = outcome else {
            panic!("前跳应当被增量模型修好，实际 {outcome:?}");
        };
        let duration = probe_duration(&fixed).await;
        assert!(
            (7.5..8.5).contains(&duration),
            "前跳必须被压掉，产物时长应回到 8s，实测 {duration}s"
        );
        for path in [&good, &jumped, &fixed] {
            let _ = tokio::fs::remove_file(path).await;
        }
    }

    /// 逐 tag 改写 FLV 时间戳：`shift(tag_type, timestamp)` 返回新值。`text_tag` 为真时在
    /// 第一个 tag 后插一个 timestamp=0 的 onTextData script tag——ffprobe 把它认成一条只有
    /// 一个包的 text 流，就是录制侧没跟着重基的那个数据包。
    async fn restamp_flv(src: &Path, dst: &Path, text_tag: bool, shift: impl Fn(u8, u32) -> u32) {
        let bytes = tokio::fs::read(src).await.expect("read flv");
        let mut out = bytes[..13].to_vec();
        let mut pos = 13;
        while pos + 11 <= bytes.len() {
            let size =
                u32::from_be_bytes([0, bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]) as usize;
            let end = pos + 11 + size + 4;
            let mut tag = bytes[pos..end].to_vec();
            let ts = u32::from_be_bytes([tag[7], tag[4], tag[5], tag[6]]);
            let [ext, b1, b2, b3] = shift(tag[0], ts).to_be_bytes();
            tag[4..8].copy_from_slice(&[b1, b2, b3, ext]);
            out.extend_from_slice(&tag);
            if text_tag && pos == 13 {
                // AMF0: "onTextData" + ECMA array { text: "hi" }
                let mut body = vec![2, 0, 10];
                body.extend_from_slice(b"onTextData");
                body.extend_from_slice(&[8, 0, 0, 0, 1, 0, 4]);
                body.extend_from_slice(b"text");
                body.extend_from_slice(&[2, 0, 2]);
                body.extend_from_slice(b"hi");
                body.extend_from_slice(&[0, 0, 9]);
                let len = body.len() as u32;
                out.push(18);
                out.extend_from_slice(&len.to_be_bytes()[1..]);
                out.extend_from_slice(&[0; 7]);
                out.extend_from_slice(&body);
                out.extend_from_slice(&(11 + len).to_be_bytes());
            }
            pos = end;
        }
        tokio::fs::write(dst, out).await.expect("write restamped");
    }

    /// ffprobe 读各流起点。
    async fn probe_starts(path: &Path) -> Vec<f64> {
        let output = tokio::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=start_time",
                "-of",
                "csv=p=0",
            ])
            .arg(path)
            .output()
            .await
            .expect("spawn ffprobe");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| line.trim().parse().expect("parse start_time"))
            .collect()
    }

    /// 断言修复后各流起点对齐（相差不到 0.1s）、时长回到真实的 8s。
    async fn assert_aligned_repair(sample: &Path) {
        assert!(
            matches!(
                SystemFfmpeg::default()
                    .detect(sample)
                    .await
                    .expect("detect"),
                Detection::Anomalous { .. }
            ),
            "起点错位必须被判为异常，否则这个测试什么也没验"
        );
        let outcome = normalize_timestamps(sample, &SystemFfmpeg::default()).await;
        let RepairOutcome::Repaired(fixed) = outcome else {
            panic!("起点错位应当被修好，实际 {outcome:?}");
        };
        let starts = probe_starts(&fixed).await;
        assert_eq!(starts.len(), 2, "只保留一路视频一路音频：{starts:?}");
        assert!(
            (starts[0] - starts[1]).abs() < 0.1,
            "起点必须对齐：{starts:?}"
        );
        let duration = probe_duration(&fixed).await;
        assert!(
            (7.5..8.5).contains(&duration),
            "产物时长应为 8s，实测 {duration}s"
        );
        let _ = tokio::fs::remove_file(&fixed).await;
    }

    /// issue #75 修复产物的形态：一条流只有首包在跳变前，另一条流首包就在跳变后。曾经各流
    /// 首包原样放行，修出来两条流起点错开整个跳变，复检还判干净。
    #[tokio::test]
    #[ignore]
    async fn system_ffmpeg_aligns_a_stream_that_starts_after_the_jump() {
        let dir = std::env::temp_dir();
        let good = dir.join("tsr_late_start_source.flv");
        let sample = dir.join("tsr_late_start.flv");
        make_source(&good, 8).await;
        restamp_flv(&good, &sample, false, |_, ts| {
            if ts > 0 { ts + 1_616_666 } else { ts }
        })
        .await;
        assert_aligned_repair(&sample).await;
        for path in [&good, &sample] {
            let _ = tokio::fs::remove_file(path).await;
        }
    }

    /// issue #75 的另一种：音视频自己对齐，一个 script 数据包比它们早一整段。
    #[tokio::test]
    #[ignore]
    async fn system_ffmpeg_drops_a_stale_data_packet() {
        let dir = std::env::temp_dir();
        let good = dir.join("tsr_stale_data_source.flv");
        let sample = dir.join("tsr_stale_data.flv");
        make_source(&good, 8).await;
        restamp_flv(&good, &sample, true, |kind, ts| {
            if kind == 18 { ts } else { ts + 2_400_000 }
        })
        .await;
        assert_aligned_repair(&sample).await;
        for path in [&good, &sample] {
            let _ = tokio::fs::remove_file(path).await;
        }
    }

    /// 在 `src`（FLV）里把第 `nth` 个视频 tag 原样复制一份插到它后面，时间戳 +1——录制侧
    /// `TimestampRebase` 给 CDN 重发帧的落盘形状。
    async fn duplicate_video_tag(src: &Path, nth: usize, dst: &Path) {
        let bytes = tokio::fs::read(src).await.expect("read flv");
        let mut out = bytes[..13].to_vec();
        let (mut pos, mut seen) = (13, 0);
        while pos + 11 <= bytes.len() {
            let size =
                u32::from_be_bytes([0, bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]) as usize;
            let end = pos + 11 + size + 4;
            let tag = &bytes[pos..end];
            out.extend_from_slice(tag);
            if tag[0] == 9 {
                seen += 1;
                if seen == nth {
                    let mut dup = tag.to_vec();
                    let ts = u32::from_be_bytes([dup[7], dup[4], dup[5], dup[6]]) + 1;
                    let [ext, b1, b2, b3] = ts.to_be_bytes();
                    dup[4..8].copy_from_slice(&[b1, b2, b3, ext]);
                    out.extend_from_slice(&dup);
                }
            }
            pos = end;
        }
        assert!(seen >= nth, "source has too few video tags");
        tokio::fs::write(dst, out).await.expect("write dup");
    }

    /// B 帧源里 CDN 重发一帧（同 PTS、DTS+1）不是时间戳异常：文件自己的 DTS 严格单调。
    /// 曾经 detect 带 `+igndts`，从 PTS 反推 DTS 在重复帧处必然倒退，整段被误判 Unfixable。
    #[tokio::test]
    #[ignore]
    async fn system_ffmpeg_detect_clean_on_bframe_source_with_a_resent_frame() {
        let dir = std::env::temp_dir();
        let src = dir.join("tsr_bframe_src.flv");
        let status = tokio::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
            ])
            .arg("testsrc=duration=3:size=320x240:rate=30")
            .args(["-c:v", "libx264", "-preset", "ultrafast", "-bf", "3", "-an"])
            .arg(&src)
            .status()
            .await
            .expect("spawn ffmpeg");
        assert!(status.success());
        let dup = dir.join("tsr_bframe_dup.flv");
        duplicate_video_tag(&src, 40, &dup).await;
        assert_eq!(
            SystemFfmpeg::default().detect(&dup).await.expect("detect"),
            Detection::Clean,
            "重发一帧的 B 帧源不应报时间戳异常"
        );
        let _ = tokio::fs::remove_file(&src).await;
        let _ = tokio::fs::remove_file(&dup).await;
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
