# 01 script tag 贴在已写出的最大值上

Status: implemented（待 PR）

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

## 待验

需要真实录制：dev 环境开一场直播、分段时长设 1–2 分钟，录 3 段以上，ffprobe 每段各流
`start_time` 相差不超过 1 s。
