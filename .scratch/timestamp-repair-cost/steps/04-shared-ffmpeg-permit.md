# 04 · 重型 ffmpeg 共享全局 permit

Status: needs-info
Blocked by: 03
优先级：P1，**条件性**

## 条件

**只有 03 决定保留 x264 兜底时才做。** 如果 setts 全项通过、重编码被删掉，剩下的
remux/scan 都是 IO 密集的，CPU 争抢的前提消失，这一步直接标 wontfix。

## 为什么

`NORMALIZE_SLOTS`（`Semaphore::new(1)`）定义在
[`audio_normalization.rs`](../../../crates/biliup-cli/src/server/common/audio_normalization.rs)，
只在 `normalize_for_upload` 内部持有。`normalize_timestamps` 从
[`upload.rs`](../../../crates/biliup-cli/src/server/common/upload.rs) 独立调用，不持有任何
permit，于是两个 CPU 密集型 ffmpeg 能在 2 vCPU 上同时跑。

生产观测：同为约 30 分钟、输出大小接近的四段，响度标准化墙钟耗时 7分40秒 → 12分21秒 →
20分25秒 → 22分54秒，后三段都与时间戳重编码重叠，退化到基线的 2.7–3 倍。
两者都以 background 优先级启动（`process_priority::background`），但**优先级相同就不解决
互相争抢**。

## 改哪里

把这个 permit 提成两边共用的东西，时间戳重编码也持有。约十行。

- 只让**重编码**共享，还是扫描/remux 也共享，单独判断：扫描和 remux 是 IO 密集的，
  把它们也串行化会平白拉长端到端延迟。倾向只护重编码。
- 注意别把 permit 的持有范围扩大到网络上传阶段——那是 `queued` 相有自己的 permit，
  两个搅在一起会造出新的死锁面。
- `audio_normalization.rs` 里有一条注释把 `NORMALIZE_SLOTS` 和就地替换绑成跨管道不变量
  （单测证明过），挪动定义时不要破坏它。

## 验收

- `cargo test -p biliup-cli` 全绿，尤其是那条依赖 `NORMALIZE_SLOTS` 的跨管道不变量测试。
- 一个断言「两个重型任务不会同时进入临界区」的单测；别为此搭测试框架。
