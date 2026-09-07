# 录制重试关联到触发它的下载 attempt

来源：[dplei/biliup#28](https://github.com/dplei/biliup/issues/28)

本轮只完成源码核对、语义决定与实施拆解，不修改业务代码。公开规划不复述生产样本数量、
场次或时间；只保留已做只读核对且缺失持续存在的结论。

## 结论

根因位于服务端录制循环的唯一调用点：

- `crates/biliup-cli/src/observe.rs::retry_scheduled` 已接受 `attempt: Option<&str>`，并把它写入
  `download_attempt_id`，事件模型、SQLite、查询与前端均已支持该字段。
- `crates/biliup-cli/src/server/common/download.rs::DownloadTask::execute` 调用该 emitter 时固定传
  `None`，因此不是采集、清洗或存储阶段丢字段。
- 本轮下载结束后，`plugin.check_stream` 会返回下一轮的 `LiveStream`，代码随后用它覆盖当前
  `stream`。所以不能在发 retry 时临时读取 `stream.attempt_id`：那会把退避关联到下一次选流，
  而不是触发退避的已结束 attempt。

## 字段语义

沿用现有 `download_attempt_id`，明确 `recording.retry_scheduled` 上的值指向**触发本次退避的、
刚结束的下载 attempt**。`recording.disconnected` 与 retry 因而共享同一个 ID；后续真正建立连接
时，`recording.reconnected` 仍使用新 attempt 的 ID。

不改名为 `triggering_download_attempt_id`：现有字段已经进入 allowlist、存储、查询和界面，改名会
制造一次无收益的 schema/API 迁移。也不在调度时预分配下一次 attempt；下一次连接是否发生、使用
哪条刷新后的流，都应由下一轮实际下载决定。

## 最小方案

在每轮 `DownloadTask::execute` 开始实际下载前，复制当时的 `stream.attempt_id`；后续检查直播状态、
刷新候选和覆盖 `stream` 都不再改变这份快照。只有该轮确实执行过下载时，非零退避事件才传入这份
快照。

这比从 `pending_reconnect`、线路健康状态或事件时间反推更小，也不会把没有执行下载的冷却轮次
冒充成新 attempt。`DownloadTask::download`、各 downloader 和 emitter 的公开签名无需改变。

## 契约与校验

在 `.scratch/structured-logging/contract-v1.md` 的 C05 说明中补一句：retry 的 DA 指向触发退避的
已结束 attempt，reconnected 的 DA 指向新连接 attempt。`coverage-ledger.md` 同步记录这条关联规则
和回归入口。

不新增通用“服务端身份存在则 DA 必不为空”的机器校验。v1 总契约允许拿不到的 ID 保持未知，且
当前并非所有平台都会分配 `LiveStream.attempt_id`；无条件校验会把范围扩大成全平台 attempt 分配
改造。此次回归只约束“本轮已有 attempt ID 时，retry 不得把它丢掉”。全平台补齐 DA 如有真实需求，
另开 issue。

## 实施拆解

| step | 内容 | 依赖 |
| --- | --- | --- |
| [01](steps/01-preserve-triggering-attempt.md) | 保存已结束 attempt 的 ID、传给 retry、补两轮关联回归并同步 C05 文档 | — |

单个 step 即可闭环，不拆 emitter、存储或 UI 子任务。

## 验收

- 确定性测试模拟连续两轮不同 attempt ID 的失败与退避；两条
  `recording.retry_scheduled.download_attempt_id` 分别等于各自触发它的 ID，不等于下一轮 ID。
- 同轮 `recording.disconnected` 与 `recording.retry_scheduled` 可按 DA 一一关联；已有
  `recording.reconnected` 新 attempt 语义不变。
- 没有 attempt ID 的入口仍保持未知，不写伪 ID，也不复用房间、场次或时间作为替代。
- `cargo test -p biliup-cli --lib`、`cargo fmt --all -- --check` 通过；契约索引校验仍通过。

## 不做

- 不新增事件字段、数据库迁移、API 或前端改动。
- 不改变重试时长、线路切换、下播确认或重连策略。
- 不生成跨平台通用 attempt ID；这是比 #28 更大的独立覆盖问题。
- 不等待真实平台断流作为合入前置；确定性回归足以证明 ID 传递，真实样本只用于合并后核对。
