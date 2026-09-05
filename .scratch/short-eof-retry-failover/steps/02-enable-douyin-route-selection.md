# 02 · 让抖音候选真正进入线路选择

Status: resolved

## 目标

当 `route_health_enabled=true` 且 `douyin_route_failover=true` 时，抖音候选必须传入启用状态的
`RouteHealthState::select_route`；无进展失败达到熔断阈值后可以选择不同健康候选。

## 改动

- `crates/biliup-cli/src/server/common/download.rs`
  - `failover_enabled` 的平台判据改用 `stream.platform == "douyin"`。
  - 不修改 `LivePlugin::name()`：它是既有展示名，其他调用也依赖首字母大写形式。
  - 不修改 `RouteKey` 或候选排序。

这是根因修复，不需要新建平台枚举或统一重命名所有插件。若直接测试这一行必须搭建完整下载流程，
可抽一个私有的一行开关函数并只测“抖音机器值 + 两个开关”；不要为它建立新类型。

## 测试

- 复用 `route_health.rs` 现有的 `two_flv_failures_switch_to_different_host_before_hls`，确认状态机行为
  保持不变。
- 增加一个调用端开关回归：机器平台值 `douyin` 时开启，展示名 `Douyin` 不再参与判定；任一配置
  开关关闭时结果为 false。

## 验收

- `cargo test -p biliup-cli route_health --lib`
- `cargo test -p biliup-cli --lib`

## 回执

- `failover_enabled` 改由私有的 `douyin_failover_enabled(&stream.platform, ..)` 计算，平台判据用机器值
  `douyin`；`plugin.name()` 的展示名不再参与判定。`LivePlugin::name()`、`RouteKey` 与候选排序均未改动。
- 新增调用端开关回归 `douyin_failover_needs_machine_platform_and_both_switches`：机器值开启、展示名
  `Douyin` 为假、任一配置开关关闭为假、其他平台为假。
- `cargo test -p biliup-cli route_health --lib`：通过（15 passed）。
- `cargo test -p biliup-cli --lib`：通过（368 passed，8 ignored）。

## Comments
