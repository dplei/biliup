# 子任务 07：数据库迁移、兼容回填与事故恢复

Status: ready-for-agent

Blocked by: 02, 05, 06（均已完成）

## 目标

在不修改既有 migration、不丢历史会话和 missing 记录的前提下，引入 v2 生命周期模型，并为会话 #227 生成只读恢复报告。

## migration 现状

本任务不再新增 schema。所需的表和字段已随前置任务落地：

- `11_add_upload_segment_lifecycle.sql`（任务 02）：`upload_missing_segment` 的生命周期、进度和 attempt 字段及兼容索引。
- `12_add_upload_line_health.sql`（任务 04）：持久线路健康表。
- `13_add_session_completeness_gate.sql`（任务 05）：submit claim 与完整性闸门。
- `14_add_upload_recovery_audit.sql`（任务 06）：迟到事件与 finalized 拒绝的审计表。
- `15_add_lifecycle_backfill_journal.sql`（任务 06）：本任务 backfill 使用的断点日志表 `upload_lifecycle_backfill` 及其事件表。

已经应用的 migration 文件禁止修改任何字节，包括注释和空白。下面「迁移前检查」与「兼容 schema」两节保留为对生产副本执行既有迁移时的核对清单。

## 详细步骤

### 1. 迁移前检查

- [ ] 在生产停写后备份 `data.sqlite3`、`data.sqlite3-wal` 和 `data.sqlite3-shm`。
- [ ] 记录 `_sqlx_migrations` 的版本和校验和。
- [ ] 对生产数据库副本执行迁移，不直接用生产库做首次验证。
- [ ] 检查旧数据中同路径多状态、同 session/order 重复和损坏 videos_json 的数量。

### 2. 兼容 schema

- [ ] 新增字段允许旧行保持 NULL/lifecycle_version=1，避免 migration 因历史重复失败。
- [ ] v2 唯一索引只约束 lifecycle_version=2 或 normalized path 非 NULL 的行。
- [ ] 为 active status、session/order、stale uploading 和线路 cooldown 建查询索引。
- [ ] migration 只做可预测 DDL，不在 SQL 中尝试解析 JSON 或删除冲突行。

### 3. 可重复 backfill

- [ ] 实现带版本标记和进度日志的 Rust backfill，可中断后继续。
- [ ] 对每个非 finalized 活动 session 解析 `videos_json`。
- [ ] 能按 title/filename stem 与 filelist/missing 对应的 Video，写回对应 lifecycle 行的 `video_json` 和 succeeded。
- [ ] 无法对应本地路径的旧 Video 创建 `legacy://session/{id}/part/{order}` synthetic succeeded 基线。
- [ ] synthetic 行只用于完整性基线，不参与本地文件恢复。
- [ ] 同路径存在多条旧 active 行时，优先保留 succeeded；否则保留最新有效状态，并合并最大 attempts、最新错误和最完整目标绑定。
- [ ] 不自动合并两个均声称不同远端 Video 的冲突，标记 Conflict 并阻止投稿。
- [ ] 清洗完成后填 normalized path 和 lifecycle_version=2；每批 transaction 提交。

### 4. finalized 历史处理

- [ ] finalized session 默认只生成审计，不创建 synthetic active 或触发 submit。
- [ ] 已绑定 finalized 的 legacy missing 保留原绑定，供人工 edit 恢复。
- [ ] finalized 后新发现的孤儿文件列入报告，不自动建 session。

### 5. 会话 #227 审计

- [ ] 从生产副本读取 session #227 的 aid/bvid/status/submit trace/videos_json。
- [ ] 列出绑定或同场的所有 missing 行、状态、attempts、线路和错误。
- [ ] 列出 22:29:32、22:59:56、23:30:19 文件是否存在、大小和媒体有效性。
- [ ] 通过只读 B 站信息接口获取实际稿件分 P；若生产授权不可用，则输出待人工采集命令。
- [ ] 按 order、源路径、Video filename、title 比对缺失与重复。
- [ ] 对 `04:54:30` 重复项给出 identity 证据，不仅凭标题判断。
- [ ] 输出三类建议：无需处理、可安全补齐、必须人工确认。
- [ ] 未经用户确认不调用 edit、不删除分 P、不改生产 DB。

### 6. 回滚策略

- [ ] 新代码回滚后旧版本可忽略新增 nullable 字段和新表。
- [ ] 不通过 down migration 删除生命周期记录。
- [ ] migration 失败时恢复停写前备份；backfill 失败优先从进度继续，不覆盖原 videos_json。
- [ ] 保留迁移及 backfill 的结构化摘要以便审计。

## 测试

- [ ] 用旧 schema fixture 启动并自动迁移成功。
- [ ] 重复、损坏和空 videos_json 数据均不会导致启动崩溃。
- [ ] backfill 运行两次结果一致。
- [ ] 中断后继续不会重复 synthetic 行。
- [ ] 迁移后的 active legacy session 能通过完整性闸门正确判断。

## 验收标准

- 生产数据库副本迁移、backfill、重启和查询全部通过。
- 历史数据没有被静默删除或自动重投。
- #227 获得可操作但不自动执行的恢复报告。

