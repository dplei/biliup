# 06 — 多路并发峰值验收

Status: 部分完成 / 待真实录制验收（2026-08-30）
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

## 本机落地结果（2026-08-30）

验收标准 1 的核心断言已在 `dev` 上用真实 ffmpeg 验过，固化为 `#[ignore]` 测试：

```bash
cargo test -p biliup-cli concurrent_normalization -- --ignored --nocapture
```

四段真实素材并发跑完整链路，**并发期间同时存在的中间件峰值为 1 份**，跑完无 `.part` 残留、
活动产物表清空、四段原片全部被就地替换。`NORMALIZE_SLOTS` 与就地替换合起来确实把额外占用
锁在一份之内。

顺带测到的产物/原片倍率：

| 原片音频 | 倍率 |
| --- | --- |
| 64k / 44.1 kHz | 1.89 |
| 128k / 48 kHz | 1.36 |
| 192k / 48 kHz | 1.36 |
| 320k / 48 kHz | 1.36 |

⚠️ **这组数字不能外推到真实录像。** 合成素材的视频只有 ~33 kbps（静态 testsrc），音频占了
大头；真实直播录像是 Mbps 级视频 + 192k 音频，音频占比不到 2%，音频怎么重编都动不了总大小
几个百分点。192k→192k 仍有 1.36，是因为 `sine` 波原本压得极好，经 loudnorm 处理后压缩效率
下降——同样是合成素材才有的现象。

能读出的只有趋势：**音频占比越高，`OUTPUT_SIZE_FACTOR = 1.1` 越会低估**。对真实录像它是
安全的；低码率视频或纯音频源会低估，后果是准入放行、转码中途撞硬水位中止，白跑一遍但不会
写爆磁盘。是否改成「按 probe 出的音频码率推算」见下方待决项。

## 仍需真实环境的部分

- 验收标准 2（流水线未退化）：要有真实录制 + 真实上传才能比端到端时长，本机测不了。
- 验收标准 4、5（水位降级）：需要人为压低可用空间，在真实盘上做。
- 验收标准 6（补传不重编码）：需要一段真实失败后进补传的分段。
- 长跑观察：多路并发录制下用
  [`scripts/normalization-disk-sample.py`](../../../scripts/normalization-disk-sample.py)
  采样一整场。
