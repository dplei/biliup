# 06 — 多路并发峰值验收

Status: ready-for-human
Blocked by: 01, 02, 03, 04, 05

## 背景

[`spec.md` 第 9 节](../spec.md)的验收总纲需要一次端到端确认。前五个 ticket 的单测只能证明
各自的局部行为，「多路并发下额外占用不超过一份分段」是个跨管道的整体性质，必须实测。

## 做法

本机起多条并发管道（用短分段配置压缩单轮时长，不必等真实的 30 分钟分段），
用 [`scripts/normalization-disk-sample.py`](../../../scripts/normalization-disk-sample.py)
在整个过程中采样中间件的数量、字节总和与目录总占用：

```bash
# 实验组：默认配置，脚本默认断言「任何时刻最多一份中间件」
python3 scripts/normalization-disk-sample.py <录像目录> --csv in-place.csv

# 对照组：audio_normalization_keep_original: true，只采样不判定
python3 scripts/normalization-disk-sample.py <录像目录> --csv keep-original.csv --max-parts 99
```

实验组退出码为 0 即验收标准 1 的前半；对照组用来确认测量方法本身有效——它应当能采到多份并存。

## 验收标准

1. **峰值**：实验组的 `.part` 文件数在任何采样点都 ≤ 1，字节总和 ≤ 一份分段大小；
   对照组应能观察到多份并存（证明测量方法本身有效，不是采样没抓到）。
2. **流水线未退化**：实验组的端到端总时长与对照组相当（差异在噪声范围内）。
   这一条是本方案相对 #8 候选方案 1 的全部意义所在，必须实测而不是推断。
3. **响度不变**：同一段素材在改动前后，`input_i` 与产物整段 LUFS 一致。
4. **准入降级**：人为把可用空间压到阈值以下，确认 `DiskAdmissionDenied`，录制与上传照常。
5. **硬水位取消**：转码途中压低可用空间，确认 `DiskPressureAborted`、`.part` 不残留、
   原片完好、上传照常传原片。
6. **补传不重编码**：让一段已标准化的分段走补传，日志出现
   `audio_normalization = "skipped"` 且 `reason = "already_normalized"`，无 ffmpeg 调用。
7. **回滚安全**：`keep_original: true` 时全部行为退回当前实现。

## 产出

把结论写进本目录的 `verification.md`（对齐
[`recording-expiry/`](../../recording-expiry/) 的做法），含采样曲线的要点与实测数字。
全部通过、且改动已落到 `dev` 之后，整个目录按
[`docs/agents/issue-tracker.md`](../../../docs/agents/issue-tracker.md) 归档到 `.archive/`。
