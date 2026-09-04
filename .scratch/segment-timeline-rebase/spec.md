# 分段计时对时间基准跳变的免疫

对应 [issue #35](https://github.com/dplei/biliup/issues/35)。发起时版本 1.3.12。
与 [#32](https://github.com/dplei/biliup/issues/32) 同源——同一段代码的两个反向故障，
#32 修的是「永远不切」，本题是「秒级乱切」。

## 问题

抖音 FLV 重连后，CDN 会反复重发 timestamp=0 的关键帧。在「绝对媒体时钟（本例约 3.3e7 ms，
即 9 小时余）+ 重发 0」这个组合下，`Segmentable` 的计时被拖进一个循环：

1. 重连后首个关键帧把 `time.start` 锚定到绝对基准，正确；
2. 一个 timestamp=0 的关键帧命中 `set_time_position` 里 `number < start` 的回锚分支，
   **start 被拉到 0**；
3. 下一个正常关键帧带回绝对基准，`elapsed = 绝对基准 - 0` ≫ 配置的 1800s，`TimedSplit` 成立；
4. 切片后用当前关键帧重新锚定，下一个 0 又把它拉回去——回到第 2 步。

产出是十几秒、几百 KB 的碎片，全部低于 `filtering_threshold` 被判无效删除，成片对应位置直接缺失。
连续几段字节数完全相同，正是每轮循环装进去的那一批固定 CDN 初始化 tag。

## 根因不在回锚分支，在计时模型

`elapsed_time()` 是 `current.saturating_sub(start)`，这个式子**假设媒体时间轴单调**。
CDN 重发违反了这个假设，于是需要一个「回锚」补丁来兜住真实的时间轴回绕；补丁又区分不出
「换基准」和「单 tag 抖动」，就变成本题。

关键对照（issue 证据）：**分段中途**的 timestamp=0 不会早切，只有**重连后新建分段**会。
因为中途那次 start 已经落在更小的值上，回锚分支根本不触发。这说明触发条件是
「start 是个大数」而不是「出现了 0」——继续在回锚分支上加条件，是在给一个错的式子打补丁。

## 方案：把 elapsed 从「减法」换成「累加前向增量」

> ⚠️ **下面这段代码是初稿，已被证伪，留作「当时怎么想的」。** 它只推演了「单发」，
> 漏了 CDN **逐帧交替**重发（`[0, B+1000, 0, B+2000, …]`）——那种输入下每个增量都跨基准、
> 都超步长，`elapsed` 永远是 0，本段再也切不了片，正好翻回 #32 的故障。
> 实际落地的判据多了 `pending_base` 换基准确认与「时间戳未推进直接返回」两处，
> 推演过程见 [step 01 的落地小节](steps/01-accumulate-forward-deltas.md)。

```rust
struct Time { expected: Option<Duration>, elapsed: Duration, last: Option<Duration> }

/// 关键帧间隔正常在 10s 内；段内合法空档的上限由停顿看门狗兜住（默认 30s 就断连）。
/// 超过这个步长的前向跳变是换基准，不是真的过了这么久。
const MAX_STEP: Duration = Duration::from_secs(30);

pub fn set_time_position(&mut self, number: Duration) {
    if let Some(last) = self.time.last {
        let delta = number.saturating_sub(last);
        // 回退、或超步长跳变 = 时间基准不连续：只重新对齐，不计入本段时长
        if delta <= MAX_STEP { self.time.elapsed += delta; }
    }
    self.time.last = Some(number);
}
```

一条规则同时覆盖两个场景，回锚分支整个删掉：

| 场景 | 走哪个分支 | 结果 |
|---|---|---|
| 真回绕（换基准后持续递增） | 回退那一步不计，之后每步 ≤ MAX_STEP 照常累加 | 现有测试仍通过 |
| 瞬时抖动（→0 再跳回） | 回退不计 + 跳回超步长不计 | 本段时长不受影响，不早切 |
| 连接起始的空段 | 首帧就是 timestamp=0，同一条规则 | 一并修掉 |

`start`/`current` 是私有字段，外部只用公开方法，`httpflv.rs`、`hls.rs`、`stream-gears` 都不动。

**取舍**：段内真实空档（丢帧、CDN 卡顿）不再计入 elapsed，分段会略长于配置值。这反而更接近
「本段录到了多少内容」的语义，`file_size` 兜底不变。`MAX_STEP` 留成常量可调。

## 对 issue 里四条建议的处置

| 建议 | 处置 |
|---|---|
| 1 连续 N 个 tag 低于 start 才回锚 | **不做**。引入计数器状态和一个说不清的 N，而 CDN 恰恰是一次重发**一批**初始化 tag，N 容易被凑满 |
| 2 回锚时 current 一并归到新基准 | **已是现状**。`set_time_position` 末尾无条件 `current = number`，这条基于误读 |
| 3 elapsed 跃变 > 2× expected 就拒切 | **不单独做**。只挡切片动作、start 仍停在 0，之后每帧都被拒 → 退化成 #32。观测部分见 step 02 |
| 4 回归测试 | **做**，见 step 01。现有 `a_backward_timeline_rearms_the_segment_start` 保留 |

## 拆解

| step | 内容 | 依赖 |
|---|---|---|
| [01](steps/01-accumulate-forward-deltas.md) | `Time` 换模型 + 单测 + `parse_flv` 端到端回归 | — |
| [02](steps/02-observe-timeline-rebase.md) | 跨基准跳变的结构化事件（建议 3 的观测部分） | 01 |

step 01 单独上线即闭环；02 只补观测，可后置。

## 为什么不安排 dev 环境实跑

触发条件是「抖音 CDN 重连后重发 timestamp=0」，本地起不了这个上游。step 01 的端到端测试
直接用合成 FLV 复刻这一串输入（绝对基准递增 + 周期性插入 timestamp=0 关键帧），比守在
dev 前等一次不可控的重连更可控。生产侧的验收放在 issue 的 `awaiting-verification` 清单：
重连日志之后不再出现秒级 `split_limit`。

## 不做

- #36（碎片被静默删除时无事件）：独立问题，不在本次范围。
- 段内空档补偿（把跳变的时间按 wall clock 折算回 elapsed）：分段略长无害，等有人抱怨再说。
