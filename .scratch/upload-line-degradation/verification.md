# 验证记录

## dev 实跑（已完成）

本地 `biliup server`（release 构建）+ 本地 sqlite + 仅自己可见的投稿模板，走**手动补传**入口
（`POST /v1/uploads/missing/{id}/retry`，显式指定线路），因此走的是带 watchdog 的
`upload_enrolled_with_watchdog` 那条真实路径，不是 mock。

### 两处替代手段，以及为什么等价

**没有用 `pf`/`dnctl` 限速，改为抬高基线。** 判据是纯比值（`mbps < baseline / SLOW_RATIO`），
把基线抬到实测的 ~11.6 倍，与把带宽压到 1/11.6 对判据完全等价，且不需要 sudo、不影响本机
其它流量。做法是往 `upload_line_health` 里给一条不参与本次上传的线路写 `avg_mbps = 40`，
基线取全库 `MAX(avg_mbps)`，于是判慢门槛 = 10 MB/s，而本机实测 3.4 MB/s。

**关掉了响度标准化。** 判据只度量网络阶段（`instant` 起点在预处理和全局 permit 之后），
预处理开多久都不进入判据；关掉它只是省掉每次几分钟的无关 CPU 时间。

本机实测上行 **3.42–3.49 MB/s**（≈28 Mbps），与「上行 30 Mbps」一致，全程稳定。

### 结果

| # | 场景 | 期望 | 实测 |
| --- | --- | --- | --- |
| A | 冷启动，138 MB | 只写 EWMA，不判慢 | 40.20s / 3.44 MB/s，`avg_mbps=3.4357`，`cooldown_until` 为空 ✅ |
| B | 基线 40，138 MB | 判慢，30 分钟冷却 | 冷却 +30min、`last_failure_kind=slow_throughput`、`last_error="throughput 3.49 MB/s < baseline 40.00/4 MB/s"`、`consecutive_failures` 仍为 0、EWMA 3.4357→3.451 ✅ |
| C | 基线 40，950 MB | 前半段中止 | 传输第 90 秒整中止：`watchdog="slow_transfer" window_secs=90 window_mbps=3.48 baseline_mbps=40 uploaded_bytes=314572800 total_bytes=949737600`（33%）✅ |
| D | 基线 40，497 MB | 过半后不中止 | 同样 3.48 MB/s 一路爬完，142.94s 传完，**没有**第二条 `slow_transfer` ✅ |
| E | 三条可取回线路只剩一条 | 安全阀只 warn | `线路吞吐劣化，但冷却它会让可取回线路全部挂起，本次只更新均值 line="bda2" mbps=3.418 baseline=40`，`cooldown_until` 为空，EWMA 照常 3.455→3.4439 ✅ |

C 与 D 是同一组参数下只改文件大小得到的对照：950 MB 在第 90 秒时才传到 33%，中止；
497 MB 在第 90 秒时已到 ~62%，判据让路，忍完剩下的 38%。**「只在前半段止损」这条不等式
在真实链路上按设计生效。**

C 的失败出口也确认走通：`upload line failure recorded kind="request_timeout"
error="slow_transfer" cooldown_remaining_secs=60`，行上落了 `last_chunk_error`
（`slow_transfer: phase=transferring line=... chunk=29 acknowledged_bytes=...`），
`upload attempt ended outcome="failed"`。

B 的结果同步出现在 `GET /v1/health/upload-lines` 的 `last_failure_kind` 与 `avg_mbps`
字段上——页面「下一条线路」列复用同一份数据，无需另改前端。

本次 6 次上传全部只落到 UPOS，会话因为存在未成功分段而始终被投稿闸门挡住
（`aid` 为空、`submit_attempts=0`），**没有产生任何稿件**。跑完已清理全部临时行与临时配置。

### 一个手工塞库的坑（不是产品缺陷）

第一次跑 E 时安全阀没生效。原因是我用 `datetime('now','+30 minute')` 手写 `cooldown_until`，
sqlite 存成 `2026-…-… 07:19:35`（空格分隔、无时区），而 sqlx 绑定的 `DateTime<Utc>` 是
`2026-…-…T06:50:15.759382+00:00`。SQLite 按 TEXT 字典序比较，空格（0x20）小于 `T`（0x54），
于是 `cooldown_until > ?` 把这条本该在冷却中的行判成了没冷却。**手工往这张表塞时间必须用
RFC3339**；程序自己写的行没有这个问题（全部经 sqlx 绑定）。改用 RFC3339 重跑即通过。

## 未覆盖

**中止后的「重试换线」没在 dev 复现。** 本机上行 30 Mbps，AUTO 那套 4×10MB/4s 的探测门槛
必然全失败，所以 dev 配置把线路钉死在一条；而显式指定的线路本来就不受冷却影响
（「主人点名要哪条就用哪条」），换线因此无从触发。换线走的是 selection 里既有的
`active_cooldowns` → `excluded` 通道，本次改动没有碰它——中止时写进去的那条失败记录，
和任何一次网络失败写进去的没有区别。

## 阈值校准

初值全部保持不变（`SLOW_RATIO = 4.0`、`SLOW_COOLDOWN = 30min`、`SLOW_WINDOW = 90s`、
前半段保护线 50%）。dev 这一轮只能证明判据在人为拉开的差距上按设计工作，**证不了它在真实
抖动上会不会误伤**——那需要生产一周的分布。待观察项见
[`steps/03-verify-and-calibrate.md`](./steps/03-verify-and-calibrate.md) 的「生产观察」节。
