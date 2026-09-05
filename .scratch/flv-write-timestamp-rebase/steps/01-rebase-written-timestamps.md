# 01 写盘侧统一 offset 重基

根因步骤。做完之后，新录制的分段文件里不再出现 CDN 基准跳变导致的 DTS 倒退。

## 改哪里

[`httpflv.rs`](../../../crates/biliup/src/downloader/httpflv.rs) 的缓存写出循环——就是当前
只 `warn!` + `dts_rollup.record(...)` 然后 `out.write_tag(tag_header, ...)` 那一段。
把写出的 `TagHeader` 换成 offset 映射后的副本（`TagHeader` 是 `Copy`，改副本即可）。

**不要改 [`flv_writer.rs`](../../../crates/biliup/src/downloader/flv_writer.rs)。**
`write_tag_header` 保持「给什么写什么」的语义，判据需要的上下文（是否换基准、切段状态）
全在 `httpflv.rs` 这一层，下沉进 writer 只会让它需要一份自己看不懂的状态。

## 状态与判据

抽一个小结构承载三个状态，便于单测——不需要跑网络也不需要真文件：

```rust
struct TimestampRebase {
    offset: i64,      // 当前基准的修正量
    last_src: Option<i64>,  // 上一个源时间戳，用来算 delta
    high_water: i64,  // 已写出的最大 emit，换基准时接在它之后
}

impl TimestampRebase {
    /// 返回该 tag 应当写出的时间戳；同时在换基准时更新 offset。
    fn map(&mut self, src: u32) -> u32 { ... }
    /// 本次调用是否判定为换基准（供事件上报，见下）。
    fn rebased(&self) -> bool { ... }
}
```

判据三条，**每条的理由见 spec，实现时不要随手放宽**：

1. `delta < -INTERLEAVE_TOLERANCE` → 换基准。小于容忍量的倒退是 audio/video 合法交错，
   **必须原样映射**（`emit` 允许小于 `high_water`）。
2. `delta > MAX_STEP` → 换基准。取 30s，与 `util.rs` 的 `MAX_STEP` 同源。
3. 其余 → 沿用当前 offset。

换基准时 `offset = high_water + NOMINAL_GAP - src`。`high_water` 用**已写出的最大值**而不是
上一个 emit，否则一次合法交错倒退后紧跟换基准会把新基准接错位置。

`INTERLEAVE_TOLERANCE` 与 `NOMINAL_GAP` 的取值先按码流实际交错幅度定，**在本文件里写下取值
理由**，别只留一个裸常数。

## 必须推演并留测的输入

spec 断言「交替重发下不需要 `pending_base`」——这是 #35 那轮**被证伪过一次**的同类推演，
本步必须自己验一遍，不许直接引用结论：

| 输入形态 | 期望 |
|---|---|
| 单发换基准 `[…, B, 0, 1000, 2000, …]` | 跳变处推进一个 `NOMINAL_GAP`，之后按真实增量累加 |
| 逐帧交替 `[0, B+1000, 0, B+2000, …]` | 每帧推进 `NOMINAL_GAP`，输出单调，时长偏差有界 |
| audio/video 合法交错 `[1000, 980, 1040, 1020, …]` | offset 不变，相对关系原样保留 |
| 大幅前跳换基准 | 同单发，不能被当成真的过了这么久 |
| 无跳变的正常码流 | 输出与输入逐值相等（`offset` 恒 0） |

最后一行是回归底线：**没有跳变时本改动必须是恒等映射。**

## 事件语义跟进

`DtsBackwardRollup` 现在的文案是「检测到时间戳倒退，继续录制并标记待检查」。重基之后文件
本身是好的，这句话会把运维引到错误结论上。至少要让事件说清「已重基」，并带上修正量。

事件名 `recording.dts_backward` 已进结构化日志覆盖清单（`.scratch/structured-logging/`
的 C04）与试点核对脚本，**改字段前先看那份清单**，改完把 C04 一起更新。

## 验证

1. 上面那张表逐行做成单测，跟现有 `dts_rollup_reports_the_first_jump_then_one_summary_per_segment`
   放一起。
2. 端到端用现成的 [`scripts/structured_logging/recording_pilot.py`](../../../scripts/structured_logging/recording_pilot.py)——
   它的 `inject_dts_backward` 就是为这个形态写的。跑完把产出文件喂
   `ffmpeg -v verbose -i FILE -c copy -f null -`，断言 stderr **不再**出现 DTS 异常行。
3. 注意该脚本的 fixture 里 timestamp=0 的关键帧本来就会触发 `dts_backward`，
   本步之后它的期望事实要跟着改（`.scratch/segment-timeline-rebase/steps/02` 里踩过）。
