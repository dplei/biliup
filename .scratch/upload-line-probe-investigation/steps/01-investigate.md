# 01 核对故障判读与补传状态机

Type: research
Status: resolved

## 问题

确认 #96 是否是持续全线不可用、补传永久停滞或源文件丢失，
并区分环境故障与代码造成的观测缺口。

## Answer

原始日志与只读账本确认关联补传通过备用线路完成，原 issue 的持续故障判断需要更正。
短暂探测失败与一次过期证书错误是真实证据；其他失败原因因诊断链丢失而不能追溯。

调用链入口：

- `upload_line_selection.rs::resolve_planned_line`
- `line.rs::retained_lines / choose_line_and_failures / Probe::probe_line`
- `upload.rs::decide_upload_line / claim_manual_recovery / record_line_probe_failure`
- `recovery_scheduler.rs::recover_due_segments / start_due_recovery_scan`
- `upload_line_health.rs::record_success / record_failure`

详细结论及限制见 [spec](../spec.md)。本轮不修改业务代码。

## 回执

完成原始证据核对、公开索引与 TLS 校验、只读终态断言；同步代码导航。
下一步见 [02](02-preserve-probe-diagnostics.md)，待后续实施。

## Comments

公开记录只保留事件链、状态和结论，不复制账号内容、生产标识、时间线或统计。

