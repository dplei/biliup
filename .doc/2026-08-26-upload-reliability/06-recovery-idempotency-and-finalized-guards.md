# 子任务 06：补传幂等、文件存在性与 finalized 防护

Status: ready-for-agent

Blocked by: 02, 05

## 目标

统一补扫、自动恢复和手动恢复的资格检查。不存在的文件不反复重试，成功或 finalized 的逻辑分段不再产生新任务。

## eligibility 规则

按以下顺序判断，并返回明确枚举结果：

1. 生命周期记录已 succeeded：幂等 no-op。
2. session 已 finalized：禁止创建新任务；既存且已绑定的 legacy missing 可进入“编辑原稿恢复”。
3. normalized path 已被其他 succeeded 行占用：幂等 no-op 或数据冲突。
4. 文件不存在/不是普通文件：转 `source_missing` 或跳过新建。
5. 媒体验证不是 Valid：不进入正常补传，保留诊断。
6. 行已经 uploading 且 lease 有效：返回 AlreadyRunning。
7. pending/failed 且 due：允许 attempt claim。

## 详细步骤

### 1. 统一资格检查

- [ ] 新建纯查询阶段 `check_recovery_eligibility`，供 rescan、manual recover、retry 和 silent recover 使用。
- [ ] 返回强类型结果：Eligible、AlreadySucceeded、AlreadyRunning、SourceMissing、FinalizedRejected、LegacyFinalizedEdit、InvalidMedia、Conflict。
- [ ] API 不再把所有非执行情况都返回模糊的 `{ok:true}`。

### 2. 文件存在性

- [ ] enrollment 和 rescan 创建新任务前使用同一规范化路径检查普通文件。
- [ ] 检查后到打开文件之间仍可能发生竞态；open 的 NotFound 同样收敛为 source_missing。
- [ ] 新发现但源文件不存在时不插入 active missing。
- [ ] 既存任务发现不存在时写 `status=source_missing`、清除 token 和 next retry。
- [ ] source_missing 不增加 attempts，不参与 silent due 查询。
- [ ] 页面允许删除记录，但不提供无意义的自动 retry；文件重新出现后可显式“重新检查”。

### 3. finalized guard

- [ ] `rescan_local_valid_segments` 查询到同场 finalized session 时返回 skipped_finalized，不创建新 session。
- [ ] `ensure_missing_segment_session` 不得在同场只有 finalized session 时创建 uploading session。
- [ ] 迟到 SegmentEvent 命中 finalized session 时写审计告警并保留文件，不新建 active missing。
- [ ] 新 v2 session finalized 后，数据库约束/仓储逻辑拒绝新增生命周期行。
- [ ] legacy missing 若在 finalized 之前已经持久绑定，可继续走 edit archive；成功后标记 succeeded。

### 4. 成功幂等

- [ ] 正常上传成功记录永久保留，不再删除。
- [ ] rescan 的 known 集合同时使用 normalized path、enrollment id 和 succeeded video identity。
- [ ] 重复 pending enqueue 不改变 succeeded、attempts、line index 或 order。
- [ ] edit archive 前读取 B 站 Studio；若目标 Video identity 已存在，直接标记本地行 succeeded，不重复 edit。
- [ ] edit 请求成功但本地写回失败时记录远端响应 identity，下一次先对账再决定是否重发。

### 5. 人工操作

- [ ] source_missing 提供“重新检查文件”而非直接上传。
- [ ] finalized legacy 恢复按钮明确会编辑现有稿件并可能触发重新审核。
- [ ] Conflict 状态不允许一键重试，要求先查看两个冲突 identity。
- [ ] 删除操作继续幂等接受本地文件已经不存在。

## 测试

- [ ] 已删除文件不会产生新的 active missing。
- [ ] 既存 pending 文件删除后只转换一次 source_missing。
- [ ] finalized session 补扫不增加 session/missing 行数。
- [ ] legacy finalized missing 可以补进原稿，但不能建新稿。
- [ ] 同一路径成功后重复 SegmentEvent、补扫和 retry 都不上传。
- [ ] B 站已存在目标 Video 时不重复 edit。

## 验收标准

- 日志不再出现针对已经成功/已删除同名文件的无限补传。
- finalized session 不会被重新打开或派生新 session。
- 每个 API 非执行结果都有明确、可展示的原因。

