# 子任务 08：验证、灰度、发布与可观测性

Status: ready-for-agent

Blocked by: 01, 02, 03, 04, 05, 06, 07

## 目标

验证所有上传入口遵守同一生命周期和熔断规则，通过测试主播灰度后再部署生产，并确保出现异常时可回滚镜像而不丢恢复数据。

## 详细步骤

### 1. 静态和单元验证

- [ ] `cargo fmt --all -- --check`。
- [ ] `SQLX_OFFLINE=true cargo check --workspace`。
- [ ] 运行 biliup 与 biliup-cli 全部单元测试。
- [ ] 运行 migration 和 SQLite 集成测试。
- [ ] 运行前端 `tsc --noEmit`、lint 和 production build。
- [ ] 检查代码中不存在关闭 TLS 校验或 fallback 到 `Line::default()` 的路径。

### 2. 端到端故障矩阵

- [ ] validated 后立即结束进程，重启恢复 pending。
- [ ] validated 后发生 TransportError，前一段仍登记。
- [ ] UActor 被长直播占用，其他直播仍立即 enrollment。
- [ ] 上传中间分块永久 pending，5 分钟触发 no-progress timeout。
- [ ] 持续有进度但超过 2 小时，触发 total timeout。
- [ ] 用户手动 retry，旧 future 被取消且无法延迟写回。
- [ ] bda2 失败后切 tx，tx 失败后切 auto。
- [ ] bldsa 证书过期后冷却 24 小时并立即回退。
- [ ] 文件删除后任务转 source_missing，不继续增长 attempts。
- [ ] session 有任一 active missing 时下播不 submit。
- [ ] 所有 missing 成功后只 submit 一次。
- [ ] 重复 SegmentEvent、补扫、API 双击和重启恢复不产生重复分 P。
- [ ] finalized session 的补扫结果为 skipped，不创建 session。

### 3. 结构化日志与指标

- [ ] 记录 validated、enrolled、outbox、pending、uploading、failed、succeeded、source_missing 数量。
- [ ] 记录每次 attempt token 的短标识、当前线路、attempts、开始/完成/取消原因。
- [ ] 记录 watchdog 类型、已上传字节和最后进度距今时间。
- [ ] 记录每线路 probe/pre-upload/upload 成功率、错误分类和 cooldown 剩余。
- [ ] 记录 session completeness 各状态计数和 blocked submit 次数。
- [ ] 记录 order/identity 冲突，禁止在日志中输出鉴权信息。

### 4. API 与页面验收

- [ ] 缺失补传页能看到当前线路、下次线路、字节进度、最近进度和超时原因。
- [ ] uploading 的 retry 文案说明会取消旧 attempt。
- [ ] source_missing 显示为源文件缺失，不展示普通补传按钮。
- [ ] finalized、Conflict 和线路冷却均显示具体原因。
- [ ] session 被阻止投稿时显示阻塞数量和对应分段。
- [ ] 健康接口能看到 outbox backlog、stale uploading 和 line health。

### 5. 灰度

- [ ] 使用测试主播和 30 分钟 FLV 切片配置运行至少一场完整直播。
- [ ] 主动注入一次单线路失败和一次上传停顿。
- [ ] 验证下载继续落盘，上传超时不会阻塞其他直播 enrollment。
- [ ] 验证故障恢复后 session 自动完整投稿，分 P 数量和顺序与本地账本一致。
- [ ] 灰度期间不对会话 #227 执行自动修复。

### 6. 生产部署

- [ ] 按 `BUILD_AND_DEPLOY.md` 使用 amd64 buildx 构建，不在 2C2G ECS 编译。
- [ ] 推送 immutable tag 和 latest，并记录远端 digest。
- [ ] 停容器后备份 `/opt/data`，确认 `/opt` 持久挂载来源。
- [ ] 拉取新镜像并确认 migration 11/12 成功、无 restart loop。
- [ ] 验证 Web、数据库表、outbox 目录和 health API。
- [ ] 保留上一镜像 tag、image id 和数据库备份路径。

### 7. 发布后观察

- [ ] 连续观察至少 7 天或 20 场直播。
- [ ] 每条 validated 日志抽样核对 lifecycle 行。
- [ ] active uploading 最老年龄不得超过 watchdog 阈值和调度延迟。
- [ ] 不完整投稿调用次数必须为 0。
- [ ] 检查同源 identity 重复 Video 数量必须为 0。
- [ ] bldsa 冷却期间 probe/pre-upload 次数必须为 0。
- [ ] finalized 后新增 active missing/session 数量必须为 0。

### 8. 回滚

- [ ] 功能异常时回滚到上一 immutable 镜像，保留新增表和生命周期数据。
- [ ] 不因回滚删除 pending/outbox 或本地媒体。
- [ ] 只有确认数据库损坏时才停写并恢复备份。
- [ ] 回滚后生成失败时间窗、受影响 session 和待恢复分段清单。

## 最终验收标准

- 01 中所有事故 fixture 通过。
- 所有构建、测试、migration 副本验证和前端检查通过。
- 灰度直播能够在故障注入后自动恢复且只投稿一次。
- 生产观察期没有再次出现“validated 但不可见”“无限 uploading”“不完整投稿”或“finalized 后新补传”。

