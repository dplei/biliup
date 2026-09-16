# 单个短片段 defer 后无人消费

来源：[dplei/biliup#54](https://github.com/dplei/biliup/issues/54)（前置观察见 #11 的第二个前置条件）

## 现象

`preserve_recoverable_short_segments: true` 时，「分段边界后几十秒内断流、随后同 session 续录」
每次都在工作目录留下一个孤儿 `.flv` + `.biliup-recovery-*.json` + 一条
`recoverable_short_batch` 行，状态永远 `Deferred`、`attempts=0`、`next_retry_at` 过期后无任何后续。
这几秒到几十秒的内容不进成片，且随时间累积。

## 根因（已对照代码核实）

`crates/biliup-cli/src/server/common/download.rs`：

1. `flush_pending_short_segments` 只在 `group.len() > 1` 时调用 `merge_compatible_segments`；
   单个片段直接 fall through 到 `defer_recovery_batch`，`last_error` 硬编码为
   `"media parameters are not compatible with an adjacent recovery group"`。这条路径**没有做任何
   兼容性比较**，文案是误导。
2. `recoverable_short_batch` 表有入无出：全仓只有 `upload.rs::persist_recovery_batch_manifest`
   的 INSERT 和 `GET /v1/recovery-batches` 的只读 SELECT。`recovery_scheduler.rs` 只扫
   `upload_missing_segment`。因此 `next_retry_at`、`recoverable_short_retry_interval_secs`
   都不起作用。
3. 本场景每个分段边界只产出一个短片段（它是上一条连接被时间条件切出来的尾巴，随即断流），
   flush 由下一条连接的 Valid 分段触发（`process` → `Valid` → 先 flush 再 enqueue），
   pending 里永远只有 1 个，必然走到 1 → 2。

补充发现（不在 issue 里）：

- `recoverable_short_segment_mode` 是死配置：只在 `config.rs` 定义和测试，没有任何读取者。
  `merge_or_defer` 这个值从未影响行为。
- `media_compatibility_key` 在 `danmaku_file_path.is_some()` 时直接返回 `None`，
  `compatible_segment_groups` 对 `None` 键不合组。开弹幕录制的房间即使一次攒下多个短片段，
  也会拆成 N 个 size=1 的组、全部 defer。本 issue 不修这条，但它意味着「size=1 的组」
  不等于「pending 只有 1 个」，方案里要区分。
- 设计出处：`b7b19df fix(download): defer recoverable short-segment batches`。当时的 plan 写了
  「延迟恢复任务由后台低频重试」但从未实现；`Deferred` 的原始语义是「合并失败」，单片段
  fall through 是没被设计过的路径。当时禁止「逐片上传原文件」的动机是抖音重连风暴下几十个
  短片同时打上传接口触发 601；这个风险现在已由 `UploadRateGate` + 有界队列独立兜住。

## 三个候选的取舍

| 方案 | 内容找回 | 改动 | 代价 |
| --- | --- | --- | --- |
| 1 拼到下一个 Valid 头部 | 完整、无碎分 P | 中：flush 需拿到下一个 Valid 事件；复用 `merge_compatible_segments(&[short, valid])` | 为几秒内容 `-c copy` 重写一个 ~GB 级文件；临时双倍磁盘；2 vCPU 服务器上传前多等数十秒 |
| 2 加消费者按 `manual_recover` 补进 session | 找回 | 大：新 scan、manifest 回读、重新验证、enroll | enroll 的 `segment_order = MAX+1`，15 分钟后补进去**顺序错**；session 已 finalize 则 `FinalizedRejected`；表里没有 session id |
| 3 直接删除 + 记账 | 不找回 | 小 | 与 `preserve_recoverable_short_segments` 的语义矛盾（开了等于没开） |

**方案 2 的「延后」本身是问题根源**：flush 发生的那一刻，处理器手里正好有正确的顺序位置
（下一个 Valid 尚未 enqueue，或 session 正在 finish），延后到 scheduler 反而把顺序信息丢了。

## 采用：方案 2 的原地版本——孤立短片段当场作为独立分 P 入队

在 `flush_pending_short_segments` 里，**当 pending 只有一个事件**时直接
`enqueue_validated(&mut event, bytes)`，不写 manifest、不落 `recoverable_short_batch`。

- 顺序天然正确：`process` 里 flush 在下一个 Valid 的 enqueue 之前；`finish` 里 flush 在
  `persist_closed_session_intents` 之前，所以它会成为该 session 的倒数第二/最后一个分 P。
- 没有上传风暴：一个文件一次上传，且仍受 `UploadRateGate` 与有界队列约束（队列满时
  `enqueue_validated` 已有 enrollment 兜底，走 `upload_missing_segment` 的正常补传）。
- 磁盘：不产生新文件，上传成功后按既有分段生命周期清理。
- 已有的 `>1` 合并路径与合并失败 defer 路径不动。

代价是成片里多一个几秒到几十秒的分 P。这是 `preserve_recoverable_short_segments` 开启后
用户已经接受的语义（合并产物本身也可能小于阈值，现有代码照样 enqueue），不再另设阈值。

**不做方案 1** 的原因写在表里：为几秒内容重写 GB 级文件不值。若日后觉得碎分 P 不可接受，
再评估「短片段 ≤ N 秒时并入下一个 Valid」，届时 `merge_compatible_segments` 可直接复用。

### 仍走 defer 的情况要说实话

pending 有多个事件但拆成了 size=1 的组（键不同或键为 `None`）时保留 defer，但 `last_error`
改为如实描述：`"single segment cannot form a merge group (N pending, incompatible or unkeyed)"`
之类。这类 `Deferred` 目前只能人工处理，文档 `docs/short-segment-recovery.md` 要把
「`Deferred` 批次不会自动重试」写明，不再暗示有后台重试。

### 不做

- 不给 `recoverable_short_batch` 加 scheduler 消费者：顺序问题无解，见上表。
- 不删 `recoverable_short_segment_mode` / `recoverable_short_retry_interval_secs`：配置项删除
  牵动 UI 与 config 兼容，另开 issue。
- 不清理生产上已有的孤儿：已知列表可人工用 `manual_recover` 或直接删除，不写一次性迁移。
- 不修弹幕房间 `media_compatibility_key` 返回 `None` 的问题。

## 验收

1. 单元测试：pending 恰好 1 个 → 走 enqueue，不生成 manifest；pending 2 个不兼容 →
   仍 defer 且 `last_error` 是新文案。
2. dev 环境：`segment_time` 调短，录制中在分段边界后手动断流再续，日志应出现
   `queueing recoverable short media segment` → `segment validated and enrolled`（同一文件），
   **不出现** `recoverable segment deferred without immediate upload`；成片分 P 数比原来多 1。
3. 生产：新版本上线后 `recoverable_short_batch` 不再新增 `attempts=0` 的单文件行；
   工作目录不再残留单个 `.biliup-recovery-*.json`。
