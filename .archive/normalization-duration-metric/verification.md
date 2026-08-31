# 验证记录

## 本机（已完成）

### 素材复现

生产现象需要**两个条件同时成立**才复现，缺一即退化成正常 FLV：

```bash
ffmpeg -f lavfi -i "testsrc=duration=6:size=320x240:rate=10" \
       -f lavfi -i "sine=frequency=440:duration=6" \
       -c:v libx264 -c:a aac -output_ts_offset 3600 \
       -flvflags no_duration_filesize live.flv
```

探得 `container_duration=3605.9 / start=3599.977 / span=5.923`，与线上分段同构
（线上一段 30 分钟分段是 `start ≈ 398407`、`duration ≈ 400208`、真实跨度 1801 s）。

### 红→绿

把 `content_span` 临时改回「直接取 `format.duration`」，以下用例全部失败；改回来后全绿：

| 用例 | 覆盖 |
| --- | --- |
| `probe_reports_the_content_span_not_the_end_timestamp` | 解析层口径 |
| `an_offset_source_and_its_zero_based_output_are_consistent` | 判据层，本次回归的核心护栏 |
| `a_non_positive_span_skips_the_duration_check_instead_of_failing` | 跨度算不出正数时不误杀 |
| `a_rejection_carries_the_numbers_it_judged_on` | 失败证据字段 |
| `sample_capture_reads_real_audio_from_an_offset_timeline`（真机） | 样片截取，修复前取到空容器 |

### 真机（`-- --ignored`）

四条全过：

- `system_ffmpeg_normalizes_a_segment_recorded_on_an_offset_timeline`
  ——素材自检通过（确认复现了现象）后跑完整链路，结束态 `ReplacedOriginal`，复测 **-16.04 LUFS**。
- `sample_capture_reads_real_audio_from_an_offset_timeline` ——样片 367 KB / 30.0 s
  （修复前是几百字节的空容器）。
- `system_ffmpeg_replaces_the_original_with_a_normalized_recording`（FLV + MP4）——未退化。
- `concurrent_normalization_never_keeps_more_than_one_artifact` ——并发峰值仍为 1 份。

### 熔断

三条单测覆盖：连续同 reason 三次跳闸且只报告一次；中间成功过不跳闸；两种 reason 交替
六次不跳闸。另有一条走完整 `normalize_for_upload`，断言跳闸后 `measure`/`transcode`
不再被调用、原片不动。

## 真实录制环境（已完成）

在 dev 环境跑通：本地起 `biliup server` + `next dev`，用本地 sqlite 与一个仅自己可见的
投稿模板，对着一个真实开播的抖音直播间录了 12 分钟，分段 2 分钟，`keep_original: true`。

### 结论

| 指标 | 结果 |
| --- | --- |
| `audio_normalization="completed"` | **6 / 6** |
| `reason="duration_drift"` | **0** |
| `audio_normalization="fallback"` | **0** |
| `audio_normalization="disabled"`（熔断） | **0** |
| 上传完成 | 6 / 6 |

### 关键发现：**第一段的 `start_time` 也不是 0**

设计时假设「每场第一段 `start_time ≈ 0`，只有后续分段会踩坑」。实测不成立——CDN 推来的
时间轴本身就不从 0 开始，第一段探得：

```
start_time=273.986  duration=396.119   → 真实跨度 122.13 s（分段设的就是 2 分钟）
```

也就是说**每一段都会踩**，包括第一段。这解释了线上「13 次全失败、没有一次例外」，
比 spec 第 2 节推断的「除首段外」更严重。

### 时长口径实测（第六段，`keep_original` 留下的产物）

| | `format.start_time` | `format.duration` | 内容跨度 |
| --- | --- | --- | --- |
| 原片 | 877.250 | 997.506 | **120.256** |
| 产物 | 0.000 | 120.321 | **120.321** |

drift = 0.065 s，容差 1.0 s → 通过。
按修复前的口径 drift = \|120.321 − 997.506\| = **877.2 s**，容差 4.99 s → 必然 `duration_drift`。

### 响度（顺带观察，非本次改动引入）

产物实测 `Integrated -16.2 LUFS / True Peak -1.3 dBTP`，目标是 -14。原片 -21.5，
需要 +7.5 dB，实际只上去 +5.3 dB——真峰限制（`TP=-1.5`）把线性增益卡住了，
loudnorm 退回动态模式所以打不满目标。这是 loudnorm 在高真峰素材上的既有行为，
与本次时长口径的改动无关，但值得单独评估要不要放宽 `TP` 或改用双遍线性模式。

### 环境复原

`segment_time` 与 `audio_normalization_keep_original` 已改回原值，主播地址与备注已还原。
