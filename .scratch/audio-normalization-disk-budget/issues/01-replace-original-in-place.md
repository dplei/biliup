# 01 — 产物校验加严并原子替换原片

Status: resolved（随 [#14](https://github.com/dplei/biliup/pull/14) 合入 `dev`；真实环境验收集中在 [`06`](./06-concurrency-acceptance.md)）

## 背景

见 [`spec.md` 第 2、3 节](../spec.md)。产物之所以把峰值推到 `2·N·S`，是因为它活到自己
上传结束，而上传是全局串行的。转码完成即替换原片，产物寿命缩到转码窗口内，峰值变成 `N·S + S`，
且不牺牲转码/上传的流水线重叠。

## 改动范围

[`audio_normalization.rs`](../../../crates/biliup-cli/src/server/common/audio_normalization.rs)

### 1. 校验加严

当前 `normalize_for_upload` 尾部的判据（非空、有音轨、原片有视频则产物有视频、时长 > 0）
是为「不过就丢弃、传原片」设计的，代价为零。原地替换后放过一个坏产物即永久损失，补三项：

1. **时长容差**：`|产物时长 − 原片时长| ≤ max(1.0s, 原片时长 × 0.005)`。
   原片时长在开头的 `runner.probe(source)` 里已经拿到，直接复用，不要多跑一次 ffprobe。
2. **字节数下界**：`产物字节数 ≥ 原片字节数 × 0.5`。视频 `-c copy`、音频重编到 192k 时产物
   应接近原片；掉到一半以下说明视频流没被搬过来。这是「明显异常」阈值，不是精确预算。
3. **视频流一致性**：产物的视频流数量与 `codec_name` 同原片一致。`-c copy` 下必然相等，
   不等即说明 ffmpeg 走了意料之外的路径。
   需要给 `ProbeStream` 补 `codec_name` 字段，`AudioProbe` 相应携带视频流的 codec 列表。

任一项不过：`artifact.cleanup()`，返回 `Original { reason: InvalidOutput }`，**原片不动**。
`InvalidOutput` 的现有语义（丢弃产物、直传原片）不变。

### 2. 原子替换

校验全过之后：

```text
fsync(.part)  →  rename(.part → 原片路径)  →  fsync(父目录)
```

- `rename` 同目录同文件系统，POSIX 保证原子。
- `TempArtifact` 的登记表在 rename 成功后必须移除该路径，否则
  `cleanup_orphaned_normalization_artifacts` 的活动集合会残留一条永不释放的项。
  推荐给 `TempArtifact` 加 `async fn commit_replacing(self, target: &Path) -> AppResult<()>`，
  内部完成 fsync + rename + 从 `ACTIVE_NORMALIZATION_ARTIFACTS` 注销，并
  `std::mem::forget` 掉自身或用 `ManuallyDrop`，防止 `Drop` 去删已经改名的文件。
- rename 失败（EXDEV、ENOSPC、权限）：删 `.part`，返回
  `Original { reason: TranscodeFailed }`，原片完好，本段直传原片。**不要**回退到「保留产物
  另存」——那正是要消灭的长寿命产物。

### 3. `NormalizationOutcome` 的形状变化

替换之后上传路径就是原片路径，`Normalized` 分支不再需要携带 `TempArtifact`。
但 `measurement` 与 `source_timestamps_clean` 仍要保留（后者是
[`upload.rs`](../../../crates/biliup-cli/src/server/common/upload.rs) 跳过整片时间戳扫描的依据）。

调用点连带改动：

- `upload_single_file_with_repair` 里 `normalization_artifact` 相关的分支、返回值第三元、
  以及上传失败时的 `artifact.cleanup()` 全部删除——**没有临时件需要清理了**。
- 三个 `(video, outcome, artifact, ...)` 元组解构点（约 `upload.rs:2139`、`2554`、`3509`）
  同步收窄。
- `segment_part_title` 那段为「临时文件名会变成分P标题」打的补丁可以去掉：路径不变，
  上传喂进去的就是原始文件名。**去掉时保留
  [`upload.rs:3819`](../../../crates/biliup-cli/src/server/common/upload.rs) 的回归测试**，
  改成断言标题来自原片路径即可，不要删测试。

`keep_original: true` 的分支保留旧行为，见 [`03`](./03-keep-original-switch.md)。

## 验收标准

1. 单测：产物时长偏离超过容差 → 返回 `InvalidOutput`，**原片内容未被修改**，`.part` 已删。
2. 单测：产物字节数不足原片一半 → 同上。
3. 单测：产物视频流 codec 与原片不一致 → 同上。
4. 单测：全部校验通过 → 原片路径的内容变为产物内容，`.part` 不存在，
   `ACTIVE_NORMALIZATION_ARTIFACTS` 为空。
5. 单测：rename 失败 → `TranscodeFailed`，原片完好，`.part` 已删。
6. 单测（回归）：分P标题仍来自原始录像文件名。
7. `cargo test -p biliup-cli` 全绿。
8. 本机冒烟：一段真实录像跑完，`ffprobe` 确认原片路径上的文件已是 AAC 48k 且整段
   LUFS 落在目标 ±1，目录下无 `.audio-normalized-*.part.*` 残留。

## 风险

产物质量问题从「可丢弃」变为「不可逆」。缓解就是第 1 节的三项加严，以及
[`03`](./03-keep-original-switch.md) 的 `keep_original` 逃生门。

时长容差取 0.5% 是因为 loudnorm 不改时长，正常偏差只来自容器时间基取整；若冒烟中发现
FLV → FLV 场景稳定偏差超过该值，先查是不是 `-c copy` 之外的路径被触发，不要直接放宽阈值。

## 实现记录（2026-08-30）

`normalize_for_upload` 增加 `keep_original` 参数，录像路径传 `false`、样片生成传 `true`
（样片要的是产物本身，截出来的 raw 才是中间件）。`NormalizedForm` 编码产物去向，
`ReplacedOriginal` 分支下 `upload_single_file_with_repair` 的第三元返回 `None`，于是三个
元组解构点一处未动。

两处与计划不同：

1. **`segment_part_title` 的补丁保留**，没有按计划删除。就地替换下路径不变、标题天然
   正确，但 `keep_original = true` 时上传的仍是临时件，那条补丁还有用。回归测试原样保留。
2. **体积判据只做下界，不做上界。** 冒烟里 64k/44.1kHz 的合成音频重编到 192k/48kHz 后，
   产物是原片的 1.88 倍。真实录像里视频占绝大部分，音频码率变化会被稀释，但这说明
   [`05`](./05-disk-watermarks.md) 的 `OUTPUT_SIZE_FACTOR = 1.1` 在音频占比高的素材上会
   低估。后果可控：准入放行后由硬水位兜住，中止并降级，不会写爆磁盘。

本机冒烟（验收 8）已通过：合成的 -24 dB 素材跑完整条链路，替换后的原片路径是 48 kHz
AAC，复测整段 -16.0 LUFS，目录无 `.part` 残留。证据固化为 `#[ignore]` 测试
`system_ffmpeg_replaces_the_original_with_a_normalized_recording`，需要本地 ffmpeg：

```bash
cargo test -p biliup-cli system_ffmpeg_replaces -- --ignored --nocapture
```
