# 01 · 拆分时间戳修复的失败降级结果

Status: ready-for-agent
优先级：P0

## 目标

让 `RepairOutcome::Clean` 只表示“已经确认源时间戳无异常”，失败后继续上传原片则返回带稳定
原因的 `Fallback`，杜绝 `executed/no_anomaly` 与真实过程矛盾。

## 改哪里

[`crates/biliup-cli/src/server/common/timestamp_repair.rs`](../../../crates/biliup-cli/src/server/common/timestamp_repair.rs)：

- 新增 `RepairFallbackReason::{DetectFailed, RemuxFailed, VerificationFailed}`。
- `RepairOutcome` 新增 `Fallback(RepairFallbackReason)`。
- 初次检测错误、remux 错误、修复后检测错误分别返回对应原因。
- 回退超限、无法解析和复检确认仍异常继续返回 `Unfixable`；真正干净继续返回 `Clean`。

[`crates/biliup-cli/src/server/common/upload.rs`](../../../crates/biliup-cli/src/server/common/upload.rs)：

- 将 `Fallback` 映射为 `processing.completed fallback/<稳定原因码>`。
- 所有文件选择、临时件清理和补传分支把 `Fallback` 当作“继续使用未修复输入”，不能新增阻断。
- `Unfixable` 继续使用 `failed/unfixable`，保留现有告警策略。

## 验收

- 改现有 fake 测试：三个错误出口分别断言对应 `Fallback`，不再断言 `Clean`。
- 保留并通过 `Clean`、`Repaired`、`Unfixable` 的既有测试。
- 增加最小事件断言，证明失败降级最终不是 `no_anomaly`。
- 跑 `cargo test -p biliup-cli --lib timestamp_repair`，再跑全 crate。

## 不做

- 没有 `ReencodeFailed`：该路径已由 #25 删除。
- 不持久化预处理结果；本 issue 修的是事件事实，不改变上传恢复状态机。
