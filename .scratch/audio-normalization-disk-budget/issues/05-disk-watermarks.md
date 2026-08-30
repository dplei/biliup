# 05 — 准入水位与转码期硬水位取消

Status: ready-for-agent
Blocked by: 04

## 背景

见 [`spec.md` 第 5 节](../spec.md)。这是 #8 的候选 2、3，与原地替换正交：即便产物寿命
已经缩到转码窗口，磁盘也可能在转码途中被别的东西吃光。失败开放，绝不阻断录制与上传。

## 改动范围

[`audio_normalization.rs`](../../../crates/biliup-cli/src/server/common/audio_normalization.rs)

### 1. 两个新的 `OriginalReason`

```rust
DiskAdmissionDenied,   // 准入不过，没启动 ffmpeg
DiskPressureAborted,   // 转码途中触及硬水位，已取消
```

走既有的 `audio_normalization = "fallback"` 日志线，不新增告警通道。日志里带上
`available_bytes`、`required_bytes`，让事后能判断是阈值定得紧还是磁盘真的满了。

### 2. 准入检查

位置在拿到 `NORMALIZE_SLOTS` permit **之后**、`measure` 之前——排队期间空间会变，
排队前的判断到执行时已经过期。

```text
required = 原片字节数 × SIZE_FACTOR + reserve_bytes
available_bytes(原片目录) < required  →  返回 Original { DiskAdmissionDenied }
```

- `SIZE_FACTOR` 为代码常量 `1.1`（视频 `-c copy`、音频重编到 192k，产物通常在原片 ±10%）。
- `reserve_bytes` 来自新配置 `audio_normalization_disk_reserve_gib`，默认 `5`。
- `available_bytes` 返回 `None` 时**放行**（fail-open）。

只暴露 reserve 一个参数：磁盘容量因机器而异，用户判断得了；`SIZE_FACTOR` 由编码参数决定，
用户无从判断。延续归档 spec 3.1 的「不提前暴露参数」。

### 3. 转码期硬水位

`transcode` 现在是 `command.output().await` 一把梭，改成与一个周期检查竞速：

```rust
tokio::select! {
    result = command.output() => result,
    _ = watch_disk_pressure(output_dir, reserve_bytes, Duration::from_secs(10)) => {
        // select 落地即 drop 掉 output() 的 future，kill_on_drop(true) 杀掉 ffmpeg
    }
}
```

- 检查周期 10s。GB 级分段的转码是分钟量级，10s 的粒度足够，也不会把 `statvfs` 调成热点。
- 判据是 `available_bytes < reserve_bytes`（不含 `SIZE_FACTOR`：此刻产物已经在写，
  要守的是最后那道保留线）。
- `kill_on_drop(true)` 已经设好，无需额外杀进程逻辑；随后 `artifact.cleanup()` 删 `.part`。
- 探测不出可用空间时，`watch_disk_pressure` 永不触发（fail-open）。

### 4. 配置项

[`config.rs`](../../../crates/biliup-cli/src/server/config.rs) 新增
`audio_normalization_disk_reserve_gib`，默认 `5`，校验范围 `1..=1024`；越界与另外两个字段
一样在 `validate` 里报错。同步更新 `public/config.yaml` 与前端全局设置。

## 验收标准

1. 单测：可用空间小于 `required` 时，`measure`/`transcode` 均未被调用，返回
   `DiskAdmissionDenied`（用可注入的空间探测桩，不要真去填磁盘）。
2. 单测：可用空间充足时行为不变（回归）。
3. 单测：空间探测返回 `None` 时放行——`measure` 被调用（fail-open）。
4. 单测：转码进行中探测跌破 reserve → 返回 `DiskPressureAborted`，`.part` 已删，
   `ACTIVE_NORMALIZATION_ARTIFACTS` 为空，**原片未被修改**。
5. 配置单测：默认 5；越界报错；per-streamer override 生效。
6. 两条降级路径都不影响上传：上传照常拿原片跑完。
7. `cargo test -p biliup-cli` 全绿。

## 风险

阈值定得太保守会让标准化在磁盘偏紧的机器上长期静默不生效。因此两条降级都必须留下带
`available_bytes`/`required_bytes` 的日志，且 `reserve_gib` 可调——不要只记一句
「skipped」就完事。
