# 上传线路劣化的感知与降级

对应 [issue #17](https://github.com/dplei/biliup/issues/17)。发起时版本 1.3.11，issue 提出时 1.3.3——
期间三个缺口一个都没被动过，唯一改过选路的 `57ab74b` 是为「原片能取回」加白名单，与本题无关。

## 问题

线路劣化没有任何机制把它降级。实测同一条线路在同一晚既能跑 25.78 MB/s（当晚中位数），也能
跑出 8.55 / 2.29 MB/s，而后者两次都**以成功告终**——`record_success` 把失败计数清零，下一次
选择照样选它。因为网络上传由容量 1 的 `GLOBAL_UPLOAD_SEMAPHORE` 串行化，一条慢线路就让整条
流水线停摆，出口带宽约 91% 空转，积压直接转成录像盘占用。

三个缺口（本次全部落在 `crates/biliup-cli/src/server/common/`）：

| # | 位置 | 现状 |
|---|---|---|
| 1 | `upload.rs:122` / `attempt_lease.rs:62` | watchdog 只有「零进度 5 分钟」和「总时长 2 小时」。2.29 MB/s 每 4.6 秒确认一个 10.5 MB 分片，两个计时器都够不着 |
| 2 | `upload_line_health.rs:230` | `record_success(pool, line_key)` 无吞吐参数，表里也无吞吐列，「传完了」与「传得好」不分 |
| 3 | `upload_line_selection.rs:258` | `resolve_planned_line` 依据 probe 的握手 RTT。RTT 快 ≠ 持续吞吐高 |

## 方案的形状

**不新建降级机制，复用已有的 `cooldown_until`。** 选路的排除通道只有一条——
`active_cooldowns` → `excluded` → `Probe::probe_filtered_with_failures`，冷却写进去，选路与页面
两侧自动同步。慢线路只是「暂时别选它」，这正是 cooldown 的语义。

因此 issue 建议 3（选择时按吞吐排序）**不单独实现**：那需要改 `crates/biliup` 的 probe 返回全部
候选再重排，而在「只有 3 条 recoverable 线路、probe 只回最快一条」的现状下，一次短冷却达到的
效果一样。等实测证明不够再说。

issue 建议 4（中止后复用已传分片）**不做**，理由是实测过的事实：`Parcel::upload_with_observer`
消费 `self`，`parts` 只在内存累积，`persist_attempt_progress` 记的是诊断字节数不是续传状态——
没有任何断点续传能力。中止 = 已传字节全部作废。所以代价靠**判据保守**来控制，不靠工程：
只在传输前半段中止（见 step 02），后半段忍完比重来便宜。

## 阈值怎么定：自校准，不写死带宽

判「慢」的基线取**全库 `avg_mbps` 的最大值**——即「这台机器见过的最好线路能跑多少」，本次
低于它的 1/4 就算劣化。这样阈值随机器出口带宽自己长出来，公开仓库里别人的 100 Mbit 或
1 Gbit 机器都不用改代码。没有任何历史样本时（冷启动、新库）判据整个关闭，只积累不判定。

## 安全阀

降级会减少候选。`RECOVERABLE_LINES` 只有三条（`bda2` / `tx` / `alia`），全被冷却就会走
`resolve_planned_line` 里那条「放开限制重探」的兜底，可能落到没有取回通道的线路上。所以
**慢降级前先查一次：如果这次冷却会让 recoverable 线路全空，就只 warn 不冷却。** 真失败的
`record_failure` 不受此限——传不上去比失去取回通道严重，那条路径的取舍已经定过了。

## 拆解

| step | 内容 | 依赖 |
|---|---|---|
| [01](steps/01-throughput-in-line-health.md) | 吞吐入库 + 慢即短冷却（缺口 2、3） | — |
| [02](steps/02-slow-transfer-watchdog.md) | Transferring 阶段滑窗速率判据，持续劣化就中止（缺口 1） | 01 的基线列 |
| [03](steps/03-verify-and-calibrate.md) | dev 环境实跑 + 生产日志校准阈值 | 01、02 |

step 01 单独上线就已经能让「慢完一次，接下来半小时不选它」成立；02 是把「这一次就别等它爬完」
补上。两步都不改上传协议，回滚只需回滚 migration 之后的代码路径。

## 不做

- 提高上传并发绕过：出口 200 Mbit 已被单线路跑满（峰值 27.53 MB/s ≈ 220 Mbit），并发只切分同一份
  吞吐，还抬高 601 风险。issue 里已定论。
- 跨线路吞吐排行榜 / 独立的吞吐历史表：一列 EWMA 够用，多的等有人要看再加。
