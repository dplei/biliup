# 01 — 把实际的标准化模式与产物响度打进日志

Status: resolved（改从转码遍的 summary 读，见「落地更正」；随 [#20](https://github.com/dplei/biliup/pull/20) 合入 `dev`）
Blocked by: —

## 背景

[`spec.md` 第 3 节](../spec.md)。ffmpeg 可能当场推翻 `linear=true` 退回动态模式，
产物响度到不了目标，而链路全绿、没有任何异常输出。

## 做法

`crates/biliup-cli/src/server/common/audio_normalization.rs`：

1. 新增 `TranscodeReport { normalization_type, output_i }`，由 `parse_transcode_summary`
   从转码遍的 `print_format=summary` 文本里取；任何一行取不到都退化成 `None`，
   **解析失败绝不能影响标准化的成败判断**。
2. `AudioFfmpegRunner::transcode` 的返回类型从 `AppResult<()>` 换成
   `AppResult<TranscodeReport>`。`SystemAudioFfmpeg` 本来就 `.output()` 捕获了 stderr，
   成功时直接丢掉——现在解析后返回，零额外开销。
3. `|output_i - target| > 1.0` 时用 `info!` 记一条 `loudness_target_missed`，
   带 `target_lufs`/`output_lufs`/`shortfall_db`/`input_lufs`/`measured_tp`/
   `normalization_type`。**用 `info!` 不是 `warn!`**：素材峰值放不下所需增益时，
   退回动态是正确的保守选择，不是故障。
4. `completed` 那条日志补 `normalization_type` 与 `output_lufs`。
5. **不改任何转码参数。**

## 落地更正

原计划是「把测量遍 JSON 里的 `normalization_type` 留下来，转码前预判」。实测推翻：

- **测量遍永远自报 `dynamic`**（没有 `measured_*` 输入，本来就做不了线性），
  余量充足的素材也一样，当预测器就是个恒为真的假信号；
- `af_loudnorm` 的线性判据不止真峰一条，`measured_LRA == 0` 也会退回动态——
  纯正弦素材 LRA 恰好是 0，最初的「安静素材」用例因此误判。自己复刻这个判据不可靠。

所以改成读转码遍的 summary：那是 ffmpeg 自己的判断，且同样零成本。

## 验收标准

1. 单测：`parse_transcode_summary` 能取出 linear/dynamic 与 `Output Integrated`；
   文本里什么都没有时返回 `TranscodeReport::default()` 而不是 panic。 ✅
2. 单测：产物响度没打到目标只记日志，**结束态仍是 `Normalized`**，产物内容不变。 ✅
3. 单测：summary 解析不出来同样不影响成败。 ✅
4. 真机（`#[ignore]`）：低占空比全幅脉冲素材 → `dynamic` 且偏差超过阈值；
   同样很轻但真峰低、LRA 非零的素材 → `linear` 且落在目标 1 dB 内。 ✅
   实测 `peaky: dynamic / -23.8`（目标 -14）、`varied: linear / -15.8`（目标 -16）。
5. 不改转码参数——filter 字符串一字未动。 ✅
