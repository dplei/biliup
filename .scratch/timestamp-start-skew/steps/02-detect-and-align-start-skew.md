# 02 检测跨流起点差，修复时对齐起点

Status: implemented（待 PR）

## 改哪里

### 检测

[`ffmpeg_scan.rs`](../../../crates/biliup-cli/src/server/common/ffmpeg_scan.rs) 的
`PacketJumpScan` 顺手记下各流首包 dts，`start_skew_ms()` = 最晚首包 − 最早首包。

[`timestamp_repair.rs`](../../../crates/biliup-cli/src/server/common/timestamp_repair.rs) 的
`SystemFfmpeg::detect`：包扫描之后，前跳没越线再看起点差，超过 `MAX_PACKET_STEP_MS` 判
`Anomalous { max_backward_ms: None }`。原片和修复产物的复检都走这里，所以修出来仍错位的
产物现在会被判 `Unfixable` 扣住，不会再上传。

不另起一遍 ffprobe：起点差和前跳用同一遍包扫描。

### 修复

- `delta_setts` 首包：`if(gt(DTS,30000), 0, DTS)`。ffmpeg 在 bsf 之前已经减掉文件起点
  （所有流里最早的首包），首包 DTS 超过 30 s 就是「本流起点比最早的流晚了一次跳变」。
  没超过的原样保留，正常文件的音画起点差不动。
- `remux_copy` 加 `-map 0:v:0? -map 0:a:0?`：script 数据流不进产物。实测 mp4 的默认选流
  已经不选它，显式写出来是为了不依赖默认选流在不同 ffmpeg 版本上的行为。

## 验证

- 单测 `start_skew_spans_the_earliest_and_latest_stream`：两种生产形态的 ffprobe 行。
- 系统测试（`--include-ignored`，需本地 ffmpeg）：
  - `system_ffmpeg_aligns_a_stream_that_starts_after_the_jump`：一条流只有首包在跳变前。
    旧 setts 下复检命中起点差 → `Unfixable`；改后两条流起点相差 < 0.1 s、时长 8 s。
  - `system_ffmpeg_drops_a_stale_data_packet`：插一个 timestamp=0 的 onTextData，音视频整体
    后移 2400 s。旧 `detect` 判 `Clean`；改后判异常并修好，产物只剩两条流。
- `cargo test -p biliup-cli -p biliup` 全绿（`explicit_app_21566_uses_regular_submit_backoff`
  偶发失败，与本改动无关：jitter 上界与 `before` 取值的时序竞争，重试上限那一轮一并修）。

## 待验

生产上线后：`timestamp_repair` 出现 `reason_code=repaired` 的分段，其产物各流起点差 < 1 s；
含单包 script 流错位的原片（R2 修好之前录的存量）被判异常并修复，投稿不再因 21588 被拒。
