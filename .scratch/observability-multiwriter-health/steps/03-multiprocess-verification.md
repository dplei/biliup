# 03 — 多进程共享库验收与回执

Status: resolved
Blocked by: 01, 02
Implementation: `d72555b`

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

## Answer

- 复用 observability 测试二进制增加真实 A/B 子进程验收：A 长驻、B 独立打开并写入后正常关闭；
  查询确认两个唯一 UID 与 `process_run_id`、A 仍活跃、unknown/累计为 0，两个进程均无额外
  dropped 或 storage failure。
- 增强既有 TempDir 强杀测试：确定性推进 A 心跳过期后由 C 打开，精确确认已提交事件保留、
  未提交事件不出现、历史未知窗口为 1，当前 unknown 为 1。
- README 已改为支持同机多进程 WAL 共享、禁止网络盘/多机/新旧 writer 混跑，并把强杀语义修正
  为“未确认正常关闭或心跳中断”；没有调整 busy timeout、WAL 或保留参数。
- [验收回执](../verification.md) 映射 spec 的 10 个场景，全部由自动化测试或既有前端检查覆盖；
  所有进程与数据库均为合成测试资源。

## Verification

- `cargo test -p biliup-observability`：30 passed，0 failed。
- `cargo clippy -p biliup-observability --all-targets -- -D warnings`：通过。
- `cargo fmt -p biliup-observability -- --check`：通过。
- `python3 scripts/check_code_index.py`：通过（116 files，59 relationships）。
- `git diff --check`：通过。
