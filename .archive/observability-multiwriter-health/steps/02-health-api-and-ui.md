# 02 — writer 健康 API 与页面语义

Status: resolved
Blocked by: 01
Implementation: `2b28587`

来源：[spec](../spec.md)、[GitHub Issue #27](https://github.com/dplei/biliup/issues/27)。

## 目标

把运行表的当前状态作为同一查询快照返回，并把页面从“确认非正常退出”修正为“未确认正常关闭或
心跳中断”。本 step 不再修改 writer 生命周期算法。

## 实现范围

1. Repository 查询在现有只读事务内聚合：
   - `active_writer_runs`：未关闭且心跳未到期；
   - `unknown_writer_runs`：未关闭且心跳已到期；
   - `unclean_shutdowns`：兼容保留的历史首次 stale 累计。
2. `/v1/log-events` 正常、禁用和不可用响应都提供三个字段；禁用/不可用值为 0。
3. 更新前端响应类型、feed meta 与日志页文案；当前 unknown 大于 0 时明确仍存在状态未知运行。
4. 不增加运行列表端点、详情弹窗、轮询器或新的状态管理抽象。

## 最小回归

- Repository 在 A 活跃、B stale、C 已关闭时分别返回 active=1、unknown=1。
- API 序列化和 unavailable 响应字段完整。
- 现有筛选、分页和事件列表测试不受 writer health 聚合影响。
- 前端不再出现“记录到 N 次非正常退出”的错误断言。

## 完成门槛

- `cargo test -p biliup-observability` 通过。
- `cargo test -p biliup-cli --test log_events_api` 通过。
- 前端现有 lint/typecheck 命令通过；若仓库命令与预期不同，按 `package.json` 的已有脚本执行。
- 在本文件追加实现 commit、测试命令与结果，将 `Status` 改为 `resolved`。

## 非目标

不提供 OS 崩溃确认、不展示历史 run 明细、不新增健康仪表盘。

## Comments

租约超时是 C14 的未知窗口证据，不是进程死亡证明；字段兼容不能牺牲页面语义准确性。

## Answer

- Repository 在现有事件页只读事务内，以同一个租约截止时间聚合当前推定活跃与状态未知的
  writer；历史 `unclean_shutdowns` 继续同页返回。
- `/v1/log-events` 的 ready、disabled、unavailable 响应都包含三个健康字段，后两种状态统一返回
  0；没有新增端点或运行详情模型。
- 前端响应类型与 feed meta 接入两个当前计数；历史提示改为“未确认正常关闭或心跳中断”，当前
  仍有 unknown writer 时另行明确提示，不再声称确认了进程异常退出。
- 回归覆盖 A 活跃、B stale、C 已关闭的聚合结果，以及正常、禁用、不可用 API 响应；现有筛选、
  分页、导出和 SSE 用例继续通过。

## Verification

- `cargo test -p biliup-observability`：28 passed，0 failed。
- `cargo test -p biliup-cli --test log_events_api`：1 passed，0 failed。
- `node_modules/.bin/next lint`：通过（保留登录页既有 `no-img-element` warning）。
- `node_modules/.bin/tsc --noEmit`：通过。
- `cargo fmt -p biliup-observability -- --check`：通过。
- `cargo clippy -p biliup-observability --all-targets -- -D warnings`：通过。
- `python3 scripts/check_code_index.py`：通过（115 files，59 relationships）。
- `git diff --check`：通过。
