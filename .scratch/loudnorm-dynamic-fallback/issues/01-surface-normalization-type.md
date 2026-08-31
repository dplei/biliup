# 01 — 把 `normalization_type` 与预计缺口打进日志

Status: open
Blocked by: —

## 背景

[`spec.md` 第 3 节](../spec.md)。信号已经在测量遍的 JSON 里，只是没被留下来。

## 做法

`crates/biliup-cli/src/server/common/audio_normalization.rs`：

1. `RawMeasurement` 增加 `normalization_type: Option<String>`，
   `LoudnessMeasurement` 增加对应字段（`Option<String>`，缺失时为 `None`——
   老 ffmpeg 或格式变动不能让整条测量失败）。
2. 在转码**之前**算出线性能否成立并一并记录：

   ```
   offset      = target_i - measured_i
   projected_tp = measured_tp + offset
   linear_ok   = projected_tp <= TRUE_PEAK
   ```

   `linear_ok == false` 时用 `info!`（不是 `warn!`——这是合理降级不是故障）打出
   `normalization_type`、`measured_tp`、`projected_tp`、`headroom = TRUE_PEAK - projected_tp`。
3. `audio_normalization="completed"` 那条日志补 `normalization_type` 字段，
   让「这一段到底打没打到目标」不必翻上一条。

## 验收标准

1. 单测：`normalization_type` 缺失时 `parse_loudnorm_measurement` 仍成功。
2. 单测：给定 `measured_i=-30.47 / measured_tp=-16.32 / target=-14`，
   判定 `linear_ok == false`，`projected_tp ≈ +0.15`。
3. 真机（`#[ignore]`）：用低占空比全幅脉冲素材（见 spec 2.1）跑完整链路，
   日志出现 `normalization_type="dynamic"`；用普通低响度素材跑，出现 `"linear"`。
4. 不改任何转码参数，产物字节应与改动前一致。
