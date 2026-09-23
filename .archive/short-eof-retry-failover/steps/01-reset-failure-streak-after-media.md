# 01 · 产出媒体后复位连续失败

Status: resolved

## 目标

重复出现“连接成功、产出可解析媒体、随后短寿命 EOF”时，每轮仍记录一次 transport failure，
但连续失败数不跨 productive attempt 累积，重试等待保持在现有基础值。

## 改动

- `crates/biliup-cli/src/server/common/download.rs`
  - 在 `SegmentEventProcessor` 中维护私有的媒体分段计数；`Valid` 和 `RecoverableShort` 都计入，
    `Invalid` 不计入。
  - `DownloadTask::download` 在 attempt 前后读取计数差值，把 `productive_attempt` 放进
    `DownloadAttempt`。
  - 把该布尔值传给 `RouteHealthState::observe_live_attempt`，不新增重试状态或配置。
- `crates/biliup-cli/src/server/common/route_health.rs`
  - 保持 `stable_attempt` 的五分钟/完整分段定义和指标用途不变。
  - `stable_attempt || productive_attempt` 可以在计入本次终止状态前清理旧的连续失败、冷却和
    storm 状态；本次 EOF 随后重新成为失败 1。

不要用 `ConnectionDiagnostics::received_bytes` 或 FLV 头读取成功代替媒体校验；前者会把协议头、
半帧和无效容器也算作恢复。

## 测试

- 在 `route_health.rs` 增加一个表驱动/循环用例：同一路线连续二十次以
  `productive_attempt=true`、短 `connected_for`、`StreamEnded` 结束，断言每次返回
  `Failure { failures: 1, circuit_opened: false }`。
- 保留并运行现有无进展失败、稳定恢复、认证刷新、切线和全部冷却测试，确认真正失败仍会熔断。
- 在现有分段处理测试中补一个最小断言：可恢复短媒体会推进 productive 计数，header-only 不会。

## 验收

- `cargo test -p biliup-cli route_health --lib`
- `cargo test -p biliup-cli --lib`

## 回执

- `SegmentEventProcessor` 直接复用 `FileValidator` 的结果累计 productive 分段；`Valid` 和
  `RecoverableShort` 计入，`Invalid` 不计入。
- `DownloadTask::download` 用 attempt 前后计数差值生成 `productive_attempt`；线路状态机用
  `stable_attempt || productive_attempt` 清理旧失败串，但稳定尝试指标仍只认原有五分钟/完整分段判据。
- 新增回归覆盖 20 轮 productive 短 EOF 每轮均为失败 1、不开熔断，以及短媒体推进计数、
  header-only 不推进计数。
- `cargo test -p biliup-cli route_health --lib`：通过（15 passed）。
- `cargo test -p biliup-cli --lib`：通过（367 passed，8 ignored）。

## Comments
