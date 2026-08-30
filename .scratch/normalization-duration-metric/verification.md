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

## 真实录制环境（待观察）

1. 结束态出现 `audio_normalization="completed"`，且不再有 `reason="duration_drift"`。
2. `keep_original: true` 跑一场，`ffprobe` 核对产物时长与原片真实跨度一致后再关掉。
3. 熔断不误跳闸——正常运行的一场里不应出现 `audio_normalization="disabled"`。
