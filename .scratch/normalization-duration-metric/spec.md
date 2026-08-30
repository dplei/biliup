# Spec：响度标准化的时长口径——FLV 非零起始时间戳导致的全量误判

Status: ready-for-human（01–05 已实现，本机验收通过；真实录制环境的结束态待观察）
来源：[`dplei/biliup#16`](https://github.com/dplei/biliup/issues/16)（1.3.3 引入）
分支：`dev`

本文只定设计与取舍，实现步骤拆在 [`issues/`](./issues/) 下。

---

## 1. 问题一句话

一致性判据把 `ffprobe` 的 `format.duration` 当成时长直接相减，
但**直录 FLV 的 `format.duration` 是末尾时间戳，不是时长**。
非首段分段的 `start_time` 远大于 0，于是 `drift` 恒等于 `start_time`，
除首段外 100% 判 `duration_drift` 并回退原片。

后果是「只烧 CPU，不生效」：每段两遍 loudnorm 跑满核 3～8 分钟，产物随即丢弃，
上传的始终是未标准化的原片。1.3.3 的原地替换行为变更实际从未生效过一次。

## 2. 根因

### 2.1 `format.duration` 在 FLV 上不是时长

`flvdec` 在读头时若拿不到可信的 `onMetaData.duration`，会 seek 到文件尾读**最后一个 tag 的
时间戳**并把它当作 `duration`；随后 `update_stream_timings` 取
`FFMAX(duration, end - start)`，末尾时间戳恒大，于是胜出。

分段录像沿用整场 session 的时间轴（同 [#13](https://github.com/dplei/biliup/issues/13) 里
`Segmentable` 的 `start` 语义），所以除首段外 `start_time` 都远大于 0。

**本机可复现**——关键是同时复现两个条件：时间轴有偏移，且没有可信的 `onMetaData.duration`：

```
$ ffmpeg -f lavfi -i "testsrc=duration=6:..." -f lavfi -i "sine=duration=6" \
      -c:v libx264 -c:a aac -output_ts_offset 3600 -flvflags no_duration_filesize live.flv
$ ffprobe -show_entries format=duration,start_time -show_entries stream=start_time,duration live.flv
stream 0 (video) start_time=3600.000000  duration=N/A
stream 1 (audio) start_time=3599.977000  duration=N/A
format           start_time=3599.977000  duration=3605.900000   ← 末尾时间戳
```

真实跨度 = 3605.900 − 3599.977 = **5.923 s**。线上一段 30 分钟分段实测同构：
`start_time ≈ 398407`、`duration ≈ 400208`、真实跨度 1801 s。

三点值得记住：

- **`stream.duration` 是 `N/A`**，指望换成逐流时长是死路，只有 `format` 这一层有数。
- **`format.start_time` 是有的**，`-show_format` 已经返回，不需要额外探测。
- 去掉 `-flvflags no_duration_filesize` 后 `duration` 立刻变回 6.023（正确跨度）——
  这正是本机验证漏掉它的原因，见 2.3。

### 2.2 判据代入后必然失败

产物那侧 ffmpeg 把输出时间轴归零（实测 `start_time=0.000`），于是：

```
source_duration = 400208      （末尾时间戳）
tolerance       = 400208 × 0.005 = 2001
output_duration = 1801        （真实跨度）
drift           = |1801 − 400208| = 398407  ≫ 2001   → duration_drift
```

注意 `tolerance` 也被同一个错值放大了 200 倍却仍然挡不住——**分子的错比分母的错大一个量级**，
所以放宽容差不是解法，换口径才是。

### 2.3 为什么本机验证没抓到

发版前的真机冒烟用的是 `ffmpeg` 直接合成的单个 FLV，与生产分段有**两处独立差异**，
每一处都单独足以掩盖这个 bug：

1. 时间轴从 0 开始（首段场景）；
2. `flvenc` 在 trailer 里回填了正确的 `onMetaData.duration`，压过了末尾时间戳估计。

结论不是「测得不够多」，而是**测试素材的产生方式与生产不同构**。修复必须连这个一起补，
否则下次换个判据还会踩同一个坑。见 [`issues/05`](./issues/05-offset-timeline-fixtures.md)。

## 3. 同一根因的第二处：样片截取

[`audio_normalization.rs`](../../crates/biliup-cli/src/server/common/audio_normalization.rs)
的 `create_reference_sample` 用同一个 `duration_seconds` 算截取位置：

```rust
let length = duration.clamp(0.1, 30.0);
let start  = ((duration - length) / 2.0).max(0.0);   // -ss <start>
```

代入 `duration = 3605.9` 得 `-ss 1788`。而 ffmpeg 的输入 `-ss` 会**自动叠加
`ic->start_time`**，实际 seek 到 `3599.977 + 1788` ≈ 5388 s——早已越过文件尾。

本机实测这条命令**退出码为 0，产出一个 257 字节的空 m4a**，而代码只检查
`result.status.success()`，于是空文件被当作合法样片继续往下走。

这一处**早于 1.3.3**（随音量样片功能一起引入），不是本次回归，但根因完全相同，
一并修掉，见 [`issues/03`](./issues/03-sample-extraction-window.md)。

## 4. 选定方案：统一「内容跨度」口径

`duration_seconds` 的语义从「容器报的 duration」改成**内容跨度**：

```
span = format.duration − format.start_time      （start_time 缺失/非有限按 0）
```

原片与产物走**同一个解析函数**，口径天然一致：产物 `start_time = 0`，`span` 就是它自己的时长。

`AudioProbe` 同时保留 `container_duration` 与 `start_seconds` 两个原值，**只为日志**——
判据一律用 `span`。留原值是因为这次事故里最贵的一环是「不 ffprobe 根本看不出差多少」。

### 4.1 为什么不改成解析 ffmpeg 的 `time=` 输出

测量那一遍已经完整 demux 过原片，它的 stderr 里有权威的解码时长。但那是**文本刮取**，
格式随 ffmpeg 版本漂移，而且只覆盖测量这条路径、覆盖不到样片截取。
容器层的 `span` 已经足够，且两处共用。若将来 `span` 被证伪，再把 `time=` 作为交叉校验加上。

### 4.2 边界处理

| 情形 | 处置 |
| --- | --- |
| `start_time` 缺失或 `N/A` | 按 0，退化成当前行为 |
| `start_time` 为负（音频提前量） | 按 0 截断，不让 `span` 虚增 |
| `span ≤ 0` | 视为「探不到时长」，**跳过时长判据**，不当失败 |

最后一条是刻意的：时长探不到时其余三条判据（有音频、视频流一致、体积下界）仍然生效，
用「少一条判据」换「不误杀」，方向与现有 `filter(|v| *v > 0.0)` 一致。

## 5. 这次事故暴露的第二层问题：失败不可诊断、不自限

口径 bug 是一次性的，但它能连续烧五小时无人察觉，靠的是两个结构性缺陷：

1. **判据失败只打 `reason=` 字符串，不打实测值。** 排查必须人肉去 ffprobe 现场文件，
   而分段可能已经被清理。→ [`issues/02`](./issues/02-diagnostic-rejection-logs.md)
2. **确定性失败会无限重试。** 每段代价 3～8 分钟满核 ffmpeg，在单核 2 GiB 机器上还与
   录制抢 CPU。判据本身是安全的（回退原片、不动数据），但**代价不是零**。
   → [`issues/04`](./issues/04-rejection-circuit-breaker.md)

熔断的设计取舍写在 04 里，核心是：**只对「连续同一 reason」跳闸**，
因为那才是「系统性问题」的信号；偶发坏分段不该关掉整个功能。

## 6. 不需要做的事

- **无数据损坏，不需要回填。** 失败路径是「删产物、传原片」，1.3.3 期间上传的都是完好的
  未标准化原片。`audio_normalized_at` 没打标记，但那些分段已经传完，不会再走补传。
- **不放宽容差。** 见 2.2，错的是分子不是分母。
- **不回滚 1.3.3 的原地替换。** 那部分逻辑本身没被证伪——它压根没被执行到过。

## 7. 验收总纲

1. 用与生产同构的 FLV（非零 `start_time` + 无 `onMetaData.duration`）跑完整链路，
   结束态为 `completed` 且原片被替换。
2. 判据失败的 WARN 能独立定位问题，不需要回到现场 ffprobe。
3. 样片截取在非零时间轴上取到真实音频（非空产物）。
4. 连续同 reason 失败会跳闸并留 ERROR，成功一次即复位。
5. 真实录制环境确认结束态为 `completed`（这一条只能上线后验）。
