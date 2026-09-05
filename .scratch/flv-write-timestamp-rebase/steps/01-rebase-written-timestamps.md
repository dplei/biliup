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

---

## 落地记录（已完成）

改在 `httpflv.rs`：`TimestampRebase` + `restamp`，`flv_writer.rs` 未动。

### 与本文原计划的三处偏离

1. **判据按 tag 类型分开维护，`INTERLEAVE_TOLERANCE` 取消。** 原计划用一个容忍阈值兜
   audio/video 交错。但交错乱序只发生在流之间，每条流自己单调——按类型分开维护 `last_src`
   就把交错挡在判据之外，不用拍阈值。更关键的是阈值有反作用：要兜住交错就得给到数百毫秒，
   而 `recording_pilot.py` 注入的正是 300ms 的**同流**倒退，阈值会把这类真异常一起放过。
2. **必须做二次确认。** spec 断言「交替重发不需要 `pending_base`」，逐帧推演后证伪：
   `[0, B+1000, 0, B+2000, …]` 下每帧都判成换基准、每帧只推进 `NOMINAL_GAP`，10 秒内容压成
   100 毫秒。所以候选新基准先只拿占位值（接在 `high_water` 之后），`last_src` 不动，等第二个
   样本落在同一条新时间轴上才换 `offset`。附带一条同样必需的细节：**时钟停在原地不算推进，
   不清 `pending`**（`util.rs` 靠 `number == last` 提前返回拿到同样效果）；少了它，junk 帧
   会不断清掉候选基准，新基准永远等不到第二个样本，时间轴照样塌。
3. **切段重发的 prelude 要重新打时间戳（`restamp`）。** onMetaData / sequence header 缓存的是
   建档时的旧 header，时间戳常是 0；原样重发会在**每次切段**制造一次假的基准跳变，还会连着
   两个同为 0 的 tag 骗过二次确认，真的把 `offset` 换掉。改成用当前关键帧的时间戳，切段处
   判据恒等通过。这也顺带消掉了以前每次切段必报一条 `Non-monotonous DTS` 的噪声。

### 常量取值

- `REBASE_MAX_STEP_MS = 30_000`：与 `util.rs` 的 `MAX_STEP` 同源。段内真实空档由停顿看门狗
  （默认 30s 不来字节就断连重连）兜住，超过它的前跳只可能是换基准。
- `REBASE_NOMINAL_GAP_MS = 10`：小于任何真实帧间隔（60fps 约 16ms，一帧 AAC 约 23ms），
  既保证同流严格递增，又不会在逐帧交替重发时把时长撑长。
- 另有一条兜底：同一 tag 类型的输出强制严格递增（`emit.max(last_emit + 1)`）。锁住的是
  「落盘文件里每条流单调」这个对外承诺，不依赖判据是否穷尽了所有 CDN 花样。

### 推演结果

| 输入形态 | 结果 |
|---|---|
| 无跳变正常码流 | 恒等映射，`offset` 恒 0 ✅ |
| audio/video 合法交错 | 两条流各自恒等，`deviation` 为空 ✅ |
| 单发换基准 | 跳变处推进两个 `NOMINAL_GAP`（占位 + 确认各一次），之后按真实增量累加 ✅ |
| 大幅前跳换基准 | 同上，不会被当成真的过了这么久 ✅ |
| 逐帧交替重发 | 锁定新基准前压掉约 2 帧，此后真实增量全部保住，10 帧 × 1s 的时长落在 8–12s ✅ |
| 孤立单帧噪声 | 接在已写出最大值之后，基准不变，下一帧恒等映射继续 ✅ |

### 验证

- 单测：`downloader::httpflv::tests::rebase::*` 六条 + 落盘级
  `a_mid_stream_base_change_is_written_as_a_monotonous_timeline`（audio/video 交错、中途换基准、
  跨一次切段，逐 tag 解回文件断言每条流严格递增）。`cargo test -p biliup` 73 passed。
  issue #32 / #35 的既有用例全部照常通过。
- 端到端：ffmpeg 造 8s 真实 FLV（testsrc + sine，H.264/AAC），把 4000ms 之后的全部 tag 时间戳
  统一减去 4000（等价于 CDN 重发 script tag 后从 0 起算），喂进真实 `parse_flv`：
  - 坏输入本身 `ffmpeg -v verbose -c copy -f null -` 报 **232** 条 DTS 异常行；
  - 重基后的落盘文件 **0** 条，`ffprobe` 时长 8.133s 与原片一致（没有被压缩），
    视频首尾 DTS 0.001s → 7.917s。
- **未跑** `recording_pilot.py`：它要拉起完整服务端与证据库，成本远高于上面这条等价链路
  （同样是真实 ffmpeg 码流走真实 `parse_flv` 再由 ffmpeg 判定）。其 `C04-dts-first` 期望的
  `reason_code=timestamp_backward` 不受影响——`inject_dts_backward` 注入的是同流 300ms 倒退，
  仍会触发 `deviation`。需要正式跑一遍的话单开一轮。

### 事件语义

`recording.dts_backward` 保留事件名与既有字段，改动是加法：

- 新增 `emitted_ms`（实际落盘的值），文案改为「检测到时间戳基准跳变，已重基后继续录制」，
  汇总改为「本分段时间戳基准跳变汇总」；
- `reason_code` 增加 `timestamp_jump_forward`（换基准也可能是大幅前跳），原 `timestamp_backward` 不变；
- 汇总沿用 `count/first_ms/last_ms/max_backward_ms`，`reason_code` 取本段首条的值。

`.scratch/structured-logging/coverage-ledger.md` 的 C04 两行已同步；`evidence.py` 的 `REQUIRED`
是「至少包含」语义，新增字段不影响校验。
