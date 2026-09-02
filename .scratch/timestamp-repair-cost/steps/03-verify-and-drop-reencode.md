# 03 · 真实片段验证 setts 的语义，并决定 x264 fallback 的去留

Status: ready-for-agent
Blocked by: 02
优先级：P0（这一步决定整个 effort 的成败）

## 为什么

02 只证明了「扫描不再报异常」。真正要回答的是**修复后的片子还能不能看**：`max()` 把
回退的 2.6 秒压掉，回退点之后的内容整体相对提前，音视频两条流各自独立 clamp。人工注入
的样本证明不了这些，必须用真实的回退片段。

## 要拿到的样本

一个真实存在 DTS 回退的直播分段。优先级：

1. 生产上还留着的那个 `Unfixable` 片段（`upload.rs` 的 `Unfixable` 分支会保留本地文件）。
2. 拿不到就在 dev 环境实录：本地起 `biliup server` + `pnpm dev`、本地 sqlite、只自己可见
   的投稿模板，录一段中途断流重连的直播。**不要因为「需要真实录制」就把这项推给上线观察。**

## 要验的项

对同一个源片段，A（现状 x264 重编码产物）/ B（setts remux 产物）逐项对照：

| 项 | 判据 |
| --- | --- |
| 全片扫描 | B 的异常命中数为 0 |
| 内容跨度 | B 相对源缩短量 ≈ 回退总量，且与 A 一致；用 `content_span` 口径，不看 `format.duration` |
| A/V 同步 | 回退点前后各抽几处人工看/听；重点是回退点之后是否整体错位 |
| 两条流回退量差 | 从扫描日志里读 stream 0 / stream 1 的回退毫秒差。issue 那次是 2599 vs 2624（差 25ms，可忽略）；若真实片段差值到百毫秒量级，`max()` 独立 clamp 的方案要重新设计 |
| 耗时 | B 应是顺序 IO 量级（1 GB 几十秒），不是分钟级 |
| 画质 | B 是 `-c copy`，payload 逐字节一致，只需确认没有意外转码 |

## 出口

- **B 全项通过** → 从 `timestamp_repair.rs` 删掉 `reencode` 与 `FfmpegRunner::reencode`，
  `normalize_timestamps` 变成两级（检测 → setts remux → 复检 → `Unfixable`），
  相应删掉 01 加的重编码超时和 `repaired_by_reencode_when_copy_insufficient` 一类单测。
  顺手把 04 标为 wontfix：x264 没了，CPU 争抢的前提就没了。
- **B 在某项上不合格** → 保留 x264 作为第 3 级兜底，01 的超时留着，04 转为必做。
  把不合格的项和数据写进本文件的 `## Answer`。

## 收尾

结论写回本文件的 `## Answer`，并同步更新 [`../spec.md`](../spec.md) 的「方案取舍」——
特别是 spec 里那条「语义警告」，验完要么划掉要么升级成约束。

## 注意

日志与结论写进公开仓库前按 `CLAUDE.md` 脱敏：去掉 BV 号/aid、房间地址、cookie 文件名、
镜像 tag 与生产统计，保留 `trigger=` / `outcome=` 这类不带标识符的证据链。
