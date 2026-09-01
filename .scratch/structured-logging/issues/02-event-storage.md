# 02 — 存储与故障边界（主题索引）

Status: needs-triage
Blocked by: —
Type: design-index

来源：[总体设计](../spec.md)。原大任务已拆细，本文件不作为独立执行任务。

## 对应执行任务

| 子目标 | 任务 |
| --- | --- |
| 资源预算、数据契约和故障样本 | [06](../../../.archive/structured-logging-p0-p2/issues/06-baseline-contract.md) |
| 采集接口、独立过滤、有界队列与健康 | [07](../../../.archive/structured-logging-p0-p2/issues/07-independent-core.md) |
| SQLite/附件/批量写入/清理/备份/故障隔离 | [08](../../../.archive/structured-logging-p0-p2/issues/08-sqlite-writer.md) |
| 不改旧链的入口旁路与生命周期 | [09](../../../.archive/structured-logging-p0-p2/issues/09-shadow-integration.md) |
| 固定来源边界、脱敏只读导出 | [10](../../../.archive/structured-logging-p0-p2/issues/10-evidence-export.md) |
| durable 业务审计的幂等投影 | [14](14-coverage-expansion.md) |

独立库和业务库不共享连接池/迁移，仍共享磁盘；并行预算必须包含旧文件、新库、WAL 和
附件。普通异步诊断尽力保存，业务审计不因此丢掉事务可靠性，不承诺跨库原子写入。

## Comments

07–08 在接现有入口前即可独立交付；云 SQL 和无限量诊断归档不是前置条件。
