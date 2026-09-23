# 01 script tag 贴在已写出的最大值上

Status: ready-for-human（已合入 `dev`，dev 环境实录通过；待生产观察）

## 改哪里

[`httpflv.rs`](../../../crates/biliup/src/downloader/httpflv.rs) 的 `TimestampRebase::map` 开头：

- script tag 且已写出过任何 tag：`emit = max(high_water, script.last_emit + 1)`，不报
  `deviation`，不动 `pending` / `offset` / `adopt`。
- 还没写出过任何 tag（连接的第一个 tag）：照旧走 `None` 分支映射，行为不变。

副作用是 script tag 再也不能「确认换基准」。它本来就不该有这个能力：#62 的幻影确认正是
script 槽里一对 timestamp=0 的重发 tag 凑出来的。

## 验证

`split_prelude_script_tag_stays_with_the_segment`：三段各 40 分钟，每段开头喂 restamp 后的
onMetaData。旧实现实测 `script 1` vs `video 2400000`（与生产文件的 0.001 / 2400.011 一致），
改后 script 落在同段首个关键帧之前 100ms 以内。原有 10 条 rebase 测试全绿。

## dev 环境实录（2026-09-23）

抖音直播，`segment_time=00:01:00`，同一连接内切了 5 次，4 段通过体积过滤并上传。第 2 段起每段
都有那个单包 text 流（切段 prelude 的 onMetaData），它的首包现在与音视频对齐：

| 段 | text 首包 | 音频首包 | 视频首包 | 各流起点差 |
|---|---|---|---|---|
| 1 | — | 0.035 | 0.001 | 0.034 s |
| 2 | 59.982 | 60.003 | 59.994 | 0.021 s |
| 3 | 179.982 | 180.003 | 179.994 | 0.021 s |
| 4 | 239.977 | 239.994 | 239.994 | 0.017 s |

旧实现下第 2 段起 text 首包会停在 0.00x（比音视频早一整段）。4 段上传前检测都判干净、原片直传，
下播后一次投稿成功（`submit_state=ok_with_aid`、`submit_attempts=1`）。

## 待验（生产）

上线后新录的分段不再出现单包 text 流早于音视频的错位。
