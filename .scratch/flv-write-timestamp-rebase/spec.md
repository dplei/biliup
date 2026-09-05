# 写盘侧不修正 CDN 基准跳变，坏时间戳被原样落盘

来源：[dplei/biliup#13](https://github.com/dplei/biliup/issues/13)。发起时版本 1.3.15。

关联：#25（存量坏片的修复成本，`.scratch/timestamp-repair-cost/`，其 spec 已把「写入侧治本」
明确留给本题）、#32 / #35（同一段代码的**计时**故障，已合并；本题是**写盘**，两条独立路径）。

> 本目录用 `steps/` 存放实施步骤。**这些不是 GitHub issue**，编号只在本目录内有意义，
> 不要和 `dplei/biliup` 的 issue 编号混用或互相引用编号。

## 一句话

CDN 在分段中途重发 script tag 并把时间戳换基准（通常归零）时，录制侧只打一条告警就把
回退后的时间戳原样写进 FLV，产出的文件 DTS 不单调；解复用器按 32 位 wrap 展开后表现为
「第 N 秒跳到约 42.9 亿秒」，B 站转码据此拒稿。

## 现状核对（2026-09-05，逐条对代码）

issue 提了两条建议，加评论追加的一条，三者状态不同：

| # | 建议 | 状态 |
|---|---|---|
| 1 | 写入侧治本：检测到段中途 DTS 倒退时重设偏移基准或强制切段 | **未做**，根因仍在 → step 01 |
| 2 | `TranscodeFailed` fallback 分支必须先跑时间戳修复再上传 | **已不成立**，但无测试锁住 → step 03 |
| 3 | （评论追加）检测器不能只匹配 ffmpeg 文案，应解析数值 | **未做**，漏检路径仍在 → step 02 |

### 建议 2 为什么已不成立

[`upload.rs`](../../crates/biliup-cli/src/server/common/upload.rs) 的修复门是
`repair_enabled && !source_timestamps_clean`，而 `source_timestamps_clean` 只在
`NormalizationOutcome::Normalized { source_timestamps_clean: true, .. }` 时为真。
`TranscodeFailed` 走的是 `Original { reason }` 分支，clean 恒为 false，**一定进
`normalize_timestamps`**。这是 `8b78a14` + `80494a5` 顺带的结果，不是为本题做的修复。

顺带修正 issue 的取证推断：当年那份「fallback 日志之间没有 `timestamp_repair` 行」的证据
本身不成立。事发版本对应的代码是无条件 `normalize_timestamps`，`processing_decided` /
`processing_completed` 事件要到 `80494a5` 才有——**当时「跳过」和「静默通过」在日志上长得
一样**。真正让坏片过关的是建议 3 那条漏检，不是建议 2 的绕过。

## 根因

### R1. issue 正文的机制描述是反的

正文称「`flv_writer` 仍在用分段起点的旧偏移基准做减法，算出负数」。**核对结论：
`flv_writer` 从建档（2025-07-15）至今从未做过任何减法。**
[`write_tag_header`](../../crates/biliup/src/downloader/flv_writer.rs) 就是把
`tag_header.timestamp` 拆成 u24 + ext 原样写出，`git log -S` 也搜不到任何偏移运算。
`CODE_INDEX.md` 对该文件的描述同样是「按 tag 原样写回时间戳（不做任何偏移重基）」。

所以文件里落下的不是「负数回绕成的大数」，而是**如实记录的一次 DTS 倒退**。42.9 亿秒是
下游解复用器把这次倒退按 32 位 wrap 展开的结果——issue 评论里的推演才是对的。

这条更正改变方案方向：要做的不是「停止用旧基准做减法」，而是**开始做偏移映射**，方向相反。

### R2. 检测点只告警，不介入

[`httpflv.rs`](../../crates/biliup/src/downloader/httpflv.rs) 的缓存写出循环：

```rust
if tag_header.timestamp < prev_timestamp {
    warn!("Non-monotonous DTS in output stream; previous: {prev_timestamp}, current: {};", ...);
    dts_rollup.record(&out.file, prev_timestamp as u64, tag_header.timestamp as u64);
}
out.write_tag(tag_header, flv_tag_data, previous_tag_size_bytes)?;
prev_timestamp = tag_header.timestamp
```

`dts_rollup` 是 `#26` 那轮补的**观测**，只汇总不干预。写出值仍是 `tag_header.timestamp`。
且 `prev_timestamp` 紧接着被更新为回退后的值，所以换基准后的后续 tag 都大于它，
**整段只告警一次**，后面成百上千个坏时间戳静默落盘。

### R3. #32 / #35 的修复够不到这里

那两题改的是 `Segmentable::set_time_position`（`util.rs`），把分段计时从减法换成
「累加前向增量」。那是**决定何时切段**的标量，与**每个 tag 写出什么值**是两条路径，
`Segmentable` 也只被喂关键帧。本题不受其修复覆盖。

## 方案：写出前套一层统一 offset

在写出点把源时间戳映射一次，`emit = src + offset`；`offset` 只在确认基准不连续时更新：

```rust
let src = tag_header.timestamp as i64;
if let Some(last) = last_src {
    let delta = src - last;
    if delta < -INTERLEAVE_TOLERANCE || delta > MAX_STEP {
        // 基准不连续：让新基准接到「已写出的最大值」之后一个名义间隔
        offset = high_water + NOMINAL_GAP - src;
    }
}
let emit = (src + offset).max(0);
```

### 三个判据各自兜什么

- **`INTERLEAVE_TOLERANCE`（小幅倒退容忍）**：写盘侧喂进来的是 audio/video **交错**的全部
  tag，不是 `Segmentable` 那样只有关键帧。同一时刻的 audio 与 video tag 本来就可能小幅乱序，
  这是合法交错，**不是换基准**。直接复用 `util.rs` 的 `continuous_step`（判据是严格
  `to > from`）会把每个交错的 audio tag 都判成换基准，把音画同步彻底打散。这是本步最容易
  踩的坑。
- **`MAX_STEP`（大幅前跳）**：换基准不一定归零，也可能跳到一个更大的值。取值与 `util.rs`
  的 `MAX_STEP`（30s）同源同理由：段内合法空档由停顿看门狗兜住（默认 30s 就断连重连，
  重连会重进 `parse_flv`），所以段内不可能存在超过它的真实空档。
- **`offset` 全局唯一**：audio 与 video 必须用同一个 offset，同基准内的相对关系才不变，
  音画同步天然保住。**不要按流分别维护。**

### 为什么不选「强制切段」

issue 给的另一个选项是在跳变处强制切段重建基准。不选，理由是实测频次：`Unexpected script
tag` 在流量高的日子一天能出现几十上百次，逐次切段会产出大量短段，撞上
`filtering_threshold` 被判无效删除（正是 #11 / #36 那条链路），拿修复换成片缺口不划算。
offset 映射没有这个副作用，且段内单调后，上传侧那条昂贵的修复链路（#25）根本不会触发。

### 交替重发不需要 `pending_base`

#35 落地时踩过 CDN **逐帧交替**重发（`[0, B+1000, 0, B+2000, …]`）的坑，为此加了
`pending_base` 二次确认。本题不需要同样的机制：`Segmentable` 可以「不计入」某一步，而写盘
必须给每个 tag 一个具体的输出值。交替输入下每帧都走不连续分支、每帧推进 `NOMINAL_GAP`，
时间轴仍然单调，时长偏差有界。**推演必须在 step 01 里写下来并留测试，不能凭这段话默认成立。**

## 步骤

| 步骤 | 内容 |
|---|---|
| [01](steps/01-rebase-written-timestamps.md) | 写盘侧统一 offset 重基（根因），含 `recording.dts_backward` 事件语义跟进 |
| [02](steps/02-detect-anomaly-beyond-ffmpeg-text.md) | 检测器解析数值，堵掉「倒退被展开成单调大跳变」的漏检 |
| [03](steps/03-lock-fallback-repair-path.md) | 给 fallback 必经修复加回归测试，锁住已有行为 |

01 是根因，02 / 03 兜存量文件与防回退。**按 session 约定一轮一步。**

## 不做什么

- **不动 `Segmentable`**：#32 / #35 已收敛，本题与其无交集。
- **不改分段起点语义**：offset 跨段保持、不在切段处复位，各段仍是「CDN 绝对时间轴 + 累计
  修正」。归零会波及 #16 的 `start_time` 判据与既有测试，收益却是零。
- **不碰存量坏片的修复策略**：那是 #25。
