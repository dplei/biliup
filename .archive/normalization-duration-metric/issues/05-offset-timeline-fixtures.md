# 05 — 非零时间轴的回归覆盖

Status: resolved（本机已验红→绿；dev 环境真实录制 6/6 `completed`）
Blocked by: 01, 03

## 背景

[`spec.md` 2.3](../spec.md)。既有覆盖漏掉这个 bug 不是因为用例少，而是因为
**测试素材与生产不同构**：本机素材时间轴从 0 开始，且 `flvenc` 回填了正确的
`onMetaData.duration`。两处差异各自都足以掩盖问题。

现有 `duration_drift` 单测用 `SOURCE_DURATION - 10.0` 构造，两侧 `start_time` 隐含为 0，
永远覆盖不到。

## 做法

**单测层**：`FakeRunner` 增加 `source_start_time`，新增一例「原片 `start_time` 非零、
产物归零、两者真实跨度相同」→ 必须判 `completed`。这是本次回归的最小护栏。

**真机层**（`#[ignore]`，需要本地 ffmpeg）：用与生产同构的方式造素材——

```
-output_ts_offset 3600 -flvflags no_duration_filesize
```

两个开关缺一不可：前者制造非零 `start_time`，后者阻止 trailer 回填 `onMetaData.duration`，
逼 `flvdec` 走「读末尾 tag 时间戳」那条路。造完先 `ffprobe` 断言
`format.duration ≈ 3605.9` 且 `format.start_time ≈ 3600`，**确认素材本身复现了现象**
再跑链路——否则这个用例会在素材退化成正常 FLV 时静默失效。

然后跑完整 `normalize_for_upload`，断言结束态 `completed`、原片被替换、产物响度达标。

## 验收标准

1. 单测层新用例在 ticket 01 之前必须失败、之后通过（写完先 `git stash` 验一遍红）。
2. 真机用例含「素材自检」断言，素材不符合预期时用例失败而不是通过。
3. 既有 FLV/MP4 真机用例继续通过。
