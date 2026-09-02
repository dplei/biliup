# 05 · 守住「修复产物被压扁」这条静默毁片路径

Status: resolved
Blocked by: 03（已完成，证据在那边）
优先级：**P0 阻塞发布**——在这条守卫落地前，`b55763c` 的 setts 不能上生产

## 为什么

03 实测到：当回退量接近或超过剩余内容时长（时间戳重置／回绕，即 #13 的形态），
`setts` 的 `max()` 会把回退点之后的**全部真实内容**压进几百毫秒。

要命的是这条路径**全程静默**：

1. `remux_copy` + setts 退出码 0，产物非空——现有的两道 guard 都过。
2. `detect_anomaly(dst)` 返回 false——复检只看单调性，压扁的文件确实是单调的。
3. `normalize_timestamps` 返回 `Repaired(dst)`。
4. `upload.rs` 用 dst 上传，成功后走 `Repaired` 分支**删掉原片**。

结果是上传一个后半段成帧风暴的稿件，本地原片已删，**不可恢复**。

`max()` 的适用前提是「回退量 ≪ 剩余内容时长」。这个前提对重复回放成立，对回绕不成立，
而代码里没有任何地方检查这个前提。

## 改哪里

`timestamp_repair.rs`：在 `remux_copy` 之后、`detect_anomaly` 复检之外，加一道**内容跨度
校验**——修复产物的内容跨度不应比源显著缩短。不合格就当这一级没修好（删掉产物，继续走
后面的路径），绝不能返回 `Repaired`。

现成的东西可以直接用，别自己造：
[`audio_normalization.rs`](../../../crates/biliup-cli/src/server/common/audio_normalization.rs)
的 `content_span` 已经解决过同一个坑——「直录 FLV 的 `format.duration` 是末尾时间戳，
不是时长」，`output_is_faithful` / `OutputRejection` 是同型的校验形态。优先复用或照抄，
不要新写一套 probe 解析。

### 待定的决策（实现前先定，别默认）

- **阈值**：跨度缩短多少算不合格？回退量本身是合法的缩短（重复内容被压掉，03 形态一里
  这个量是 0，因为总时长不变）。一个保守起点是「缩短超过源跨度的百分之几就拒绝」，但
  具体数字要拿 03 的两个样本标定。
- **不合格之后走哪条路**：
  - (a) 直接 `Unfixable`——原片直传、本地留档、告警。语义最诚实，也最省。
  - (b) 继续掉到 x264 重编码——但 03 已经判断 x264 对大回退同样会压扁或超时，
    大概率只是把静默毁片换成 30 分钟白烧。
  倾向 (a)。若选 (a)，x264 fallback 就彻底没有存在理由了，可以在本步一并删掉，
  连带 [04](./04-shared-ffmpeg-permit.md) 标 `wontfix`。

## 验收

- 新增 `#[ignore]` 集成测试，用 03 的构造法造**两个**样本：
  - 重复回放（回退 ~3.6s，剩余内容 ~8s）→ 断言 `Repaired`，跨度不缩水；
  - 时间戳重置（回退到 0）→ 断言**不是** `Repaired`。
  构造脚本见 03 的 Answer，可直接搬。
- `cargo test -p biliup-cli --lib` 全绿。

## 注意

- 这一条属于「不能偷懒」的类别：它挡的是不可恢复的数据损坏，不是性能或整洁度。
- 顺带核对一下 `upload.rs` 的 `Repaired` 分支——先删修复临时件、再删原片的顺序，在这条
  守卫加上之后是否还有别的路径能在产物不可信时删原片。

## Answer

两个待定决策都由主人拍板：阈值我给，不合格后走 (a) 原片直传，且**本地不留文件**——
原片已经直传成功，需要重修时凭上传凭证从 B 站 OS 库取回即可。

### 判据没有用「内容跨度校验」

spec 原本设想的「产物跨度不应比源显著缩短」实测**不成立**：

- `format.duration` 在有回退的 FLV 上返回的是**回退点**，不是真实跨度——重复回放样本和
  时间戳重置样本都读出 12.023s，两者真实内容都是 20s。拿不到真实跨度。
- 就算全片扫 packet 拿到时间戳 max，也挡不住重置形态：它的源时间戳 max 本身就丢了真实
  时长（B 段从 0 重来，max 仍是 A 段末尾）。
- 更糟的是重置形态的**产物跨度反而比源大** 0.35s（clamp 每包 +1ms 累积出来的），
  「缩短」这个方向根本判不出它。

### 采用的判据：最大单次 DTS 回退量，上限 10 秒

`MAX_REPAIRABLE_BACKWARD_MS = 10_000`。关键论证是**不需要知道总时长**：
`max()` 的损害上限恰好等于回退量——最坏情况下回退点之后的内容追不上，被压进
「回退量 × 1ms/packet」的窗口，被毁的内容不会超过回退的那一段。

取 10 秒的依据：

| 案例 | 回退量 | 判定 |
| --- | ---: | --- |
| 生产片段 | 2.6s | 放行 |
| CDN 回放重叠（本地样本） | 3.6s | 放行 |
| 时间戳重置（本地样本） | 25.0s | 拒绝 |
| 时间戳回绕（#13 形态） | 分钟～小时级 | 拒绝 |

- 回退量的物理含义是 CDN 回放的重叠时长，边缘缓冲区是秒级；10 秒对实测值留约 4 倍余量。
- 万一误放行，损害上限是 10 秒内容被压成快进，对 30–60 分钟分段是 0.5% 以下。
- 解析不出回退量时**保守拒绝**，不退化成「回退量 0」。ffmpeg 换措辞会表现为一批 `Unfixable`
  告警，而不是静默毁片。

回退量从 detect 已有的全片扫描里顺带解出来（`ffmpeg_scan::parse_backward_ms`），
零额外 IO；闸门在 remux 之前拦截，超限时**一次 ffmpeg 都不发起**。

### 顺带删掉了 x264 重编码

选了 (a) 之后重编码彻底没有存在理由：setts 按构造保证产物单调，闸门又挡住了它修不了的
形态，第 2 级永远不会被触发。删掉 `FfmpegRunner::reencode`、`SystemFfmpeg::reencode`、
`REENCODE_TIMEOUT`（[01](./01-reencode-internal-timeout.md) 加的超时随之移除）以及
dev-dependencies 里为它加的 `tokio/test-util`。issue #25 的成本问题至此从根上消失。

### 本地不留文件

`upload.rs` 四处 `Unfixable` 分支统一改为「照常走 postprocessor 清理」，只保留告警，
文案抽成 `unfixable_alert`。原来那句「已保留本地文件，请手动处理」不再成立。

### 验收

- `cargo test -p biliup-cli`：350 passed, 0 failed（全 crate 全绿）。
- `--lib system_ffmpeg -- --ignored`：5 passed，其中两条是本步新增的端到端样本——
  `system_ffmpeg_repairs_a_cdn_replay_overlap`（回放重叠 → `Repaired`）和
  `system_ffmpeg_refuses_to_rewrite_a_timestamp_reset`（时间戳重置 → `Unfixable`，
  且不留下修复临时件）。
- 单测覆盖闸门边界：恰好等于上限放行、超出一格拒绝、解析不出拒绝，且拒绝路径下
  `detect` 只被调用一次（放行去 remux 会让 fake panic）。

### 遗留

时间戳重置/回绕的片子现在会以原片直传 + 告警落地，本地不留档。重修流程改到本机 macOS
上做，见 [06](./06-macos-side-repair.md)。
