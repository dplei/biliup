# 上传分段可靠性修复：任务总览

Status: ready-for-agent

## 背景与结论

2026-08-25 22:29 至 2026-08-26 清晨，抖音主播“帝骑哥”的 30 分钟 FLV 切片暴露出上传会话、缺失补传和投稿完整性之间的多处断点。

当前 `dev` 已包含 `1.2.2-uploadconcurrent` 的修复：上传 Actor 不再被一场长直播独占，上传管道启动前会创建本地 session，上传前会建立 pending 记录。但现有实现仍不能完整满足事故要求：

- `validated media segment` 与数据库 enrollment 之间仍经过内存 channel，不是同一个 durable 边界；
- 投稿前不检查 `pending/uploading/failed`，卡住的 uploading 还会被自动恢复查询忽略；
- 上传没有无进度和总时长 watchdog，手动重试也不会可靠取消旧请求；
- bldsa 的证书失败只会在单次自动探测中被跳过，没有持久熔断；
- 正常上传成功后会删除 missing 行，丢失源分段的永久幂等身份；
- 补扫和未绑定恢复可能在 finalized 会话之后创建新 session。

## 必须保持的不变量

1. 媒体被确认有效后，必须先得到数据库记录或 fsync outbox 记录，之后才能进入内存上传队列。
2. 一个本地逻辑分段在同一主播下只能拥有一个生命周期记录，重复事件只能复用该记录。
3. 远端上传结果、分段 succeeded 状态和 session `videos_json` 必须在同一事务提交。
4. session 存在任何未成功分段时，不得调用 B 站投稿接口。
5. 同一 missing 行同一时刻最多有一个有效 attempt；旧 attempt 不得覆盖新 attempt。
6. finalized session 不得因补扫、重试或迟到事件创建新的待提交 session。
7. 线路 TLS 校验保持开启；单线路证书故障通过熔断和换线处理。
8. 源文件不存在时停止自动重试，但保留可审计的终态记录。

## 已锁定默认值

- 投稿策略：严格完整性闸门，不允许 incomplete 投稿。
- 无进度超时：连续 5 分钟没有完成新的上传分块。
- 单文件总时长超时：2 小时。
- 补传线路顺序：`bda2 -> tx -> auto`。
- TLS 证书错误线路冷却：24 小时。
- 上传进度落库节流：最多每 5 秒或每增加 16 MiB 写一次，以先满足者为准。
- `source_missing` 是可见、不可自动重试的终态。

## 子任务与依赖

| 编号 | 子任务 | 状态 | 依赖 | 可并行关系 |
|---|---|---|---|---|
| 01 | 事故 fixture 与不变量测试 | ✅ 已完成（`681b5b5`） | 无 | 先行 |
| 02 | 有效分段 durable enrollment | ✅ 已完成（`62efaca`） | 01 | 为后续状态模型提供基础 |
| 03 | watchdog、取消与 attempt lease | ✅ 已完成（`d009e40`） | 01、02 | 可与 04 部分并行 |
| 04 | 上传线路健康与 TLS 熔断 | ✅ 已完成（`a738e6b`） | 01、02 | 已与 03 attempt 状态机集成 |
| 05 | session 完整性与分 P 排序 | ✅ 已完成（`17ff807`） | 01、02 | 已统一正常下播与重启补提交闸门 |
| 06 | 补传幂等与 finalized 防护 | ✅ 已完成（`8f1df43`） | 02、05 | 已与 03/04 的重试行为集成 |
| 07 | 迁移、回填与事故数据恢复 | ✅ 已完成（`d7fe4ee`、`6054a96`；#227 只读审计见 `c934ba0`） | 02、05、06 | 所有数据语义确定后执行 |
| 08 | 验证、灰度与可观测性 | 🟡 部分完成（第 1–4 节，`14864d0`）；第 5–8 节待人工执行 | 01–07 | 最后执行 |

## 下一任务建议

08 号任务的第 1–4 节（静态/单元验证、端到端故障矩阵覆盖、结构化日志、API/页面验收）已由 agent
完成，见 [`08-verification-rollout-and-observability.md`](./08-verification-rollout-and-observability.md)。
第 5–8 节（测试主播灰度、amd64 buildx 构建、生产部署、7 天观察、回滚）是人工运维步骤，需要仓库
主人在有真实直播和生产机器权限的环境里手动执行，agent 不会尝试代跑。

任务 07 的 backfill 只在遇到无法读取的历史分段快照时才会拒绝执行（`6054a96`），避免用错误的
identity 静默合并/覆盖旧数据；会话 #227 的只读采集清单与审计报告骨架已产出
（`.scratch/2026-08-26-session-227-audit/`），采集结果仍需人工在生产库上跑清单里的 SQL/curl 命令
后回填。

08 号任务第 1 节复查时发现并修复了一个真实的编译回归：`crates/stream-gears/src/server.rs`
（生产实际运行的 CLI 分发路径，`cargo check -p biliup-cli` 看不到它）没有处理 07 号任务新增的
`Commands::BackfillLifecycle`，`cargo check --workspace` 会失败。第 3/4 节复查还补上了几处此前
完全没有日志的环节（enrollment 成功路径、attempt_token 生命周期、watchdog 触发、每线路失败分
类），并修了一处 `upload_missing_segment.last_error` 未脱敏就写库/展示的问题，统一复用已有的
`sanitize_error`。

01 的六条 `target_*` 契约测试均已由生产实现兑现，不再有 `#[ignore]` 占位。

## 实施边界

- 不修改已经应用的 1–10 号 migration 文件字节。
- 不关闭 Rustls/reqwest 的证书校验。
- 不通过硬编码域名替换伪造 B 站上传线路。
- 不自动编辑会话 #227 或其他已投稿生产稿件；先生成只读审计和人工确认清单。
- 不将本仓库 `data/data.sqlite3` 当作生产库。

## 整体验收

- 每条成功验证的有效分段均能通过路径或 enrollment id 查到持久记录。
- 任一 active missing 存在时，session 的 B 站 submit 调用次数为 0。
- 卡住的上传会在 5 分钟无进度或 2 小时总时长后释放资源并换线。
- 重复事件、补扫和并发重试不会产生重复分 P。
- bldsa 证书过期不会影响 bda2、tx 或 auto 的继续上传。
- finalized session 不会生成新的补传 session。
