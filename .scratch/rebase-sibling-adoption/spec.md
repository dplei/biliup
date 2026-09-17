# 写盘侧重基：一条流「确认换基准」会把兄弟流拖到错误的时间轴上

来源：[dplei/biliup#62](https://github.com/dplei/biliup/issues/62)。发起时版本 1.3.29（事发 1.3.28）。

关联：#13（`.scratch/flv-write-timestamp-rebase/`，本题是它引入的 `TimestampRebase` 的一个
设计漏洞）、#35（`pending_base` 二次确认的来源）、#25（上传侧修复策略，本题 step 02 与之相邻）。

> 本目录用 `steps/` 存放实施步骤。**这些不是 GitHub issue**，编号只在本目录内有意义。

## 一句话

`TimestampRebase` 在某一条流用两个样本「确认」了新基准后，会**无条件清空其它流的连续性判据**，
让它们的下一个 tag 直接套用新 offset。这个假设——「兄弟流也一起换了基准」——在 CDN 只是
重发了几个 timestamp=0 的垃圾 tag、真实码流仍在旧基准上时不成立：兄弟流的下一个 tag 带着
**旧基准的大时间戳**加上**按新基准（≈0）算出的 offset**，落盘时间戳前跳 ≈ 旧基准当前值。
#62 里的 +4215 s 就是 `4041193（新 offset）+ 4215 s（旧基准 src）`。

## 根因

### R1. issue 正文两条推断都不准，机制是「幻影确认 + 兄弟清空」

issue 分析 1 说「`prev_timestamp` 在 split 之后没有随 tag 推进」，分析 2 说「后续 tag 叠加了
归零前累计时长，偏移加了两次」。对着
[`httpflv.rs`](../../crates/biliup/src/downloader/httpflv.rs) 的 `TimestampRebase::map` 逐条核对：

- 日志里的 `previous` 是**该 tag 所属流（audio / video / script 三个槽之一）的 `last_src`**，
  不是全局的上一个 tag。`previous: 3790667` 恒定 424 s 不动，说明这两条 `current: 0` 的 tag
  属于一个 **424 s 内没收到过任何 tag 的槽**——只能是 Script 槽：它只在切段时收到 restamp 的
  onMetaData（时间戳 = 关键帧 K = 3790667），以及 CDN 重发的 script tag（时间戳恒 0）。
  audio / video 槽的 `last_src` 每个 tag 都在推进，当时已在 4215000 左右。
- 两条 `current: 0` 是**同一个槽连续两个样本**：第一条走「首次偏离」分支（`pending = 0`，
  写出 `high_water + 10 = 4041108`），第二条 `follows(pending, 0)` 成立 → **确认换基准**：
  `offset = high_water + 10 − 0 = 4041193`，然后**把 audio / video 槽的 `last_src` 清成 `None`**。
- audio / video 的下一个 tag 走 `None` 分支——没有判据可用，沿用当前 offset——
  `emit = 4215083 + 4041193 ≈ 8256 s`。这就是 ffprobe 看到的那一次跳变；之后两条流都在
  新时间轴上正常递增（video 槽随后自己再确认一次，`offset` 从 `high_water` 重新推导，落点
  一致），所以整段只有一处跳变、时长虚高 4215 s。

分析时对真实 `TimestampRebase` 跑过一条单测复现了同样形态（Script 槽 `last_src = K`，连发两个
`Script(0)`，video 下一帧前跳 3794020 ms ≈ K），测试原文见 step 01；另用一份 Python 仿真
穷举了 T+0 / T+424 的 tag 组合，结论同上。

### R2. Script 槽是最容易触发的，但 audio / video 同样会

同样的仿真里，video 槽连续两个 timestamp=0 的 tag（CDN 重发 `h264 sequence header + 关键帧`
这种成对的初始化 tag）后码流继续留在旧基准，audio 槽一样被拖跳。`#35` 处理过的是
**逐帧交替**重发（`[0, B, 0, B]`），`pending` 在 `src > last` 时被清掉，交替形态确认不了；
**成对**重发（`[0, 0, B]`）则刚好凑满两个样本。issue 附的近一周 `current: 0` 记录
（`previous: 123149, current: 0, written as 30` 等）看不出槽别，无法判断有几次是这条路径，
但修法对三个槽一视同仁，不需要分辨。

### R3. 为什么 #13 的设计没考虑到

#13 spec 把「修正量 offset 全局唯一」当成音画同步的保证，于是确认换基准时必须让兄弟流
**立刻**用上同一个 offset，「清掉判据、下一个 tag 直接落到新 offset」是达成这一点最短的写法。
它隐含了一个前提：能凑满两个样本的一定是真的基准变化。垃圾 tag 成对出现这个形态当时没有
推演过——#35 只推演了交替。

### 关于 T+0 那条 `previous: 0, current: 3790667`

Script 槽在 T+0 的切段处收到 restamp 的 onMetaData(K)，日志显示 `previous: 0`。要让它 424 s
后 `last_src` 停在 K，这条必须是**确认**而不是首次偏离（首次偏离不动 `last_src`）——即之前
不到 30 s 内还有一次切段（restamp 的 onMetaData 落在 `[K−30s, K]`，成为 `pending`），或者
issue 的日志摘录漏掉了中间的行。这个细节只影响「Script 槽的 `last_src` 怎么变成 K」，不影响
上面的机制，也不影响修法，不再追。

## 方案：兄弟流只在自己也偏离旧基准时才采纳新 offset

保留 #13 的全部判据（按槽分流、`pending` 二次确认、`REBASE_MAX_STEP_MS`、`high_water`），
只改「确认之后兄弟流怎么办」：

- offset 从全局一个改成**每槽一个**，`self.offset` 退化为「最近一次确认的 offset」，只给
  槽的第一个 tag（`None` 分支）用。
- 某槽确认换基准时，**不清兄弟槽的 `last_src`**，而是把新 offset 挂到兄弟槽的 `adopt` 上。
- 兄弟槽的下一个 tag：
  - `follows(last_src, src)` 成立 → 它仍在旧基准上，说明那次确认对它不成立（幻影，或它还没
    切过去）：沿用自己的 offset，**清掉 `adopt`**。
  - 不成立 → 它也偏离了旧基准，且兄弟已经确认：直接采纳 `adopt` 的 offset，`last_src = src`，
    不再走自己的 `pending` 二次确认。**不报 deviation**（与现在的 `None` 分支一致，事件语义不变）。

三个场景推演（仿真已验证，step 01 要落成单测）：

| 输入 | 现状 | 改后 |
|---|---|---|
| 幻影：某槽 `[0, 0]` 后码流留在旧基准 | 兄弟流前跳 ≈ 旧基准 src | 兄弟流恒等；该槽 2–3 个占位 tag 后自己再确认回旧基准，每步 ≤ 一帧 |
| 真换基准：两条流先后偏离 | 先确认的槽清掉兄弟，兄弟 `None` 分支套同一 offset | 兄弟偏离时采纳同一 offset，相对关系（音画同步）逐 ms 相同 |
| 逐帧交替 `[0, B+1000, 0, …]` | 单调、时长保住 | 不变（没有确认发生） |

**为什么不是「Script 槽不参与判据」**：那只堵 Script 这一条路，R2 的 audio / video 成对重发
照样触发。可以作为顺手的清理（Script tag 的时间戳对解复用器没有意义，它的 deviation 事件
纯属噪声），但不是根因修复，本次不做。

**为什么不是「确认要求第二个样本推进（`src > pending`）」**：能挡住 `[0, 0]`，挡不住
`[0, 33, B]` 这种带推进的成对重发；且它改的是「什么算确认」，而错的是「确认之后对兄弟做什么」。

**为什么不是 issue 的 B（归零即切段）**：#13 spec 已否决，理由不变——`Unexpected script tag`
高峰日一天几十上百次，逐次切段产出大量短段撞 `filtering_threshold` 被删。

## 上传侧现状（issue 建议 C 的核对结论）

[`timestamp_repair.rs`](../../crates/biliup-cli/src/server/common/timestamp_repair.rs) 已经有
issue 要的那一层：#13 step 02 加的 `detect_packet_jump` 用 `ffprobe -show_entries packet=dts_time`
按流比对相邻包，前跳 > 30 s 判 `Anomalous { max_backward_ms: None }`。**它大概率检出了这个文件**。
没拦住的原因是分档策略：`None` → `Unfixable` → **直传原片**（`normalize_timestamps` 的注释与
`upload.rs` 对 `Unfixable` 的处置都是「原片已直传成功」）。所以 C 的缺口不是「没检」，
是「检出后仍然上传」+「前跳没有修法」。这一段放 step 02，与本题根因分开推进。

## 步骤

| 步骤 | 内容 | 状态 |
|---|---|---|
| [01](steps/01-adopt-only-when-deviating.md) | 兄弟槽按「自己也偏离」采纳 offset（根因），复现测试 + 三场景单测 | 待做 |
| [02](steps/02-upload-gate-forward-jump.md) | 上传侧：前跳 `Unfixable` 的处置（不直传 / 用增量模型修） | 待决策 |

01 是根因，独立闭环。02 改的是上传策略，牵涉磁盘预算，需要主人拍板后再拆。
**按 session 约定一轮一步。**

## 不做什么

- 不动 `Segmentable`（`util.rs`）：分段计时不受影响。
- 不改 `flv_writer.rs`：仍是「给什么写什么」。
- 不做 `biliup recover --missing-id` 子命令：取回路径已由 `segment-recover` skill 覆盖
  （`upos_recovery_json` + `scripts/timestamp_shift.py`）；前跳形态的本机修复表达式要补，
  记在 step 02 的待办里。
