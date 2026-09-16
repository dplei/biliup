# 上传线路探测适配中小带宽

来源：[issue #58](https://github.com/dplei/biliup/issues/58)

## 根因

1. `preupload?r=probe` 当前返回 `probe.post = 0.1`（单位 MB），但
   `Probe::ping` 无视服务端提示，固定为每条线路发送 10 MB。四路并发时，一个探测波次会争抢
   40 MB 出向带宽，4 秒单线超时因此会把可用线路误判为失败。
2. AUTO 探测失败通过 `record_line_probe_failure` 以普通 `transport` 失败写入
   `upload_line_health`。显式配置和真实传输共用这张熔断表，无法区分「探测误判」与「这条线路
   实际上传失败」，所以用户改成显式线路后仍会被冷却挡回 AUTO。

Issue 后续实测又确认第三个独立根因：数据库迁移到出口能力不同的机器后，持久化的
`avg_mbps` 仍代表旧机器，`slow_throughput` 会把新机器上的正常成功传输误判为线路劣化。
该项不混入已合并的探测修复，见 [step 02](steps/02-reset-throughput-baseline.md)。

## 最小方案

- POST 探测体按服务端 `probe.post` 生成；缺失或非法时使用 0.1 MB，远端异常大值最多沿用旧版
  的 10 MB 上限，避免不受控分配内存。
- 保留现有并发探测和按 `line.cost` 选最快语义。服务端建议体积把四路并发总量从 40 MB 降到
  约 0.4 MB，已消除本题的带宽争抢，不再另造串行调度。
- 给探测失败单独记 `probe_failure`。AUTO 仍跳过它，显式配置或手动指定的线路只忽略这种冷却；
  真实传输产生的 TLS、超时、慢传输等冷却仍然生效。

## 不做

- 不新增配置项：探测体大小已有服务端权威值。
- 不改数据库结构：现有 `last_failure_kind` 足够表达来源。
- 不按探测速度推算上传超时：本题在请求发出前已经由错误体积造成，修根因即可。

## 验收

- 单测锁住服务端 0.1 MB 提示、非法值回退和 10 MB 上限。
- 单测锁住显式线路忽略 `probe_failure`，同时保留真实传输冷却回退。
- `cargo test -p biliup` 与相关 `biliup-cli` 测试通过。

## 后续步骤

- [01](steps/01-fix-probe-and-explicit-line.md)：探测体积与探测失败冷却隔离，已合入 `dev`，待真实链路验收。
- [02](steps/02-reset-throughput-baseline.md)：消除跨机器持久基线误判，待实现。
