# 开录首段的 sequence header 不再建立时间基准（issue #116）

来源：https://github.com/dplei/biliup/issues/116

状态：已合并（#117，1.3.53），等待生产验收；验收清单见 issue #116 评论。验收通过前不归档。

## 问题

B 站 CDN 码流开头的 AAC sequence header 带陈旧时间戳（实测 2147178），AVC sequence
header 为 0，真实媒体帧从约 2963844 起、音视频同步。`TimestampRebase::map` 把它们当普通
tag 建了各流基准：视频首个关键帧被判为前跳压到 1ms，音频接在陈旧基准后落盘约 2147 秒，
首段音画相差约 2147 秒。生产上靠上传前的 `timestamp_repair` 整片 remux 兜底。

## 做法

- `TimestampRebase::map_config`：sequence header 只取 emit = 本流 `last_emit + 1`，没有
  就贴 `high_water`；更新 `last_emit` / `high_water`，不碰 `last_src` / `pending` /
  `offset` / `adopt`，即不参与基准判定。
- `is_sequence_header(tag_type, body)`：AAC（sound_format 10）packet_type 0；
  AVC/HEVC（codec 7 / 12）packet_type 0。
- `parse_flv` 写盘循环里 sequence header 走 `map_config`，其余照旧走 `map`。

## 验收

- 新测试 `leading_sequence_headers_do_not_set_the_time_base` 先红后绿：音视频首个媒体
  帧落盘时间差 < 100ms，每条流严格递增、相邻间隔 < 1000ms。
- `cargo test -p biliup` 全过，clippy 无新告警。
- 上线后观察：B 站房间开录首段不再触发上传前的时间戳修复。
