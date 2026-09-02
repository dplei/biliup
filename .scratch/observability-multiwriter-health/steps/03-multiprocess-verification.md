# 03 — 多进程共享库验收与回执

Status: ready-for-agent
Blocked by: 01, 02
Implementation: not-started

来源：[spec](../spec.md)、[GitHub Issue #27](https://github.com/dplei/biliup/issues/27)。

## 目标

用隔离临时库完成真实进程边界的并发与强杀验收，更新公开契约和本 effort 回执。本 step 只修复
验收暴露的 Issue #27 直接回归，不顺手调整 SQLite 性能参数。

## 验收范围

1. 扩展或复用 observability 的测试子进程：A 长驻持有 Runtime，B 作为独立短运行打开、写入并
   正常关闭，确认 A 始终保持推定活跃且累计值不增加。
2. 强杀 A，确定性推进其心跳过期，再由 C 打开；确认只出现一个历史未知窗口和一个可见缺口语义。
3. 两个进程都写入唯一事件，查询确认 UID、事件数与 `process_run_id` 独立；核对没有 owner 逻辑
   导致的额外 storage failure/dropped。
4. 更新 `crates/biliup-observability/README.md`：支持同机多进程共享，仍禁止网络盘/多机共享；
   强杀表述改为未知窗口而非确认异常退出。
5. 更新 `CODE_INDEX.md` 中 `sqlite.rs` 的职责与 Runtime→SQLite 关系，运行索引检查。
6. 在本目录新增 `verification.md`，记录脱敏命令、结果、边界与未覆盖项。

## 完成门槛

- `cargo test -p biliup-observability` 通过，强杀只作用于测试创建的子进程和 TempDir。
- `cargo clippy -p biliup-observability --all-targets -- -D warnings` 通过。
- `python3 scripts/check_code_index.py` 与 `git diff --check` 通过。
- spec 的所有验收场景均有自动化测试或明确证据；没有真实生产标识和部署信息进入公开文件。
- 新增 `verification.md`，将本文件 `Status` 改为 `resolved`；三个 step 全部 resolved 后才进入合并与
  归档流程。

## 停止条件

若现有 50ms busy timeout 在目标并发场景下造成独立的事件丢弃，记录可复现证据并停止扩范围；
先判断是否为本修复必须解决的阻塞，否则另开 issue，不在本 step 猜测性调参。

## Comments

不使用生产事件库，不对运行中的服务制造强杀、锁库或 migration 演练。
