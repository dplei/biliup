# 05 — 对比、切换与验证（主题索引）

Status: needs-triage
Blocked by: —
Type: design-index

来源：[总体设计](../spec.md)、[阶段计划](../rollout-plan.md)。原来的单次切换已拆成三个
独立里程碑，未满足门槛时留在上一阶段，不将本主题单独派发。

## 对应执行任务

| 子目标 | 任务 |
| --- | --- |
| 保留旧链的入口旁路与开关 | [09](../../../.archive/structured-logging-p0-p2/issues/09-shadow-integration.md) |
| 真实双源证据和只读比较 | [10](../../../.archive/structured-logging-p0-p2/issues/10-evidence-export.md)、[11](../../../.archive/structured-logging-p0-p2/issues/11-agent-reconciliation.md) |
| 全支持入口的覆盖验收 | [14](14-coverage-expansion.md) |
| 新页面默认，继续双写观察 | [17](17-default-events.md) |
| 关闭旧写入，保留可恢复实现 | [18](18-stop-legacy-writes.md) |
| 删除旧实现和兼容收尾 | [19](19-remove-legacy.md) |

Agent 对比不仅检查旧有信息是否保留，也识别旧体系缺失、两边同时缺失和关联矛盾。
差异进入回归样本后重新采集验证；只有关键事实、健康、资源和回退门槛通过才晋级。

## Comments

本轮仍是设计与拆解。没有实施、运行期采样或部署结果，不归档为已完成工作。
