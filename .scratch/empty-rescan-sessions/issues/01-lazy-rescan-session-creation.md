# 01 — 补扫延迟创建会话

Status: resolved

## 背景

`rescan_local_valid_segments` 当前先调用 `insert_uploading_session(..., &[])`，之后才枚举并验证本地
候选。候选全部消失或无效时，新会话没有 lifecycle baseline，也没有任何可恢复内容。

## 改动范围

1. 把补扫拆成只读发现阶段与 enrollment 阶段：
   - 收集 `filelist` 与工作目录候选；
   - 规范化路径、排除已登记路径与已上传 stem；
   - 使用现有 `FileValidator` 验证；
   - 只有存在有效未知候选时才进入 enrollment。
2. 删除补扫路径对 `insert_uploading_session` 的提前调用。第一条有效候选交给
   `enroll_validated_segment` 原子创建或复用会话。
3. `LocalSegmentRescanResult.upload_session_id` 改为可空：
   - 已有活动/finalized 会话时返回其 id；
   - 首条 enrollment 成功后返回 enrollment 的会话 id；
   - 无会话且零有效候选时返回 `null`。
4. 保留 finalized 会话短路和 `rescan_skipped_finalized_session` 审计。
5. 增加零产出与新建会话的结构化日志；前端 Toast 不再假定一定有会话号。

## 不变量

- 零有效未知候选时不得新增或更新 `upload_session`、`upload_missing_segment`。
- 不采用“创建后再删除”的补偿式实现。
- 重复补扫同一文件只能得到一条 lifecycle row。
- 同一次补扫的所有有效候选必须落到同一个符合场次身份规则的会话。
- finalized 历史场次不得被创建替代会话。

## 验收与测试

1. 孤儿 `streamerinfo` + 空临时目录：返回 `upload_session_id=null`、`created_session=false`、
   `queued=0`，会话计数不变。
2. `filelist` 指向不存在文件：同上。
3. 目录只有 13 字节 FLV：`skipped_invalid=1`，但不创建会话。
4. 一条测试生成的有效媒体：创建一个会话、登记一条 lifecycle row，返回对应 id。
5. 对同一目录重复调用：第二次 `queued=0`、`skipped_known>=1`，会话和 lifecycle 计数不增长。
6. 已有活动会话但没有新文件：返回原会话 id，不改其投稿状态或更新时间。

## Comments

- 现场没有遗留分段不构成阻塞：前三类回归只需要空目录/不存在路径；有效路径可复用测试媒体生成器。

## Answer

- `LocalSegmentRescanResult.upload_session_id` 已改为可空，并新增 `valid_candidates`。
- 补扫不再提前调用 `insert_uploading_session`；首个有效未知媒体统一走
  `enroll_validated_segment`，由事务返回是否创建了会话。
- 新增 `local_rescan_without_valid_candidate_has_no_session_side_effect` 与
  `local_rescan_creates_session_with_first_valid_candidate_and_is_idempotent`，覆盖空目录、消失文件、
  13 字节无效 FLV、有效合成 FLV 和重复补扫。
