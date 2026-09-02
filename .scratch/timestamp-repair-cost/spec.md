# 时间戳修复的成本失控：x264 fallback 反复超时与预处理雪崩

来源：[dplei/biliup#25](https://github.com/dplei/biliup/issues/25)

关联：#13（录制写入侧时间戳回绕，治本方向）、#4（网络失败后丢弃已完成 loudnorm 产物）

> 本目录用 `steps/` 存放实施步骤。**这些不是 GitHub issue**，编号只在本目录内有意义，
> 不要和 `dplei/biliup` 的 issue 编号混用或互相引用编号。

## 一句话

一个只有约 2.6 秒 DTS 回退的 40 分钟 1080p60 分段，走完了「扫描 → remux copy → 整段
x264 重编码」的修复链路，重编码无法在 preprocessing watchdog 内完成，被反复终止并从零
重试，同时和 loudnorm 抢 2 vCPU，把正常分段的响度标准化拖慢 2.7–3 倍。

## 根因（已对代码核对）

### R1. remux copy 那一步在结构上不可能修好这类回退

[`timestamp_repair.rs`](../../crates/biliup-cli/src/server/common/timestamp_repair.rs) 的
`remux_copy` 用 `-fflags +genpts+igndts`。`igndts` 的语义是**丢弃 DTS、改用 PTS 推导**，
而本例 PTS 与 DTS 一起回退，所以这一步对该故障形态无效——不是「碰运气没修好」，是必然
落到第 3 级的 x264 重编码。

`detect_anomaly` 同样带 `+igndts`，因此它在「复检 remux 产物」时看到的仍是同一个回退。

### R2. 修复手段和故障量级不成比例

时间戳属于容器/packet 元数据，283 个异常 packet 却触发解码 + 重编码整段 40 分钟
1080p60 的 H.264 payload。这是链路里唯一真正昂贵的一步。

### R3. watchdog 的输入变量选错了

[`attempt_lease.rs`](../../crates/biliup-cli/src/server/common/attempt_lease.rs) 的
`preprocess_deadline` = `PREPROCESS_BASE`(10 min) + `PREPROCESS_PER_GIB`(10 min) × GiB。
1.10 GB → 30 min，与生产观测的 `deadline_secs=1800` 完全吻合。

但 x264 的成本由**内容时长 × 分辨率 × 帧率**决定，不由字节数决定。文件大小对
loudnorm（顺序解码转码）是合适的代理变量，对像素重编码不是。

### R4. 超时的处理方式是「杀掉整个 attempt」而不是「预处理降级」

watchdog 触发即 drop 上传 future，`kill_on_drop` 杀 ffmpeg，扫描结论与 remux 中间产物
全部丢弃。恢复调度重新 claim 后从第一步重来，三次 attempt 走了完全相同的路径。

需要修正 issue 正文的一处措辞：**退避是有的**——
[`missing_segment.rs`](../../crates/biliup-cli/src/server/common/missing_segment.rs)
的 `retry_delay_for_attempt` 给出 10m / 30m / 1h / 2h / 6h，生产观测到的 10 分钟间隔正是
attempt 1 的退避。所以这不是热循环。真正缺的是**「这条路已经被证明走不通」的记忆**：
`attempts` 无上限，最终会以 6h 为周期永远重复烧 30 分钟 CPU。

### R5. 重型 ffmpeg 之间没有全局互斥

`NORMALIZE_SLOTS`（capacity 1）只在 `normalize_for_upload` 内部持有；
`normalize_timestamps` 从 [`upload.rs`](../../crates/biliup-cli/src/server/common/upload.rs)
独立调用，不持有任何 permit。两个 CPU 密集型 ffmpeg 因此可以在 2 vCPU 上同时运行。

## 方案取舍

### 采纳：用 `setts` bitstream filter 做时间戳重写，取代 x264

issue 正文暗示要自己写 FLV tag 时间戳重写。不需要——ffmpeg 5.0+ 自带 `setts` bsf，
镜像里的 n8.1（`Dockerfile` 拉 BtbN `ffmpeg-n8.1`）具备。

本地已实测（造一个 DTS 回退 2.6 秒的 FLV）：

```text
原始 clean 素材        扫描命中 0
注入回退后             扫描命中 >0
setts 修复后复检       扫描命中 0
耗时                   20 秒素材 0.02s（纯顺序 IO）
payload                未解码、未重编码
```

生效的表达式（注意变量名是 `PREV_OUTPTS` / `PREV_OUTDTS`，**没有** `PREV_OUTTS`，
写成后者会 `Error initializing bitstream filter: setts`）：

```text
setts=pts=max(PTS\,PREV_OUTPTS+1):dts=max(DTS\,PREV_OUTDTS+1)
```

选 `pts=/dts=` 分开写而不是省事的 `ts=`：`ts=` 会把 PTS 与 DTS 设成同一个值，有 B 帧的
源会被破坏。直播 FLV 通常没有 B 帧，但不值得赌。

落点是**改现有 remux 那一步的几行**，不是新增一个流水线阶段：去掉无效的 `igndts`，
加上 `setts`（`-bsf:a` 需要链式写成 `aac_adtstoasc,setts=...`）。

**语义前提（03 已实测，结论比原本的担心更尖锐）**：`max()` 只在「回退量 ≪ 剩余内容
时长」时正确。

- 生产的实际形态是 **CDN 回放重叠内容**，clamp 把重复段压掉正是正确语义：总时长不变、
  内容不丢、A/V **零漂移**，只在回退点留下约 0.1 秒的快进抖动。
- 但**时间戳重置／回绕**（#13 的形态）下，clamp 会把回退点之后的全部真实内容压进几百
  毫秒，而复检看不出来——它只看单调性。这条路径会静默上传坏片并删掉原片。

所以 setts 必须配一道产出合理性校验才能上生产，见 [`steps/05`](./steps/05-guard-against-collapsed-output.md)。
两条流独立 clamp 不引入漂移（`max()` 一旦追上就完全透明），issue 里那个 25ms 差值无害。

### 采纳：给重编码一个自带超时，超时按 `Unfixable` 降级

不需要新表、不需要 fingerprint 字段。问题的实质是「重编码超时」被表达成了「attempt 失败」，
而它本该是「预处理修不好 → 降级直传原片 + 告警」——这个语义 `RepairOutcome::Unfixable`
已经有了，`upload.rs` 里已经在保留本地文件并发 webhook 告警。

给 `reencode` 一个明显短于 `preprocess_deadline` 的内部超时即可：一次 attempt 必然得到
确定结果，重复重跑自然消失。这一条与 setts 无耦合，可以先合，作为止血。

### 条件采纳：重型 ffmpeg 共享 permit

把 `NORMALIZE_SLOTS` 提成公共 permit、时间戳修复也持有，改动约十行。但**优先级取决于
上一条**：x264 从链路里消失后，剩下的 remux/scan 是 IO 密集的，CPU 争抢自己就没了。
只有最终决定保留重编码兜底路径时才有必要。

## 明确不做

- **进度感知 watchdog（`ffmpeg -progress` + `out_time`）**：这是清单里改动最大的一条，
  要给 `ffmpeg_scan` 的管线加一路 fd、解析器和无进度判定。而 setts 落地后 preprocessing
  里最贵的只剩 loudnorm，它的成本确实与文件大小线性相关，现有 size-derived deadline
  本来就适配。只有决定长期保留 x264 兜底时才重新评估。
- **更短切片（10–15 分钟）**：同意 issue 自己的判断——线性降低单片延迟，但总 CPU 工作量
  不变，是缓解不是治本。
- **loudnorm 单遍 `fast_dynamic`（issue 的 D 节）**：独立优化，与本次上传失败无因果关系。
  要做就单开一条线，不混进这个 effort。
- **写入侧治本**：留在 #13。它不解决存量坏文件。

## 期望终局

- 少量 DTS 回退由 `-c copy` + `setts` 在一次顺序读写内修好，不重编码画面。
- 任何一次 attempt 的预处理都在自己的超时内得到确定结论：修好、降级直传、或
  `Unfixable` 告警——不再由 watchdog 杀 attempt。
- 同一输入不再从零重复执行确定会超时的重编码。
- 时间戳修复不再把并发的响度标准化拖慢 2–3 倍。

## 实施步骤

见 [`steps/`](./steps/)：

| # | 步骤 | 优先级 | 阻塞于 | 状态 |
| --- | --- | --- | --- | --- |
| 01 | [重编码自带超时，超时降级直传](./steps/01-reencode-internal-timeout.md) | P0 止血 | — | ✅ resolved |
| 02 | [remux 接入 setts，去掉 igndts](./steps/02-setts-in-remux.md) | P0 | — | ✅ resolved |
| 03 | [验证 setts 语义，并决定 x264 去留](./steps/03-verify-and-drop-reencode.md) | P0 | 02 | ✅ resolved |
| 05 | [守住「修复产物被压扁」的静默毁片路径](./steps/05-guard-against-collapsed-output.md) | **P0 阻塞发布** | 03 | ready-for-agent |
| 04 | [重型 ffmpeg 共享 permit](./steps/04-shared-ffmpeg-permit.md) | P1 条件 | 05 | needs-info |

> ⚠️ **05 落地之前不要发版**：02 引入的 setts 在时间戳回绕（#13 形态）下会静默产出坏片
> 并删掉原片。03 的 Answer 有完整实测证据。
