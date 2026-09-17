# 02 上传侧：前跳判 `Unfixable` 之后不该直传原片

Status: resolved（主人拍板：修，修不好的不上传）

不是根因，是第二道防线的策略缺口。step 01 上线后新片不会再产生这种前跳，本步只影响
存量坏片与 01 漏掉的未知形态。

## 现状

`detect_packet_jump`（#13 step 02）已能检出相邻包前跳 > 30 s，返回
`Anomalous { max_backward_ms: None }`；`normalize_timestamps` 对 `None` 判 `Unfixable`，
`upload.rs` 的处置是**直传原片 + 告警**。#62 的文件就是这样被送去 B 站的。

## 落地

采用 A，并把 `Unfixable` 从「直传原片 + 告警」改成「报错不上传 + 告警，本地文件保留」，
三处消费点（首次上传 / 补传 / 手动补投）的直传分支删除，lifecycle 行按常规 10 分钟退避重试。
表达式实际写法（`delta_setts`）：

```
STEP = if(gt(DTS-PREV_INDTS,30000), 1, max(DTS-PREV_INDTS,1))
dts  = if(lt(PREV_OUTDTS,-1e15), DTS, PREV_OUTDTS+STEP)
pts  = if(lt(PREV_OUTDTS,-1e15), PTS, PREV_OUTDTS+STEP+PTS-DTS)
```

`clip(Δ,1,30000)` 是错的——它把 30 s 以上的前跳压成 30 s 而不是 1 ms，本机实测过一次才改成
上面的分段写法。首个 packet 的 `PREV_OUTDTS` 是 NOPTS（≈ −9.2e18），用 `lt(…,-1e15)` 识别。
ffmpeg 9 实测：干净文件产物与旧表达式逐包一致；前跳 4000 s 与回退 3 s 的样本都回到 8.01 s
且过现有 `detect`。`system_ffmpeg_*` 四条 ignored 测试全绿（重置样本产物 38.3 s = 25 s +
关键帧回退后的 13.3 s 尾段）。

已知上限（代码里有 `ponytail:` 注记）：文件内逐帧交替的垃圾 tag 会按 tag 数压缩时间轴，
录制侧已不会产生这种文件；`Unfixable` 行每 10 分钟重跑一遍全片扫描，真修不好的靠缺失补传页
删除。

## 当时的两个方向（记录）

**A. 检出即修，用增量模型而不是夹取**

现有修法 `setts=dts=max(DTS, PREV_OUTDTS+1)` 只对回退有效（且有 10 s 闸门，见
`MAX_REPAIRABLE_BACKWARD_MS`）。换成按流累加增量、把超步长的增量压成一帧：

```
dts = PREV_OUTDTS + clip(DTS − PREV_INDTS, 1, 30000)
pts = 上式 + (PTS − DTS)
```

回退（增量为负）与前跳（增量 > 30 s）都被压成 +1 ms，之后按真实增量走——**没有帧风暴**，
10 s 闸门可以一并去掉。代价：A/V 各自压缩，相对偏移在跳变处改变 ≤ 一帧。需要验证
`setts` 的 `PREV_INDTS` 在生产 ffmpeg 版本可用（≥ 5.1），以及 `-c copy` 下 pts 表达式对 B 帧
（composition time）的处理。**推荐**：修法便宜（`-c copy`，秒级），修完直接过第二次 `detect`。

**B. 检出即拦，不上传**

最简单，但磁盘预算有限（issue 已说明「延迟删除」不可行），拦下来的文件要么占盘要么删，
删了就等于丢内容。只在 A 做不出来时兜底。

## 待办（无论选哪个）

- `scripts/timestamp_shift.py` / `segment-recover` skill 只算回退的平移量，前跳形态要补一条
  「减去跳变量」的分支，本机应急用。
