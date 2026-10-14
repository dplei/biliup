# timestamp_shift.py 补前跳、修 B 帧误判

来源：https://github.com/dplei/biliup/issues/109

## 根因

1. 只检测回退，直播源断流造成的前跳（相邻包向前跳几十秒）完全没覆盖；dts 回退扫描对前跳 0 命中。
2. `find_drops` 读 `packet=pts` 按解码序判回退，含 B 帧的视频流 PTS 本来就不单调 → 大量假回退。
3. 默认输出 mp4，edit list 让解码少出帧；源是 flv 时应保持 flv。

## 方案

- 判定一律用 DTS（缺失时退回 PTS）。
- 回退模式：逻辑不变，只把输入换成 DTS。
- 前跳模式：每条流找「DTS 间隔 > 1 s」的空洞；按时间重叠把各流空洞聚成一个断流点，
  **每条流都在该处有空洞**才处理（只有一条流跳说明不是断流，收回去会音画漂移，拒绝）。
  每个断流点一个增量偏移，音视频共用：
  `O = min_流(min(后段最小DTS − 前段最大DTS, 后段最小PTS − 前段最大PTS)) − max_流(典型间隔)`，
  PTS 那一项就是 B 帧 composition 约束（断流截断的 GOP，前段最大 PTS 可能比 DTS 大一两帧）。
  分段条件按 DTS 落在哪个断流点之后，`setts=pts=PTS-E:dts=DTS-E`，不用 `ts=`。
- 同一文件既有回退又有前跳：直说，不硬套。
- ffmpeg 命令加 `-copyts`：ts 这类允许不连续的格式，ffmpeg 会在 bsf 之前自己纠正回退/前跳，
  再平移一次就成了双倍（实现中用 ts 拼接样本发现，旧脚本同样受影响）。
- 产物默认与源同扩展名；`aac_adtstoasc` / `+faststart` 只在 mp4/mov 输出时加。
- 新增 `scripts/test_timestamp_shift.py`：合成带 B 帧和两个空洞的小样本，断言复检四项。

## 不在范围

- 服务端 `timestamp_repair.rs` 的前跳处理（rebase-sibling-adoption 那边记过待办）。
- 回退与前跳混合的文件。

## 进度

- [x] 01 实现与自检（`steps/01-forward-jump.md`）——PR 待合并
