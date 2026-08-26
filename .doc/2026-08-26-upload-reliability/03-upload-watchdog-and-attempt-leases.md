# 子任务 03：上传 watchdog、取消与 attempt lease

Status: completed

Blocked by: 01, 02

## 目标

任何上传都不能无限停留在 uploading。自动恢复、手动补传和主上传路径共用 attempt lease、进度采集、取消与超时状态机。

## 状态约定

- claim：`pending|failed -> uploading`，同时生成不可复用的 `attempt_token`。
- 成功：只有 token 仍匹配的 attempt 才能写 succeeded。
- 失败/超时：增加 attempts、推进 line index、清除 token、写 failed 和 next retry。
- `source_missing`、succeeded、deleting 不可被 claim。

## 详细步骤

### 1. 原子 claim

- [x] 实现单条 SQL CAS claim，条件包含可执行状态、due time 和当前无有效 token。
- [x] attempt token 使用随机 UUID，不以时间戳或自增 attempts 代替。
- [x] claim 时写 `upload_started_at`、`last_progress_at`、`uploaded_bytes=0` 和 `current_line`。
- [x] 并发请求未获得 claim 时返回明确的 AlreadyRunning/AlreadyCompleted，而不是伪成功。

### 2. 取消注册表

- [x] 建立进程级 `missing_id -> {attempt_token, CancellationToken}` 注册表。
- [x] 注册和移除使用 RAII guard，panic/错误返回也必须清理。
- [x] 手动 retry 先取消匹配 token 的旧任务，并等待其 future 退出或达到短取消等待上限。
- [x] 取消只影响该 missing id，不影响同 session 的其他分段。
- [x] reqwest future 被 drop 后必须释放全局上传 permit。

### 3. 进度回调

- [x] 在 `crates/biliup` 上传接口增加“服务端确认一个分块完成”的 observer。
- [x] observer 参数至少包含本次完成字节、累计完成字节、总字节和分块序号。
- [x] 不把“已从磁盘读取”误记为“已上传”；进度以 UPOS 分块响应成功为准。
- [x] biliup-cli 侧按 5 秒或 16 MiB 节流持久化。
- [x] 每次持久化都带 attempt token 条件，旧 attempt 的进度更新影响 0 行。

### 4. watchdog

- [x] 无进度 deadline 初值为 attempt 启动后 5 分钟。
- [x] 每次成功分块将无进度 deadline 推迟 5 分钟。
- [x] 总 deadline 固定为 attempt 启动后 2 小时，不因进度延长。
- [x] 使用 `tokio::select!` 同时等待上传结果、取消信号和两个 deadline。
- [x] 超时错误区分 `no_progress_timeout` 与 `total_upload_timeout`。
- [x] 超时后执行一次统一失败收敛，避免多分支重复增加 attempts。

### 5. 重启恢复

- [x] 服务启动后扫描 uploading 行。
- [x] 进程内没有匹配 token 且 `last_progress_at` 超过 5 分钟的行视为遗留 lease。
- [x] 遗留行 CAS 转 failed、attempts 增加一次、line index 前进一次。
- [x] 未达到阈值的行延迟到阈值再处理，不在启动瞬间误杀刚写入的任务。

### 6. 页面与 API

- [x] missing 列表返回 `current_line`、字节进度、开始时间和最近进度时间。
- [x] uploading 行显示百分比、当前线路和已无进度时长。
- [x] retry 操作文案明确“取消旧 attempt 并从下一条健康线路重新上传”。
- [x] retry API 不保持旧的“旧请求不一定取消”语义。

## 测试

- [x] 4 分 59 秒无进度不超时，5 分钟触发一次失败转换。
- [x] 持续进度达到 2 小时仍触发总时长超时。
- [x] 手动 retry 能取消旧 future并释放 semaphore。
- [x] 旧 attempt 延迟成功无法写 Video 或 succeeded。
- [x] 进程重启后 stale uploading 自动恢复为 due failed。
- [x] 并发 retry 只有一个 token 获得执行权。

## 完成记录

- 完成时间：2026-08-26。
- 实现提交：`d009e40`（`fix: supervise upload attempt leases`）。
- 验证：`SQLX_OFFLINE=true cargo check --workspace`；`cargo test -p biliup-cli --lib`（190 passed，1 ignored）；事故 fixture（10 passed，4 个跨后续任务契约 ignored）。
- 前端业务页已通过 Next.js 编译与类型检查到既有阻塞点；完整构建仍被 `app/ui/TemplateFields.tsx:117` 的既有 `onClick` 类型错误阻塞，与本任务改动无关。

## 验收标准

- 数据库中不存在无限期 uploading；所有 uploading 都有 token、线路、开始和进度时间。
- 发生超时后 attempts 和 line index 恰好各推进一次。
- 新 attempt 成功后，旧 attempt 无法污染 session。
