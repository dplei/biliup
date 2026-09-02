# Spec：多进程共享事件库的 writer 运行健康

状态：已实现，待合并

来源：[GitHub Issue #27](https://github.com/dplei/biliup/issues/27)

分支：`fix/issue27-260902-193442`

日期：2026-09-02

## 1. 决策摘要

同一台主机上的主服务和短命令继续写入同一个 observability SQLite；不通过禁用短命令采集或拆分
数据库绕开问题。

删除 `log_meta.dirty` 的运行时职责，改用按 `process_run_id` 维护的 writer 运行表：每个 Runtime
只注册、续租和关闭自己的行。租约过期只表示“未确认正常关闭/心跳中断”，不能宣称已经证明
操作系统进程崩溃。

首版复用现有 Runtime、SQLite WAL、维护循环和 `process_run_id`，不增加守护进程、进程探针、
新依赖或用户配置。

## 2. 背景与根因

当前 `SqliteStore::open` 执行：

```sql
UPDATE log_meta
SET unclean_shutdowns = unclean_shutdowns + dirty,
    dirty = 1
WHERE singleton = 1
```

任意 writer 正常关闭又会执行：

```sql
UPDATE log_meta SET dirty = 0 WHERE singleton = 1
```

这个单例布尔值只能表达一个 writer。A 仍运行时 B 打开会把 A 误计为异常；B 关闭又会把 A 的
运行状态清掉，随后 A 被强杀时可能漏报。

另有一个必须纳入设计的重连路径：`Runtime` 的消费者写失败后会丢弃当前 consumer，并在同一个
逻辑 Runtime 内再次调用 factory。一次 Runtime 的多次 `SqliteStore::open` 必须复用同一个
`process_run_id`，否则普通存储恢复也会被拆成多个运行并产生误报。

现有 README 写着“同一数据库只由一个 Runtime 所有者运行”，与当前全入口共享同一路径的实际
用法冲突。本任务把支持边界明确为“同机、多进程、短写事务”；网络文件系统和多机共享仍不支持。

## 3. 目标与非目标

### 3.1 目标

1. A、B 并发打开同一事件库时，各自的启动、心跳和关闭互不覆盖。
2. 同一 Runtime 因存储错误重连时幂等复用自己的运行记录。
3. 超过租约仍未正常关闭的运行只累计一次未知窗口。
4. 查询 API 分开返回当前推定活跃 writer、当前状态未知 writer 和历史未知窗口累计值。
5. 旧库加法迁移，保留事件、附件、保留水位与原累计值。
6. 通过确定性并发、重连和强杀测试证明修复，不依赖真实生产库或长时间 sleep。

### 3.2 非目标

- 不判断进程在 OS 层是否已经死亡，不读取 PID、`/proc` 或启动时间。
- 不引入文件锁、分布式锁、独立协调服务或新依赖。
- 不支持网络盘或多台主机共享事件库。
- 不把普通事件队列升级成业务审计账本，也不承诺强杀时零丢失。
- 不借本任务调整 SQLite busy timeout、批大小、WAL 预算或保留策略；并发测试发现独立问题时
  另开 issue。
- 不新增可配置的心跳/租约参数；首版使用 crate 内常量。

## 4. 持久化设计

新增 observability migration `0003_writer_runs.sql`：

```text
log_writer_run
├── process_run_id       TEXT PRIMARY KEY
├── instance_id          TEXT NOT NULL
├── started_at_ms        INTEGER NOT NULL
├── heartbeat_at_ms      INTEGER NOT NULL
├── closed_at_ms         INTEGER NULL
└── stale_detected_at_ms INTEGER NULL
```

约束与索引：

- 时间均为 Unix 毫秒；`process_run_id`、`instance_id` 继续服从现有身份长度与字符校验。
- `closed_at_ms >= started_at_ms`；`heartbeat_at_ms >= started_at_ms`。
- 索引覆盖 `(closed_at_ms, heartbeat_at_ms)`，供活动状态和到期扫描使用。
- 已正常关闭的历史行，以及持续 stale 且最后心跳已超过 90 天的行有界清理，每轮最多 64 行；
  已恢复活跃的 stale 历史行不能被误删。`log_meta.unclean_shutdowns` 保留累计值。

迁移时执行一次：

```sql
UPDATE log_meta
SET unclean_shutdowns = unclean_shutdowns + dirty,
    dirty = 0
WHERE singleton = 1;
```

它把旧版本遗留的单个 dirty 状态保守收敛成一个旧版未知窗口。迁移后不删除 `dirty` 列，旧列只作
兼容遗留，不再由新代码读写。

## 5. 生命周期与并发语义

### 5.1 稳定身份

`Runtime` 仍是 `process_run_id` 的唯一生成者。实现需要让生产 SQLite factory 获得 Runtime 已生成
的 ID，同时保持现有普通 `Runtime::start` 调用可用；不得让 `SqliteStore::open` 自行生成运行 ID。

同一 Runtime 的 factory 重试、consumer 重建和最终 close 都携带同一个 `process_run_id`。

### 5.2 打开与重连

打开事务按以下顺序执行：

1. 插入当前运行；主键已存在时只刷新自己的 `heartbeat_at_ms`，保持原 `started_at_ms`。
2. 主键已存在但 `closed_at_ms` 非空时拒绝重开；Runtime ID 不应在正常关闭后复用。
   `stale_detected_at_ms` 不清零，历史心跳中断仍保留。
3. 扫描其他运行的到期 owner，并原子标记 `stale_detected_at_ms`。
4. 用本次实际标记的行数增加 `unclean_shutdowns`，重复扫描不得重复累计。

先刷新并排除自己可避免同一 Runtime 重连，或机器长暂停后自己先恢复时，把自己误收割。

### 5.3 心跳

- 沿用 worker 约 1 秒一次的空闲维护循环；每次 `maintain` 刷新当前运行。
- 每次事件写事务也刷新当前运行，持续有流量时不依赖空闲维护。
- 固定租约为 60 秒。它远大于正常维护间隔，但仍只代表应用层租约，不是进程死亡证明。
- 到期扫描放在 writer 的打开/维护路径；只读 Repository 不修改运行状态。

### 5.4 正常关闭与恢复

正常关闭只更新：

```text
WHERE process_run_id = 当前 Runtime
```

关闭其他 writer、清空全局状态或删除所有 owner 都是错误。若一个已被标记 stale 的运行后来恢复，
它可以继续刷新心跳并重新进入当前活跃集合；历史 stale 计数不回退，因为那段可观测性窗口确实
无法确认完整。

### 5.5 状态定义

以查询时刻 `now` 和固定租约计算：

| 状态 | 判据 | 对外含义 |
| --- | --- | --- |
| 推定活跃 | `closed_at_ms IS NULL` 且心跳未到期 | 最近收到该 writer 心跳 |
| 状态未知 | `closed_at_ms IS NULL` 且心跳已到期 | 未确认正常关闭，相关窗口可能缺事件 |
| 正常关闭 | `closed_at_ms IS NOT NULL` | 该 Runtime 执行了正常 close |

不提供“已确认 OS 崩溃”状态。若未来确实需要该保证，另行设计 OS 级 owner lock；不能把租约超时
改名冒充确认结果。

## 6. 查询 API 与页面语义

保留现有 `unclean_shutdowns` 字段以避免不必要的破坏性 API 变更，但重新明确它是“历史上首次
检测到的未关闭/心跳中断运行数”。同一查询快照新增：

```text
active_writer_runs   当前推定活跃运行数
unknown_writer_runs  当前心跳到期且未正常关闭运行数
```

Repository 在读取 `log_meta` 的同一个只读事务中聚合这两个当前值，避免事件页与健康数字来自不同
快照。禁用/不可用响应返回 0，保持现有响应形状可直接消费。

页面不再显示“记录到 N 次非正常退出”，改为：

> 事件库曾检测到 N 个未确认正常关闭或心跳中断的运行，相关时段可能有事件未写完。

当 `unknown_writer_runs > 0` 时额外说明当前仍有状态未知的 writer。活跃数只进入健康信息，不新增
常驻卡片或运行详情页。

## 7. 失败与边界处理

- 注册、到期标记和累计计数必须在同一 SQLite 写事务内；busy/rollback 后整组操作重试，不能出现
  “已累计但未标记”或相反状态。
- 当前 writer 心跳失败沿用现有 storage health 降级路径，不另写递归事件。
- `close` 的 checkpoint 即使失败，也不能关闭其他 owner。当前 owner 的 close 与 checkpoint
  结果应如实进入现有 storage failure 统计。
- 多进程短事务仍由 SQLite WAL 串行化。此次只验证现有并发预算能通过目标场景，不预先调大超时。
- 旧二进制仍会写 `dirty`，因此部署按现有单机重启流程切换，不宣称新旧 writer 混跑安全。

## 8. 验收场景

1. A 打开，B 打开，B 正常关闭，A 正常关闭：累计未知窗口为 0。
2. A、B 打开，B 正常关闭；把 A 心跳置为过期后 C 打开：只累计 A 一次。
3. A、B 都未关闭且心跳过期，C 打开：累计 2；C 重复维护不再增加。
4. A 活跃时短命令 B 打开和关闭，A 始终保持推定活跃。
5. 同一 `process_run_id` 因写失败重连：只有一行，`started_at_ms` 不变且不累计未知窗口。
6. 已 stale 的 A 恢复心跳：当前重新归入活跃，历史累计不回退。
7. 两个 Runtime 并发写事件：UID 幂等、事件数正确，没有由 owner 逻辑造成的额外丢弃。
8. 真实子进程强杀：已提交事件仍可查；租约到期后显示一个未知窗口，未提交范围仍保持未知。
9. 旧库 `dirty=1` 升级：只收敛一次；再次打开不重复累计。
10. API 和页面使用“未确认正常关闭/心跳中断”文案，不声称确认进程崩溃。

测试通过显式回写心跳时间或注入 `now` 推进时间，不用 60 秒真实 sleep。

## 9. 实现步骤

| Step | 范围 | 前置 | 状态 |
| --- | --- | --- | --- |
| [01](steps/01-writer-run-lifecycle.md) | 稳定运行身份、migration、writer 注册/心跳/关闭/收割及存储回归 | — | resolved |
| [02](steps/02-health-api-and-ui.md) | Repository/API 当前状态投影、前端类型与准确文案 | 01 | resolved |
| [03](steps/03-multiprocess-verification.md) | 跨进程/强杀验收、文档、索引与回执 | 01、02 | resolved |

每轮只完成一个 step，回写对应文件后提交并停下。

## 10. 回退

这是加法 migration。代码回退前必须停止所有新 writer；旧二进制会忽略新表并重新使用 `dirty`，
因此只允许作为短期单 writer 回退，不允许新旧版本并发写同一库。新表不删除，事件与附件不受影响。
