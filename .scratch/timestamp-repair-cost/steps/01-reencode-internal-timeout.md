# 01 · 重编码自带超时，超时按降级处理而不是拖垮 attempt

Status: resolved
优先级：P0（止血，与 02 无耦合，可先合）

## 为什么

现在重编码没有自己的时间上限，唯一的上限是 attempt 层的 `preprocess_deadline`
（10 min + 10 min/GiB）。它一到点就 drop 整个上传 future、`kill_on_drop` 杀 ffmpeg，
attempt 判失败，扫描结论和 remux 中间产物一起丢弃，恢复调度再从第一步重来。

生产上同一个 1.10 GB 输入因此连续三次各烧 30 分钟，一次可上传结果都没有。

而这个情形本该有现成的表达：`RepairOutcome::Unfixable` —— 保留本地原文件、发 webhook
告警、直传原片。attempt 本身应该是**成功**的。

## 改哪里

- [`crates/biliup-cli/src/server/common/timestamp_repair.rs`](../../../crates/biliup-cli/src/server/common/timestamp_repair.rs)
  - `SystemFfmpeg::reencode` 外面套 `tokio::time::timeout`。超时即返回 Err；
    `normalize_timestamps` 现有的「reencode 进程失败 → 降级 Clean 直传原片」分支已经能
    接住，但要确认这条路径也会**告警**——目前只有 `Unfixable` 触发 webhook，
    静默降级会让这类片段无声无息地按原样上传。二选一：
    - 超时映射为 `Unfixable`（保留本地文件 + 告警，语义最准）；或
    - 保持降级 `Clean`，但补一条明确的 `error!` + 告警。
    倾向前者：文件确实没修好，本地留一份等手动处理是对的。
- 超时值取一个明显小于 `preprocess_deadline` 的常数，并在注释里写清它为什么必须更小
  （否则又会被 attempt watchdog 抢先）。别再引入一个随文件大小变化的公式——R3 说明了
  字节数对像素重编码不是合适的代理变量。

## 验收

- 单测：`FfmpegRunner` 的 fake 里让 `reencode` 挂起超过超时值，断言
  `normalize_timestamps` 返回预期 outcome，且不 panic、不泄漏临时文件。
  现有测试模块已有脚本化 fake，照着加一个用例即可，别新建测试骨架。
- `cargo test -p biliup-cli` 通过。

## 注意

- 超时后临时件要清掉（现有失败分支已经在 `remove_file`，确认新路径也走到）。
- 不要顺手改 `preprocess_deadline`。那是另一个问题，且 02/03 落地后 x264 大概率整个消失。

## Answer

已实现，选了「超时映射为 `Unfixable`」那一支。

- `REENCODE_TIMEOUT = 10 min`，定义在 `timestamp_repair.rs` 顶部，注释写明它为什么必须
  小于 attempt 层的 `preprocess_deadline`。取固定值而不是随文件大小变化的公式：R3 已经
  说明字节数对像素重编码不是合适的代理变量，而在 2 vCPU 上十分钟做不完的软件编码，再给
  多久也做不完。
- 超时在 `normalize_timestamps` 里收口（不在 `SystemFfmpeg::reencode` 内），这样它对任何
  `FfmpegRunner` 实现都成立，也才能用 fake 驱动测试。future 被 drop 时 `kill_on_drop`
  收掉 ffmpeg，半成品临时件同路清掉。
- 核对过下游语义：`upload.rs` 的 `upload_path` 在 `Unfixable` 时取标准化产物/原片照常
  上传，落库后保留本地文件并发 webhook 告警，attempt 本身成功。正是想要的效果。
- 新单测 `unfixable_when_reencode_exceeds_its_own_timeout`，用 `#[tokio::test(start_paused)]`
  的虚拟时钟，跑完 0.01s。为此给 `biliup-cli` 的 dev-dependencies 加了
  `tokio` 的 `test-util` feature——只影响测试构建。
- `cargo test -p biliup-cli --lib timestamp_repair`：9 passed, 1 ignored。
