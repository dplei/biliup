# 子任务 02：有效分段 durable enrollment

Status: ready-for-agent

Blocked by: 01

## 目标

把持久化边界从“上传管道开始处理”前移到“媒体确认有效”。在进入任何内存上传队列前，分段必须已经绑定 session 并拥有稳定顺序。

## 数据模型

扩展 `upload_missing_segment`，使其成为完整的分段生命周期账本，而不只是一张失败队列。新增字段由 migration 11 创建，旧行保持兼容：

- `normalized_file_path TEXT`
- `lifecycle_version INTEGER NOT NULL DEFAULT 1`
- `video_json TEXT`
- `total_bytes INTEGER`
- `uploaded_bytes INTEGER NOT NULL DEFAULT 0`
- `current_line TEXT`
- `upload_started_at DATETIME`
- `last_progress_at DATETIME`
- `attempt_token TEXT`

新 enrollment 写 `lifecycle_version=2`。为 v2 行建立：

- `(live_streamer_id, normalized_file_path)` 唯一约束；
- `(upload_session_id, segment_order)` 唯一约束；
- active 状态和 watchdog 扫描索引。

旧行在子任务 07 完成清洗前不得强制填充唯一字段。

## 详细步骤

### 1. 抽取 enrollment 仓储接口

- [ ] 新建统一的 `enroll_validated_segment` async 接口。
- [ ] 输入包含 Context、原始 `SegmentInfo`、规范化上传路径、媒体总字节和当前时间。
- [ ] 输出包含 `missing_id`、`upload_session_id`、`segment_order` 和是否为重复事件。
- [ ] 将路径规范化规则集中实现：绝对化、去除 `.`，不要求文件 canonicalize 后仍存在才能识别历史行。

### 2. 原子创建 session 与分段记录

- [ ] 在一个 SQLite transaction 内查询同一 `live_streamer_id + streamer_info_id` 的非 finalized session。
- [ ] 没有 session 时插入 uploading session；有候选时按现有恢复窗口规则续接。
- [ ] 在事务内读取 v2 最大 `segment_order` 并分配下一个值。
- [ ] 插入或复用路径唯一的生命周期行，初态为 pending。
- [ ] 同事务 upsert `filelist`，避免 enrollment 已存在但文件索引缺失。
- [ ] 若唯一冲突，读取既有行并返回；不得增加 attempts 或改写 succeeded 为 pending。
- [ ] finalized session 命中规则交由子任务 06 的 eligibility guard 处理，不在此静默创建新 session。

### 3. 调整事件边界

- [ ] 将 `SegmentEventProcessor::process` 及必要调用链改为 async。
- [ ] Valid 分段先调用 enrollment，再输出 `validated and enrolled media segment`。
- [ ] `SegmentInfo` 增加 enrollment 元数据，上传管道不得再次分配 session/order。
- [ ] 上传 Actor channel 满或暂不可用时，生命周期行保持 pending；不再仅依赖内存 channel。
- [ ] short segment 合并产物也走相同 enrollment，原始 recovery sources 继续由 manifest 保护。
- [ ] 无上传模板的仅录制模式只登记 `filelist`，不生成 active missing。

### 4. 数据库失败 outbox

- [ ] transaction 遇 SQLite busy/暂时错误时进行有上限的短退避重试。
- [ ] 最终失败时把 enrollment payload 写入 `data/upload-enrollment-outbox/` 临时文件。
- [ ] `sync_all` 文件并原子 rename 成正式 manifest，才算 durable。
- [ ] manifest 不包含 Cookie、完整错误链中的敏感 URL 或上传凭据。
- [ ] 启动时和固定间隔扫描 outbox，成功导入后删除 manifest。
- [ ] 健康接口返回 outbox 条数和最老记录时间。
- [ ] 数据库与 outbox 都失败时返回错误并保留媒体文件，禁止输出成功 enrollment 日志。

### 5. 上传成功事务

- [ ] 上传成功后按 `missing_id + attempt_token` CAS 获取当前行。
- [ ] 在同一 transaction 内写 `video_json`、状态 succeeded、完成时间和最终进度。
- [ ] 从生命周期记录按顺序重建 session `videos_json`，或在兼容期安全合并基线 Video。
- [ ] 不再删除正常成功的 missing 行。
- [ ] transaction 失败时不运行删除型 postprocessor，本地媒体继续保留。

## 测试

- [ ] validated 后立即终止进程，重启后仍能恢复 pending 行。
- [ ] channel 满、UActor 忙和初始化失败均不丢 enrollment。
- [ ] 同一路径发送 100 次只产生一行。
- [ ] 并发分段得到唯一且递增的 order。
- [ ] 成功结果与 `videos_json` 写回具备事务原子性。
- [ ] outbox 在数据库恢复后只导入一次。

## 验收标准

- 日志中出现 `validated and enrolled` 时，必能从数据库或已 fsync manifest 找到该分段。
- 上传管道不再负责创建首个 session 或猜测分段顺序。
- 正常上传成功后仍保留 succeeded 生命周期记录作为幂等依据。

