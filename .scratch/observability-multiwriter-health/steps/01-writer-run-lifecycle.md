# 01 — writer 运行身份与租约生命周期

Status: ready-for-agent
Implementation: not-started

来源：[spec](../spec.md)、[GitHub Issue #27](https://github.com/dplei/biliup/issues/27)。

## 目标

用按 `process_run_id` 隔离的运行表替换全局 `dirty` 读写，并证明并发打开、关闭、重连和 stale
收割不会互相覆盖。本 step 不改 HTTP API 或前端。

## 实现范围

1. 新增 `0003_writer_runs.sql`：创建 `log_writer_run`、索引和约束；把旧 `dirty` 一次性收敛进
   `unclean_shutdowns` 后清零。
2. 让生产 SQLite factory 获得 Runtime 已生成的稳定 `process_run_id` 与 `instance_id`；保持普通
   非 SQLite consumer 的 `Runtime::start` 接口可用。
3. `SqliteStore` 保存当前 writer 身份；打开/重连幂等注册自己，先刷新自己再收割其他过期运行。
4. `write` 和 `maintain` 刷新当前心跳；60 秒为 crate 内固定租约，不新增配置。
5. `close` 只关闭当前运行；历史 stale 记录不因恢复或正常关闭而抹除。
6. 维护过程按 90 天边界、每轮最多 64 行清理已关闭的历史行，以及持续 stale 且最后心跳也已
   超期的行；不得删除已经恢复活跃的 stale 运行。
7. 移除新代码对 `log_meta.dirty` 的运行时读写，保留列以兼容旧库。

## 最小回归

- A/B 交错打开与正常关闭，累计值保持 0。
- A 过期、B 正常关闭，只累计 A；重复扫描不重复增加。
- A/B 同时过期，准确累计 2。
- 同一 `process_run_id` 重连只有一行且保留原启动时间。
- 当前运行先续租、再扫描，不会把自己收割。
- stale 运行恢复后当前心跳重新有效，历史累计不回退。
- 旧 `dirty=1` migration 只收敛一次。

测试通过可控 `now_ms` 或直接回写隔离库时间戳推进，不做真实长 sleep。

## 完成门槛

- `cargo test -p biliup-observability` 中本 step 新增回归通过。
- `cargo fmt -p biliup-observability -- --check` 通过。
- `cargo clippy -p biliup-observability --all-targets -- -D warnings` 通过。
- 在本文件追加实现 commit、测试命令与结果，将 `Status` 改为 `resolved`。

## 非目标

不改 API/页面，不增加 PID/OS 锁，不调整 busy timeout、队列、WAL 或事件保留预算。

## Comments

消费者写失败会在同一 Runtime 内重新调用 factory；稳定运行 ID 是根因修复的一部分，不是附加优化。
