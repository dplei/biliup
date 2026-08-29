# 04 — 统一上传线路决策

Status: resolved
Model: Sonnet 5 —— 逻辑边界清晰（抽一个纯决策函数 + 改四处调用点 + 删掉前端硬编码），风险
集中在「显式配置优先」的取舍上，而那部分由主人先拍板。

## 背景

对应评估报告 C。配置了 `lines = alia`，补传却实际走 `bda2`，数据库 `current_line` 与日志均可证明。

## 根因

全仓有三套并存的线路策略：

| 路径 | 入口 | 线路来源 |
| --- | --- | --- |
| 录制期自动上传 | [`initialize_upload_context`](crates/biliup-cli/src/server/common/upload.rs:351) | `select_configured_line(config.lines)`，遵从配置 |
| 页面整场上传 | [`post_uploads`](crates/biliup-cli/src/server/api/endpoints.rs:648) → [`upload`](crates/biliup-cli/src/server/common/upload.rs:2130) | `UploadLine::from_str(config.lines)`，遵从配置 |
| 静默补传 / 手动补传 | [`recover_due_missing_segments`](crates/biliup-cli/src/server/common/upload.rs:1766)、[`manual_recover_missing_segment`](crates/biliup-cli/src/server/common/upload.rs:2563) | [`select_recovery_line`](crates/biliup-cli/src/server/common/upload.rs:459)，**硬编码 `bda2 → tx → auto`** |

`select_recovery_line` 的 `CANDIDATES` 常量完全不读 `config.lines`；`row.line_index` 只是失败计数
（`fail_enrolled_attempt` 每次失败 `+1`），与配置无关。页面「下次线路」列在
[endpoints.rs:826](crates/biliup-cli/src/server/api/endpoints.rs:826) 附近把同一份
`["bda2","tx","auto"]` 又硬编码了一遍，所以页面显示与实际选择「一致地错」。

另外录制期整场只在 `initialize_upload_context` 选一次线路，中途劣化不会换线（本任务先记录，
是否改由主人定）。

## 改动范围

- 抽出单一决策函数：输入 `(config.lines, 强制指定线路, 线路健康快照, 重试轮次)`，
  输出 `(最终线路, 候选序列, 选择原因)`。
- 四处调用点全部改调它：录制期自动上传、静默补传、手动补传、页面整场上传。
- 显式配置严格优先：配置线路不在冷却中就必须使用；冷却时才回退，且候选序列以配置线路为首、
  `auto` 兜底，不再以 `bda2` 打头。
- 决策结果写结构化日志（配置线路 / 候选 / 最终 / 原因），并由接口返回给前端；
  `get_missing_uploads` 的 `next_line`、`line_skip_reason` 改用后端决策结果，删掉前端侧推算。
- 保留 `bldsa` 不作为隐式候选的现有约束。

## 验收

- 配置 `alia` 后，下一次补传的 pre-upload 日志、分块日志、`current_line` 字段、页面「下次线路」
  四者一致为 `alia`。
- 只有在 `alia` 明确冷却时才回退，且回退原因在日志与页面均可见。
- `bda2` 不再是任何路径的隐式首选。

## 已决策（主人 2026-08-27 定）

**允许回退。** 即本文「改动范围」里描述的行为就是最终行为：显式配置线路不在冷却中就必须使用；
只有该线路明确处于冷却时才按候选序列回退，候选序列以配置线路为首、`auto` 兜底。

回退必须留痕，不能静默发生：

- 日志记录「配置线路 / 为何回退（冷却原因 + 剩余时间）/ 实际线路」；
- 页面「下次线路」列展示实际线路与回退原因（`line_skip_reason` 已有这个位置）；
- `current_line` 字段记录本次 attempt 真实使用的线路，供 08 的切换历史消费。

## Answer

已实现。新模块 `upload_line_selection`：

- 纯函数 `plan_upload_line(configured, forced, cooling, now) -> LinePlan`，
  输出 `{chosen, source, candidates, skipped}`；`resolve_planned_line` 只负责把 `auto` 变成一次探测。
- 候选序列以「手动指定 > 配置线路」打头，其后是 `bda2`、`tx`，`auto` 兜底；没有首选线路时序列就是
  `[auto]`——不再有任何路径以 `bda2` 开头。`bldsa` 仍然只在显式配置时才用。
- `line_index` 退化为纯失败计数，不再参与选路；配置线路只要不在冷却就必须使用，与重试轮次无关。
- 四处调用点（录制期 `initialize_upload_context`、页面整场 `upload`、静默补传
  `recover_due_missing_segments`、手动补传 `claim_manual_recovery`）全部改调 `decide_upload_line`。
- 回退留痕：`log_line_decision` 打印「配置线路 / 候选 / 实际 / 跳过原因（含剩余秒数）」；
  attempt 落库 `current_line` + `line_source`；`get_missing_uploads` 的 `next_line` /
  `line_skip_reason` / `line_candidates` 改由同一个纯函数算，删掉了前端与接口里各一份的
  `["bda2","tx","auto"]` 硬编码。同时删掉了 `missing_segment` 里已无人调用的 `FALLBACK_LINES`
  与 `upload_line_for_recovery`。

测试：`upload_line_selection` 六条单元测试 + `upload::tests` 三条（配置线路优先、冷却回退留痕、手动指定覆盖配置）。
