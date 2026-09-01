# 03 — 进度与决策语义（主题索引）

Status: needs-triage
Blocked by: —
Type: design-index

来源：[总体设计](../spec.md)。原大任务已拆细，本文件不作为独立执行任务。

## 对应执行任务

| 子目标 | 任务 |
| --- | --- |
| executed/skipped/fallback/failed、reason 与分级规范 | [06](../../../.archive/structured-logging-p0-p2/issues/06-baseline-contract.md) |
| 独立还原、压缩差异与缺口回归 | [11](../../../.archive/structured-logging-p0-p2/issues/11-agent-reconciliation.md) |
| 录制/重连/DTS 原生观测与有界告警汇总 | [12](12-recording-pilot.md) |
| 上传进度复用、预处理/补传/投稿原因和结果 | [13](13-upload-pilot.md) |
| 进度单独展示，日志不会顶掉阅读位置 | [16](16-preview-ui.md) |
| 原生覆盖完成后删除旧专用重复调用点 | [19](19-remove-legacy.md) |

**对照期不删除或降低旧进度 INFO。** 新原生事件流降噪，旧行与新快照/汇总的语义映射写进
覆盖表；逐块 ack、业务快照、heartbeat/watchdog 仍按原规则更新。

## Comments

原先「先改善旧控制台、先删逐块 INFO」的执行顺序被并行迁移方案替代，避免破坏比较基线。
