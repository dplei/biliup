# 08 — 补传控制页的线路选择与任务控制

Status: resolved
Blocked by: 02, 04
Model: Sonnet 5 —— 前端交互 + 一个新接口 + 一处数据留存设计，模式常规；依赖项落地后这里没有
需要深推理的地方。

## 背景

对应评估报告 F。页面能看不能管：卡住的任务只能「重新补投」，不能停；不能指定线路；看不到
线路切换历史；`bldsa` 的证书熔断在本页不可见。

## 现状

[`app/(app)/missing/page.tsx`](app/(app)/missing/page.tsx) 已有进度百分比、`current_line`、
无进度秒数、开始时间、下次线路与跳过原因、会话完整性横幅。缺的是：

- 没有线路选择器，`recover` / `retry` 接口也不接受任何线路参数；
- `uploading` 行只有「重新补投」（走 `retry` → 取消旧 attempt → 立刻重传），没有「只停止、
  不重传」，无法先释放一个卡住的任务再决定怎么办；
- 没有线路切换历史（`current_line` 只有当前值，`line_index` 只是失败计数）；
- `/v1/health/upload-lines`（[router.rs:74](crates/biliup-cli/src/server/router.rs:74)）已存在但
  本页没消费。

## 改动范围

**接口层**
- `recover` / `retry` 接受可选 `line` 参数并严格遵从，对接 04 的统一决策函数。
- 新增「停止当前 attempt」接口：内部走
  [`cancel_registered_attempt`](crates/biliup-cli/src/server/common/upload.rs:1745) +
  `fail_enrolled_attempt`，终态为 `failed` 而不是继续 `uploading`；停止不等于重试，不得自动
  起下一次。

**数据层（已决策：建表）**
- 新建 `upload_attempt` 表，一次 attempt 一行，写一张新 migration（沿用现有编号序列，
  不改已应用文件字节）。建议字段：
  - `id`、`missing_id`、`attempt_token`
  - `line_key`、`line_source`（配置 / 手动指定 / 回退，对接 04 的决策原因）
  - `started_at`、`ended_at`
  - `phase_reached`（`preprocessing` / `queued` / `transferring`，对接 01 的阶段）
  - `outcome`（`succeeded` / `failed` / `cancelled` / `stale`）
  - `uploaded_bytes`、`last_chunk_index`（对接 05 的诊断字段）
  - `error`（走已有的 `sanitize_error` 脱敏）
- 写入点与 01 的阶段流转、`fail_enrolled_attempt`、`persist_segment` 对齐，保证每次 attempt
  有且只有一行、且必有终态。
- 需要一条按 `missing_id` 查询的索引；同时定一个保留策略（例如只保留每行最近 N 次或 N 天），
  避免长期运行后表无限增长。

**页面**
- 每行加线路下拉（默认「跟随配置」）+「停止」按钮 +「换线重试」。
- 顶部展示线路健康横幅，含 `bldsa` 冷却剩余时间。
- 错误提示走 03 的统一映射。

## 验收

- 能为单个任务指定线路，并在日志与页面同时看到该线路生效。
- 点「停止」后任务不再是 `uploading`，且没有自动重传。
- `bldsa` 的熔断状态与剩余冷却在本页可见。
- 切换历史能看出「这次任务先后用过哪些线路、各自为何结束」。

## 备注

attempt 历史采用建表方案（主人 2026-08-27 定：更工程）。表结构见「数据层」。
05 的诊断字段与 01 的 migration 若尚未合并落地，本表的相关列可先留空，但结构一次到位。

## Answer

已实现（`upload_attempt` 表并入 migration 16）。

接口层：
- `recover` / `retry` 接受可选 `{"line": "..."}`，直通 04 的决策函数（手动指定与配置线路同级优先，
  仅在该线路冷却时回退，回退原因随响应返回）。
- 新增 `POST /v1/uploads/missing/{id}/stop`：`cancel_registered_attempt` + `fail_enrolled_attempt`，
  终态 `failed` 且保留正常退避——停止不等于重试，不会自动起下一次。上一次上传没能在等待窗口内退出时
  返回 409 而不是强行释放。
- 新增 `GET /v1/uploads/missing/{id}/attempts` 读取 attempt 历史。

数据层：`upload_attempt` 一次 attempt 一行，字段为
`missing_id / attempt_token / line_key / line_source / started_at / ended_at / phase_reached /
outcome(succeeded|failed|cancelled|stale) / uploaded_bytes / last_chunk_index / error`；
写入点与 claim、阶段流转、`fail_enrolled_attempt`、`persist_segment` 对齐，必有终态。
索引 `(missing_id, id desc)`，保留策略为每行最近 20 次（插入时顺带裁剪）。

页面：
- 每行「线路」列带下拉（默认「跟随配置」，冷却中的线路带标注）、「停止」「换线重投」按钮；
- 顶部线路健康横幅，列出冷却中的线路、失败类型、连续失败次数与剩余冷却时间（`bldsa` 熔断在本页可见）；
- 展开任意一行显示 attempt 历史（哪条线、何种来源、止步于哪个阶段、为何结束、已确认字节、分块号）；
- 「上传进度」列区分阶段：转码/排队阶段显示阶段名和已耗时，不再显示会误导人的「已无进度 N 秒」；
- 错误提示走 03 的统一映射。
