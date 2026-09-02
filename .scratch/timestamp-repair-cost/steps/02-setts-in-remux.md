# 02 · remux copy 接入 setts，去掉无效的 igndts

Status: resolved
优先级：P0

## 为什么

`remux_copy` 现在用 `-fflags +genpts+igndts`。`igndts` 是丢弃 DTS 改用 PTS 推导，而这类
故障 PTS 与 DTS 一起回退，所以第 2 级修复对它结构上无效，每次必然掉到第 3 级 x264。

`setts` bitstream filter 只改 packet 时间戳、不碰 H.264/AAC payload，成本是一次顺序读写。

## 改哪里

[`crates/biliup-cli/src/server/common/timestamp_repair.rs`](../../../crates/biliup-cli/src/server/common/timestamp_repair.rs)
的 `SystemFfmpeg::remux_copy`：

- 去掉 `igndts`（`genpts` 是否保留一并确认：setts 接管时间戳后它可能多余）。
- 视频加 `-bsf:v setts=pts=max(PTS\,PREV_OUTPTS+1):dts=max(DTS\,PREV_OUTDTS+1)`。
- 音频现有 `-bsf:a aac_adtstoasc` 要改成**链式**：`aac_adtstoasc,setts=...`，
  不能写成第二个 `-bsf:a`（后者会覆盖前者）。

## 已验证的事实（本地实测，不必重新摸索）

- 变量名是 `PREV_OUTPTS` / `PREV_OUTDTS`。**没有 `PREV_OUTTS`**，写错会直接
  `Error initializing bitstream filter: setts`。
- 用 `pts=`/`dts=` 分开写，不要用省事的 `ts=`：`ts=` 把 PTS 和 DTS 设成同一个值，
  有 B 帧的源会被破坏。
- 在人工注入 2.6 秒回退的 FLV 上：修复后全片扫描命中数归零，20 秒素材耗时 0.02s。
- 镜像 ffmpeg 是 BtbN `ffmpeg-n8.1`，`setts` 从 5.0 起就有，不需要动 `Dockerfile`。

## 验收

- 现有单测全绿（`normalize_timestamps` 的状态机没变，只换了 remux 的实参）。
- 加一个 `#[ignore]` 的 `system_ffmpeg` 集成测试，沿用现有那个的写法：用 setts 注入回退
  造样本，跑 `remux_copy` + `detect_anomaly`，断言修复后无异常。
  注入用的表达式：`setts=ts=if(gt(N\,300)\,TS-2600\,TS)`。
- **本步不删 x264 fallback**，留到 03 用真实片段验完再决定。

## 注意

`max()` 的语义是把回退的那段时间**压掉**——回退点之后的内容整体相对提前。这对「重连
造成时间重叠」是正确的，对其他成因不一定。03 会用真实片段确认，本步不下结论。

## Answer

已实现。

- `-fflags` 从 `+genpts+igndts` 改为 `+genpts`：`igndts` 去掉（对本类故障结构上无效），
  `genpts` **保留**——实测三种 fflags 组合在有 setts 时产物完全一致，但缺 PTS 的源会让
  `max(PTS,...)` 拿到 NOPTS 去比大小，`genpts` 是那个边界的兜底，代价为零。
- 表达式提成两个常量 `SETTS_MONOTONIC` / `SETTS_MONOTONIC_AFTER_ADTSTOASC`，用 Rust 原始
  字符串写 `\,`——argv 直接交给 ffmpeg，中间没有 shell，所以不需要再转义一层。
- 新增 `#[ignore]` 集成测试 `system_ffmpeg_remux_repairs_backward_timestamps`：造 20 秒素材
  → 从第 300 个 packet 起注入 2.6 秒回退 → 断言 `detect_anomaly` 命中 → `remux_copy` →
  断言复检干净。
- `cargo test -p biliup-cli --lib`：345 passed；
  `--lib system_ffmpeg -- --ignored`：4 passed。

顺带一条 03 用得上的证据：测试素材的 x264 日志里有 `ref B L0/L1`，即**源确实带 B 帧**，
而这个测试是过的。这从侧面确认了 `pts=`/`dts=` 分开写的必要性——用省事的 `ts=` 会把两者
设成同一个值，这条测试就会挂。

**仍未回答**：`max()` 压掉回退时间在真实片段上的音画同步表现，以及两条流回退量的差值
量级。人工注入的样本证明不了这些，见 [03](./03-verify-and-drop-reencode.md)。x264 fallback
按计划**保留未删**。
