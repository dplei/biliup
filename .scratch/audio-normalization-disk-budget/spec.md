# Spec：响度标准化的磁盘预算——产物原地替换与两级水位

Status: ready-for-agent
来源：[`dplei/biliup#8`](https://github.com/dplei/biliup/issues/8)（2026-08-30 设计讨论）
分支：`dev`

本文只定设计与取舍，实现步骤拆在 [`issues/`](./issues/) 下。

---

## 1. 问题一句话

开启自动响度标准化后，磁盘最坏峰值是 `N·S 原片 + N·S 标准化产物`。
`NORMALIZE_SLOTS=1` 只限制了**同时运行的 ffmpeg**，不限制**同时存在的产物**。

## 2. 根因（比 #8 的描述更具体）

[`upload.rs:1613`](../../crates/biliup-cli/src/server/common/upload.rs) 的
`normalize_for_upload` 跑在 [`upload.rs:1664`](../../crates/biliup-cli/src/server/common/upload.rs)
的 `acquire_global_upload_permit()` **之前**，源码注释写明这是故意的：

> CPU/磁盘处理不占网络上传 permit。

于是 N 条管道各自转码完成、各自去排那个容量为 1 的上传 permit，产物就一个个堆起来，
每一份都活到自己上传结束。`N·S + N·S` 正是这么来的。

`TempArtifact` 的 `Drop` 会删文件，所以不存在泄漏——**问题不是产物没被回收，是它活得太久**。

## 3. 选定方案：产物校验通过后原子替换原片

转码产出 `.part` → 严格校验 → `rename` 覆盖原片路径 → 在生命周期账本上打「已标准化」标记。

峰值降到 `N·S + S`：只有正在转码的那一份 `.part` 是额外占用，`NORMALIZE_SLOTS=1`
保证它同时最多一份。

### 3.1 为什么不选 #8 的候选方案 1（产物槽位跟随 `TempArtifact` 生命周期）

那条路的峰值同样是 `N·S + S`，但代价是**放弃转码与上传的流水线重叠**。

permit 持有到上传结束，意味着一条管道在上传时，其他管道连转码都不能开始。全局上传
permit 本来就是 1，转码是当前唯一还能并行、还能被上传时间掩盖掉的环节；把它也串起来，
端到端从 `≈ Σ上传ᵢ` 变成 `Σ(转码ᵢ + 上传ᵢ)`。GB 级分段下这是实打实的净损失。

原地替换在同样的峰值下**完全保留流水线重叠**，因此严格优于候选方案 1。

### 3.2 连带失效的三条候选

选定原地替换后，#8 里的这几条不再需要，不要顺手实现：

| #8 候选 | 处置 | 原因 |
| --- | --- | --- |
| 1. 产物槽位跟随生命周期 | 不做 | 被 3.1 取代 |
| 4. 失败缓存全局字节/数量上限 | 本轮不做 | 产物寿命只剩转码窗口，不存在需要预算管理的长寿命缓存；若 #4 的复用方案落地再单独评估 |
| 5. watchdog 区分「等待产物槽位」 | 不做 | 不引入新排队，`NORMALIZE_SLOTS` 语义不变 |

保留并实现的是 #8 的候选 2、3（准入水位与转码期硬水位），见第 5 节。

## 4. 原地替换的四个约束

### 4.1 必须加「已标准化」标记，不能接受二次编码

替换后原片不复存在，补传读到的 `file_path` 已是标准化产物。若不打标记，补传会重新
measure + transcode：measure 得出 `input_i ≈ -16`、`offset ≈ 0`，增益几乎为零，但
**AAC→AAC 的有损重编码照做不误，还要多一整遍全片 IO**。补传本身会重试多次，损失会叠加。

`upload_missing_segment` 在 v2 下对每个通过校验的分段都建行
（见 [`11_add_upload_segment_lifecycle.sql`](../../crates/biliup-cli/migrations/11_add_upload_segment_lifecycle.sql)
与 [`segment_enrollment.rs`](../../crates/biliup-cli/src/server/common/segment_enrollment.rs) 的
`enroll_in_database`），标记加一列即可，成本远低于反复重编码。

> ⚠️ **命名陷阱**：该表已有列 `normalized_file_path`，它是**路径规范化**的结果
> （`normalize_segment_path`，用于唯一索引），与响度标准化毫无关系。新列必须另起名
> `audio_normalized_at`，实现时不要复用或改写既有列。

### 4.2 崩溃窗口：先 rename、后落标记

两步之间崩溃有两种排法，都无法消除窗口（跨文件系统与 DB 的两阶段提交不值得为此引入）：

- **先 rename 后落标记**（选定）：崩溃后文件已标准化、DB 说没有 → 补传多做一次有损编码。
- 先落标记后 rename：崩溃后 DB 说已标准化、文件仍是原片 → **静默漏掉一段的标准化**。

选前者：多一次编码是有界的、可从日志看出来的；静默漏掉难以发现。窗口本身也短（一次
`rename` 加一条 UPDATE）。

崩溃在 rename 之前则完全安全：`.part` 残留由既有的
`cleanup_orphaned_normalization_artifacts` 清理（它按 `.audio-normalized-` + `.part.`
匹配），原片完好。

### 4.3 校验必须比现在更严

现有校验（[`audio_normalization.rs`](../../crates/biliup-cli/src/server/common/audio_normalization.rs)
`normalize_for_upload` 尾部）只查：产物非空、有音轨、原片有视频则产物有视频、时长 > 0。
这套判据是为「校验不过就丢弃产物、传原片」设计的，**代价是零**。

原地替换后判据一旦放过坏产物就是永久损失，因此要补时长容差、字节数下界与视频流一致性。
具体见 [`issues/01`](./issues/01-replace-original-in-place.md)。

### 4.4 postprocessor 语义变化，需要一个退出开关

用户自定义 postprocessor 与补传此后拿到的都是标准化后的文件。新增
`audio_normalization_keep_original`，默认 `false`（即默认原地替换）。

默认值选 `false` 的理由：`audio_normalization_enabled` 本身默认 `false`，本变更只影响
主动开启标准化的用户；若默认保留原片，这个 effort 等于没做。行为变更写进 `CHANGELOG.md`
与 `public/config.yaml` 注释。

## 5. 两级磁盘水位（#8 候选 2、3）

与第 3 节正交，无论是否原地替换都要做，它是失败开放的安全网。

```text
准入（软水位）：required = 原片字节数 × SIZE_FACTOR + reserve_bytes
                available < required → 不启动 measure/transcode，直接传原片
硬水位：        转码期每 10s 检查，available < reserve_bytes
                → 取消 ffmpeg、删 .part、记录 DiskPressure 原因、降级直传原片
```

- `SIZE_FACTOR` 为代码常量 `1.1`：视频 `-c copy`、音频重编到 192k，产物通常在原片 ±10%。
- `reserve_bytes` 暴露为配置 `audio_normalization_disk_reserve_gib`，默认 `5`。
  只暴露这一个是因为不同机器磁盘容量差异大，而 `SIZE_FACTOR` 由编码参数决定，用户无从判断。
  这一取舍延续归档 spec 3.1 的「不提前暴露参数」。
- 探测目标是**分段文件所在目录**的文件系统，不是进程 CWD。
- 取消机制：`transcode` 现在是 `command.output().await`，用 `tokio::select!` 与周期检查
  竞速即可；`kill_on_drop(true)` 已经设好，drop 就会杀进程。
- 跨平台：unix 用 `libc::statvfs`（`libc` 已是 `biliup-cli` 直接依赖），非 unix 返回
  `None` 并**跳过检查**（fail-open），对齐
  [`process_priority.rs`](../../crates/biliup-cli/src/server/common/process_priority.rs)
  的 no-op 惯例。

新增两个 `OriginalReason`：`DiskAdmissionDenied`、`DiskPressureAborted`，走既有的
`audio_normalization = "fallback"` 日志线，不新增告警通道。

## 6. 不采用：拉流期单遍标准化（#8 的 `live` 模式）

评估结论是**不做，也不作为可选项开发**。三条理由，前两条 #8 原文低估了：

1. **默认录制路径根本不经过 ffmpeg。**
   [`downloader.rs:107`](../../crates/biliup-cli/src/server/core/downloader.rs) 只有显式选
   `Ffmpeg` 才走 `FfmpegDownloader`，其余一律 `StreamGears`（自研 FLV 逐 tag 解析）。
   `live` 模式等于强制把用户切到 ffmpeg 录制，代价是一次性作废：
   [`httpflv.rs`](../../crates/biliup/src/downloader/httpflv.rs) 的停顿看门狗与
   `ConnectionDiagnostics`、关键帧边界切分、
   [`util.rs`](../../crates/biliup-cli/src/server/common/util.rs) 的 `HeaderOnly` 与短分段
   判据、以及重连缺口测量。#8 里「非 FFmpeg 下载器的处理策略」一句带过的这项，实际是决定性的。

2. **重连即滤镜状态重置，而重连是常态。**
   [`download.rs:946`](../../crates/biliup-cli/src/server/common/download.rs) 的外层 loop
   每次断连都重新 `start_download`，`live` 模式下就是一个新 ffmpeg 进程，loudnorm 的前瞻
   状态归零 → **同一场直播里每次重连后音量重新爬升一次**。双遍的稳定来自「整段一个线性
   增益」；单遍动态模式换来的是听感上一跳一跳，方向是反的。

3. **不可逆。** 现有链路的原则是失败一律降级直传原片（七种 `OriginalReason` 全部指向这条）。
   `live` 的产物就是唯一那份录像：源突然静音把增益拉飞、采样率异常、编码器出错，都是永久
   毁掉一场。可选增强变成主链路单点。

顺带纠正 #8 的一处假设：**CPU 不是要担心的东西**。192k AAC 实时编码每路远不到一个核，
视频还是 `-c copy`。要验的是可靠性与不可逆性，不是算力余量。

若日后仍要重新评估，先做这个便宜实验，不要先写代码：取一段真实录像，跑单遍动态 loudnorm
与现有双遍，对比整段 LUFS 和**前 30 秒的短时响度曲线**（`ebur128` 滤镜的 M/S 值）。第 2 条
成立与否，那条爬升曲线一看便知。

## 7. 同样不采用：固定小片标准化后合并

沿用 #8 原文结论，无修正。补一条代码依据：upos 预检确实要求准确总大小
（[`line.rs:294`](../../crates/biliup/src/uploader/line.rs) 的 `total_size` 进
pre-upload 请求体），把 ffmpeg stdout 直接接上传器不成立。

## 8. Ticket

01–05 已实现（`dev` 之外的 `feat/audio-normalization-disk-budget` 分支），06 待人工验收。

| # | 标题 | 依赖 | 状态 |
| --- | --- | --- | --- |
| [01](./issues/01-replace-original-in-place.md) | 产物校验加严并原子替换原片 | — | implemented |
| [02](./issues/02-normalized-marker.md) | `audio_normalized_at` 标记与补传跳过 | 01 | implemented |
| [03](./issues/03-keep-original-switch.md) | `keep_original` 开关与行为变更说明 | 01 | implemented |
| [04](./issues/04-disk-space-probe.md) | 跨平台可用空间探测模块 | — | implemented |
| [05](./issues/05-disk-watermarks.md) | 准入水位与转码期硬水位取消 | 04 | implemented |
| [06](./issues/06-concurrency-acceptance.md) | 多路并发峰值验收 | 01–05 | ready-for-human |

## 9. 验收总纲

1. 多路并发下，标准化带来的额外磁盘占用**任何时刻不超过一份分段大小**。
2. 准入空间不足时不启动 measure/transcode，直接用原片，录制与上传不受影响。
3. 转码途中触及硬水位时 ffmpeg 被取消，`.part` 不残留。
4. 正常空间下响度结果与上传行为同现在一致（同一段素材，替换前后的 `input_i`/输出 LUFS 一致）。
5. 补传已标准化的分段不再触发第二次 measure/transcode。
6. `keep_original: true` 时行为完全退回当前实现。
