# 02 检测器不再只匹配 ffmpeg 文案

兜存量。step 01 只保新录的片；已经落盘的坏片仍要靠上传前扫描拦住，而当前扫描存在
**确定性**漏检——不是概率问题。

## 漏检怎么发生的

[`ffmpeg_scan.rs`](../../../crates/biliup-cli/src/server/common/ffmpeg_scan.rs) 的
`stderr_indicates_anomaly` 判据是五个字符串：`Non-monotonic DTS`、
`non monotonically increasing dts`、`timestamp discontinuity`、`Invalid timestamp`、
`Application provided invalid`。

问题在于：源文件里的 32 位倒退**可能已经被解复用器按 wrap 展开成一个单调的巨大前跳**。
展开之后时间轴是单调的，muxer 无话可说，上面五个串一个都不出现 → 判 Clean → 坏片原样上传。
issue #13 评论里那次就是这条路径：预处理正常跑完、没有命中 `timestamp_repair`，站内照样拒稿。

`detect_anomaly` 还带 `-fflags +igndts`，这一点 #25 的 spec 已经分析过，会让它在复检 remux
产物时看到同样的东西。本步不动那个参数（属于 #25），但要意识到二者叠加。

## 要覆盖的形态

评论给了三条，按投入产出排序：

1. **相邻媒体包的异常大幅前跳**——本例形态，最该先做，也最便宜。
2. **原始 32 位 timestamp 的大幅回退**——需要区分 script tag / sequence header 与真实媒体
   tag，后两者的时间戳本来就可能是 0。
3. **container duration 与首尾媒体时间戳、录制墙钟时长严重不一致**——最粗但最难骗过去。

## 实现取向

**先别急着自己写 FLV tag 解析器。** ffmpeg 已有能直接吐数值的现成出口，优先看能不能用
`ffprobe -show_packets`（或 `-show_entries packet=pts,dts`）拿到包级时间戳序列，在 Rust 侧
只做数值判断。这比维护一个自研 demuxer 便宜一个数量级，也不会因为容器细节引入新的错判。
只有确认现成出口拿不到需要的东西时，才考虑解析 tag。

无论走哪条，判据阈值要和 step 01 的 `MAX_STEP` 讲同一套故事，别出现「录制侧认为是跳变、
上传侧认为正常」的夹缝。

## 保住已有行为

`parse_backward_ms` 的注释写明：解不出回退量时返回 `None`，调用方**必须当作「未知」保守
处理，不能当 0**。新判据接进来时这条约定不能被稀释——新增的是「再补一条检测」，不是
「换掉旧的」，两条应当是或的关系。

`86efea1` 把时间戳重写按回退量分档（避免 #25 的 x264 雪崩），新形态解出的「跳变量」要接进
同一套分档，别绕开它自成一路。

## 验证

- 单测：拿一段已知的包时间戳序列（单调大跳变 / 真回退 / 正常）直接喂判据函数，不跑 ffmpeg。
- 端到端：用 step 01 里 `recording_pilot.py` 注入产出的坏片做样本，断言本步之后判为异常
  且进入修复链路。**这个样本要留下来**，它是目前唯一一个能复现该漏检的真实形态。
