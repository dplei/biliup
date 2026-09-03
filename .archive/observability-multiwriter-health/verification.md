# 多进程 writer 健康验收

日期：2026-09-02

## 结论

隔离 TempDir 中的真实子进程证明：同机长驻 Runtime 与短命令可以共享同一 observability SQLite，
短 writer 的打开、写入和正常关闭不会覆盖长驻 writer；强杀后只报告未确认正常关闭/心跳中断的
未知窗口。现有 50ms busy timeout 在目标场景中没有产生额外丢弃或 storage failure。

## 验收矩阵

| Spec 场景 | 自动化证据 | 结果 |
| --- | --- | --- |
| 1. A/B 交错正常关闭 | `writer_runs_isolate_close_and_reconnect_identity` | 累计 0 |
| 2. 单个过期、正常 writer 不受影响 | `expired_writer_reaping_is_once_only_and_recovery_stays_visible` | 只累计过期运行一次 |
| 3. 两个过期运行 | `multiple_expired_writers_and_self_renewal_are_deterministic` | 准确累计 2，重复维护不增加 |
| 4. 长驻 A 与独立短命令 B | `separate_process_writer_does_not_disturb_resident_writer` | A 始终活跃，累计 0 |
| 5. 同一 Runtime 存储重连 | `busy_lock_is_bounded_and_recovery_gap_is_visible` | 仅一个运行行，不误累计 |
| 6. stale 后恢复 | `expired_writer_reaping_is_once_only_and_recovery_stays_visible` | 重新活跃，历史累计保留 |
| 7. 两进程并发写入 | `separate_process_writer_does_not_disturb_resident_writer` | 两个唯一 UID/运行 ID，无额外丢弃或存储失败 |
| 8. 真实子进程强杀 | `force_kill_retains_commit_and_reports_unclean_window` | 已提交可查，未提交不出现，unknown/累计各 1 |
| 9. 旧 `dirty=1` 升级 | `legacy_dirty_migration_is_consumed_once` | 只收敛一次 |
| 10. API 与页面语义 | `query_export_and_unavailable_states_are_distinguishable`、前端 lint/typecheck | 使用“未确认正常关闭或心跳中断” |

## 命令与结果

```sh
cargo test -p biliup-observability
cargo clippy -p biliup-observability --all-targets -- -D warnings
python3 scripts/check_code_index.py
git diff --check
```

结果：以上命令通过。进程测试只启动并终止测试二进制，数据库全部位于 `tempfile::TempDir`；未读取
或写入生产事件库、账号、业务库与部署配置。

## 支持边界

- 支持同一台主机上的多个 Runtime/进程，以 SQLite WAL 和短写事务共享事件库。
- 不支持网络文件系统、多台主机共享或新旧版本 writer 混跑。
- 60 秒租约过期只说明未确认正常关闭或心跳中断，不证明 OS 进程已经崩溃。
- 普通事件队列仍不承诺强杀或掉电时零丢失；需要业务级保证时应使用 durable outbox。
