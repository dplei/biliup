# 各流起点错位：流内判据看不见，投稿被 21588 拒绝

来源：[dplei/biliup#75](https://github.com/dplei/biliup/issues/75)。发起时版本 1.3.38。

关联：#62（`.scratch/rebase-sibling-adoption/`，script 槽只在切段时收到 onMetaData 的
事实在那里已经写过）、#13（`TimestampRebase` 的来源）。

> 本目录用 `steps/` 存放实施步骤。**这些不是 GitHub issue**，编号只在本目录内有意义。

## 一句话

一场录播里有两类分段被 B 站以「时间戳跳变」拒稿，共同点是**每条流各自单调，只是起点彼此
错开**：上传侧所有判据都是流内的（文本判据、相邻包前跳），所以全部判干净。

| 形态 | 来源 | 错开量 |
|---|---|---|
| 修复产物音频晚视频 | 上传侧 `delta_setts` 首包原样放行 | 跳变全长（实测 1616 s） |
| 单包 script 数据流早音视频 | 录制侧 `TimestampRebase` 把切段 onMetaData 写成上一个 +1ms | 一整段（实测 2400 s） |

## 根因

### R1. `delta_setts` 各流首包原样放行

setts 按流累加增量，首包（`PREV_OUTDTS` 为 NOPTS）直接输出原始 DTS。形态「视频首包在跳变前、
第二包起跳；音频首包就在跳变后」时，视频的跳变被压成 1ms、从 0 开始，音频带着跳变后的原始
时间戳开始。两条流各自单调，复检判干净。

### R2. script 槽走了它不可能通过的连续性判据

`TimestampRebase::map` 对 audio / video / script 三个槽用同一套「相邻 30s 内」判据。script 槽
在一个连接里只在切段时收到 restamp 的 onMetaData，相邻两个隔着整段时长（40 分钟），**每次**都
判成偏离，走「首次偏离」分支写成 `last_emit + 1`。结果：同一连接里第 2 段起，每段文件开头的
script 包都停在第 1 段起点附近。重连后的第一段不受影响（首个 tag 走 `None` 分支），所以同一
场里有的段错位、有的不错位。

ffprobe 把它认成 `codec_type=subtitle, codec_name=text`、1 个包的流。

### R3. 检测只有流内判据

`detect` = `-c copy -f null` 文本判据 + ffprobe 相邻包前跳，都是每条流各自比。「起点错位」在
流内不留任何痕迹。

## 方案

| 步骤 | 改哪里 | 做什么 |
|---|---|---|
| [01](./steps/01-script-tag-follows-high-water.md) | `httpflv.rs` `TimestampRebase::map` | script tag 不走连续性判据，贴在已写出的最大值上 |
| [02](./steps/02-detect-and-align-start-skew.md) | `ffmpeg_scan.rs`、`timestamp_repair.rs` | 检测加跨流起点差；setts 首包对齐；remux 只保留一路视频一路音频 |

阈值一律沿用 `MAX_PACKET_STEP_MS`（30 s）：一条流比最早的流晚 30 s 以上才有首包，和流内前跳
30 s 是同一件事。

## 不做

- **小于 30 s 的音视频同步断档**（同场另两段 25.9 s / 26.3 s）：它是和 R2 的错位一起替换后才
  投稿成功的，无法确认它单独会不会被拒。阈值同时约束录制侧重基，缺证据时不动，见 #75 评论。
- **容器时长 vs 最长流时长**判据：起点差已经覆盖了本题的两种形态。
- **重试上限**：投稿被确定性拒绝后仍无限退避重投、分段上传失败每 10 分钟重试一次，是另一个
  交付边界，另开 issue。
