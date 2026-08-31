# Spec：loudnorm 静默退回动态模式，响度打不到目标

Status: ready-for-human（机制已实测确认，策略待定）
来源：[`normalization-duration-metric`](../../.archive/normalization-duration-metric/verification.md)
的 dev 环境验收顺带观察到，非该 effort 引入
分支：`dev`

本文只定问题与取舍，实现步骤拆在 [`issues/`](./issues/) 下。

---

## 1. 现象

dev 环境一段真实录像，配置目标 **-14 LUFS**（`BASE_TARGET_LUFS = -16` + 配置的
`audio_normalization_offset_db = 2`），标准化结束态是 `completed`、判据全过，
但产物实测：

```
Integrated  -16.2 LUFS      （目标 -14，差 2.2 dB）
True Peak    -1.3 dBTP      （上限 TP=-1.5，还略微超了）
LRA          10.9 LU
```

原片 `input_i = -21.5`，需要 +7.5 dB，实际只上去 +5.3 dB。

**功能没报错、日志没异常、判据全过**——只是没做到它宣称的事。

## 2. 根因：`linear=true` 会被 ffmpeg 静默推翻

转码那一遍传的是双遍模式的完整参数，含 `linear=true`：

```
loudnorm=I=<target>:LRA=11:TP=-1.5:measured_I=..:measured_LRA=..:measured_TP=..
        :measured_thresh=..:offset=..:linear=true:print_format=summary
```

但 `af_loudnorm` 只在**真峰放得下**时才真的走线性：所需增益
`offset = target_I − measured_I` 加到 `measured_TP` 上如果超过 `TP`，
它就**悄悄把 `linear` 关掉退回动态模式**——不报错、不警告，退出码 0。

动态模式是逐帧的增益/限幅，天生不保证整段积分响度落到目标，于是就差在那里。

### 2.1 本机实测复现

造一段「整体很轻但有满幅瞬态」的素材（低占空比全幅脉冲 + 其余时间 -34 dB），
跑与生产**完全相同**的两遍流程：

```
测量遍 JSON：input_i=-30.47  input_tp=-16.32  input_lra=17.30
             normalization_type="dynamic"        ← 测量遍就已经知道了

转码遍 summary（传了 linear=true）：
  Normalization Type:   Dynamic                   ← 被推翻
  Output Integrated:   -23.8 LUFS

复测产物：input_i = -24.26 LUFS，目标 -14 → 差 10.3 dB
```

代入判据：`measured_tp + offset = -16.32 + 16.47 = +0.15 > TP(-1.5)` → 关掉线性。

**差距是无上限的**：本机这段差 10.3 dB，dev 那段差 2.2 dB，取决于素材的峰值余量。

### 2.2 这不一定是 bug

要在 -21.5 → -14 之间线性抬 7.5 dB，而素材本身峰值已经很满，线性做法只能削顶。
loudnorm 退回动态是**正确的保守选择**。

**真正的缺陷是它是静默的**——与 [#16](https://github.com/dplei/biliup/issues/16) 同一类：
一个降级路径没有任何可观测输出，于是没人知道功能有没有真的生效。

## 3. 关键事实：这个信号我们已经拿到了，只是扔掉了

测量遍的 JSON 里**本来就有 `normalization_type`**（见 2.1）。
`parse_loudnorm_measurement` 解析了同一段 JSON，但 `LoudnessMeasurement` 只留了
`input_i / input_lra / input_tp / input_thresh / target_offset` 五个字段，
**`normalization_type` 被丢掉了**。

也就是说「这一段会不会打到目标」在**转码开始之前**就能知道，零额外成本。
→ [`issues/01`](./issues/01-surface-normalization-type.md)

## 4. 待定的策略

先把 01 落地拿到真实分布，再决定下面哪条（或都不做）。**不要在没有数据时先调参数。**

| 选项 | 代价 / 风险 |
| --- | --- |
| A. 只观测，不改行为 | 零风险。动态模式本来就是合理降级，也许真实素材里根本不常发生 |
| B. 放宽 `TP`（-1.5 → -1.0 / -0.5） | 只多出 0.5～1 dB 余量，救不了差 10 dB 的素材；且 B 站转码后可能削顶 |
| C. 降低目标（减小 `offset_db`） | 用户配的 `offset_db=2` 把目标从 -16 抬到 -14，正是它让所需增益变大、更容易触发退回。这是配置项，属于「告知用户」而不是「代码改行为」 |
| D. 接受动态模式但补一次限幅后的二次增益 | 三遍编码，与「省一次有损重编码」的既有取向冲突 |

倾向 **A + C 的告知**：先让它可见，并在文档里说明 `offset_db` 调大会更容易触发。

## 5. 验收总纲

1. 日志能回答「这一段走的是线性还是动态」以及「预计差多少」。
2. 真实录制环境采一段时间，得到动态模式的实际发生比例。
3. 在拿到比例之前不动 `TP`、不动默认目标。
