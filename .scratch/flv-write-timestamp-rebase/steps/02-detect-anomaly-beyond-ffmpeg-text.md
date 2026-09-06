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

---

## 落地记录（已完成）

只做了形态 1（相邻媒体包异常大幅前跳）。形态 2、3 没做，理由见末尾。

### 漏检已实测复现

拿 ffmpeg 造 8s 真实 FLV，把 4000ms 之后的全部 tag 时间戳整体前移 4.29e9 毫秒（单调，
一步跨 49.7 天）——这正是 32 位倒退被解复用器展开后的形状：

- `ffmpeg -loglevel verbose -fflags +igndts -i … -c copy -f null -` 命中五个字符串
  **0 次**；
- 同一个文件 `ffprobe` 读出来 video 首尾 `dts_time` 是 `0.000000` → `4294007.933000`。

**顺带否掉一条备选方案**：`ffprobe -show_entries format=duration` 在这个样本上返回 8.13s
（FLV 的 duration 取自 onMetaData，而 onMetaData 是照抄 CDN 的），所以形态 3 想走
「container duration 对不上」这条便宜路子在 FLV 上不成立，只能真读包。

### 实现

按本文的取向没写 FLV 解析器，走 `ffprobe -show_entries packet=stream_index,dts_time`：

- `ffmpeg_scan.rs` 新增 `PacketJumpScan`（纯逐行状态机，可单测）+ `run_packet_jump_scan`
  （流式消费 stdout，一场长录像几十万行，只维护每条流的上一个 dts，不留全量）。
- 判据：**按 stream_index 分别比对**相邻包，前跳 > `MAX_PACKET_STEP_MS` 即异常。跨流相减
  必然出负数，交错送达的 audio/video 不分开比就没法判。
- 阈值 30s，和录制侧 `REBASE_MAX_STEP_MS` 同一个数、同一套理由（段内真实空档由停顿看门狗
  兜住），不留「录制侧认为是跳变、上传侧认为正常」的夹缝。

`SystemFfmpeg::detect` 里两条判据是**或**的关系，旧的一条没动：

1. 文本判据命中 → 照旧 `Anomalous { max_backward_ms }`，**不跑** ffprobe（已经知道有问题，
   回退量也有了，没必要再读一遍整片）；
2. 文本判据判干净 → 再跑包扫描；命中则 `Anomalous { max_backward_ms: None }`。

### 三处判断，都写下理由

- **前跳为什么报 `None` 而不是自己造一个数**：前跳不是回退，`setts` 的 `max()` clamp 对它
  结构上无效（它本来就单调）。按 `parse_backward_ms` 的既有约定，`None` = 回退量未知 → 闸门
  保守判 `Unfixable`，走的是 `86efea1` 那套分档，没有自成一路。
- **ffprobe 跑不起来时跳过该判据，不升级成 `DetectFailed`**：这一层是加法。把它的环境故障
  变成降级，会让没装 ffprobe 的机器上每个文件都报 `detect_failed`，比漏掉这条判据更糟。
- **误判的代价有限**：`Unfixable` 和 `Clean` 上传的都是原片，差别只在事件与日志。所以阈值
  可以按语义取（和录制侧对齐），不必为「万一误判毁片」再加保险。这也意味着本步的收益本身
  就是**分类与可观测**——不再对着 #13 形态的坏片声称 `no_anomaly`。

顺带把闸门里那句「解析不出回退量」改成「回退量未知（措辞未识别，或异常形态本就不是回退）」，
否则这条路径的日志会把人引去查 ffmpeg 措辞。

### 代价

健康文件多付一遍解复用。实测两条扫描同量级（同一文件 `-c copy -f null -` 与
`ffprobe -show_entries packet=…` 耗时相当），所以 `detect()` 在判干净的文件上大约翻倍。
命中文本判据的文件不受影响。

### 验证

- 单测（纯函数，不跑 ffmpeg）：单调大跳变 / 交错分流 / 真回退不归本判据 / 阈值边界
  （30000 不算、30001 算）/ 认不出的行跳过。
- 真实 ffmpeg 端到端 `system_ffmpeg_detects_a_monotonous_giant_jump`（`#[ignore]`，与既有
  三个同批）：样本用 `jump_timestamps` **代码生成**而不是入库二进制片，改判据时调参数即可
  复现。断言 `Anomalous { max_backward_ms: None }` 且 `normalize_timestamps` 得 `Unfixable`、
  一次 remux 都不发起。
- `cargo test -p biliup-cli --lib` 378 passed；四个 `system_ffmpeg` 用 `--ignored` 全跑通——
  其中 `detect_clean_on_generated_file`（干净片不能误判）与 `repairs_a_cdn_replay_overlap`
  （回放重叠仍要修好）是本步的回归底线。

### 没做的两条

- **形态 2（原始 32 位 timestamp 的大幅回退）**：现在录制侧已经不产出这种文件（step 01），
  存量文件里这种形态**会**被文本判据命中——它是真回退，muxer 会报。所以它不是漏检，
  优先级低于形态 1。
- **形态 3（duration 与首尾时间戳/墙钟不一致）**：如上，FLV 的 `format.duration` 来自
  onMetaData，在本类样本上照样返回正常值，这条路子在 FLV 上不便宜也不可靠；真要做得
  拿录制侧的墙钟时长做对照，那需要把分段元数据传进检测器，属于另一件事。
- `-fflags +igndts` 没动（属于 #25）。
