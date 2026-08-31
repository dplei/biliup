# 日志重构：阶段进度与 session 交接

本文件是新 session 的进度入口，配合 [阶段执行提示词](stage-prompts.md) 使用。
它是证据的索引，不替代代码、测试和运行报告；状态过时或与实现不符时先核实，再更正。

当前仅完成设计、任务拆分和执行说明，**P0–P6 的实际实施/验收均未开始**。
不要把本文件初始化、设计草图可点击或 ticket 已存在算作阶段完成。

## 状态约定

| 字段 | 允许值 | 含义 |
| --- | --- | --- |
| 交付 / `Implementation` | `not-started`、`in-progress`、`complete` | 任务要求的代码/工具/契约产物是否完成；complete 不代表已验收 |
| 验收 | `pending`、`partial`、`passed`、`failed` | 必要测试、场景及回退验证的实际结果 |
| 观察 | `not-required`、`pending`、`observing`、`passed` | 是否满足阶段要求的运行期证据；P6 仍须核验 P5 的观察结果 |
| 阶段状态 | `not-started`、`in-progress`、`awaiting-validation`、`blocked`、`passed` | 阶段的汇总结论，不把待观察当作通过 |

`Status:` 是仓库 ticket 的 triage 字段，不用它推断实现完成度。这里的 `blocked` 只表示
项目阶段存在具体未解前置/资源条件，不涉及任何工具中的 goal 状态。
`awaiting-validation` 用于代码已做完但缺测试/真实样本/观察窗口；等待时仍做不受阻的任务。

阶段只有同时满足以下条件才为 `passed`：本阶段适用任务的产物完成、验收通过、要求的
观察通过或确实不适用、前置证据有效且关键差异关闭。时间经过、Agent 自述或一个提交
不能单独作为依据。部分任务完成时可写交付/验收部分进展，阶段不能提前通过。

## 阶段总表

| 阶段 | 任务 | 交付 | 验收 | 观察 | 阶段状态 | 执行回执 |
| --- | --- | --- | --- | --- | --- | --- |
| P0 | 06 | not-started | pending | not-required | not-started | — |
| P1 | 07、08 | not-started | pending | not-required | not-started | — |
| P2 | 09、10、11 | not-started | pending | not-required | not-started | — |
| P3 | 12、13、14、15、16 | not-started | pending | pending | not-started | — |
| P4 | 17 | not-started | pending | pending | not-started | — |
| P5 | 18 | not-started | pending | pending | not-started | — |
| P6 | 19 | not-started | pending | not-required | not-started | — |

执行回执在阶段实际开始后按模板建立为 `receipts/P0.md` 等文件，创建后再补可点击链接。
P0/P1/P2/P6 没有独立的长观察窗口，但仍有各自必测的真实/受控场景和验收条件。

## 任务检查表

依赖的唯一编排依据是 [阶段计划](rollout-plan.md) 与对应 ticket 的 `Blocked by`；本表不
再抄写一套依赖。每轮结束时，与 ticket 的 `Implementation` 和回执逐项同步。

| 任务 | 所属阶段 | 交付 | 验收 | 有效证据 / 待完成项 |
| --- | --- | --- | --- | --- |
| [06 基线与契约](issues/06-baseline-contract.md) | P0 | not-started | pending | 待冻结契约、覆盖/预算并实际测量 |
| [07 通用事件组件](issues/07-independent-core.md) | P1 | not-started | pending | 待实现 |
| [08 独立 SQLite](issues/08-sqlite-writer.md) | P1 | not-started | pending | 待实现 |
| [09 入口旁路](issues/09-shadow-integration.md) | P2 | not-started | pending | 待实现 |
| [10 证据导出](issues/10-evidence-export.md) | P2 | not-started | pending | 待实现 |
| [11 Agent 对比](issues/11-agent-reconciliation.md) | P2 | not-started | pending | 待实现及受控演练 |
| [12 录制试点](issues/12-recording-pilot.md) | P3 | not-started | pending | 待实现 |
| [13 上传等后处理](issues/13-upload-pilot.md) | P3 | not-started | pending | 待实现 |
| [14 全范围覆盖](issues/14-coverage-expansion.md) | P3 | not-started | pending | 待实现、场景验证及首轮观察 |
| [15 查询 API](issues/15-query-api.md) | P3 | not-started | pending | 待实现 |
| [16 试用页面](issues/16-preview-ui.md) | P3 | not-started | pending | 待实现 |
| [17 默认新页面](issues/17-default-events.md) | P4 | not-started | pending | 待前置通过后切换和第二轮观察 |
| [18 停旧写入](issues/18-stop-legacy-writes.md) | P5 | not-started | pending | 待前置通过后无文件观察及回退验证 |
| [19 移除旧实现](issues/19-remove-legacy.md) | P6 | not-started | pending | 待前置通过后移除与兼容收尾 |

## 当前交接位置

- 最近已验收阶段：无。
- 当前实施阶段/任务：无；尚未开始编码或基线测量。
- 本轮已做：设计与拆分、阶段提示词和空进度表；这些不计入 P0 验收。
- 源码交付状态：没有日志重构实现；设计文档是否已提交以新 session 的 `git status` 为准。
- 真实运行/观察记录：无。
- 已知待办：从 06 核验当前入口、冻结契约/预算并测量，不依赖聊天记忆。
- 下一次可直接输入：`@.scratch/structured-logging/stage-prompts.md 实现 P0`。

## 每次回写的检查清单

1. 创建/更新 [阶段执行回执](templates/stage-receipt.md)，列明每项证据适用的源码/契约/
   采集配置，以及哪些旧验证因改动失效；公开文档不填真实部署信息。
2. 更新本表阶段行、任务行、当前交接位置；保留未通过项及阻塞原因，不能只记成功项。
3. 更新 ticket `Implementation`，在 `## Comments` 链接已存在的回执；triage 状态按仓库
   约定维护。不把 `Implementation: complete` 当作可以跳过观察的依据。
4. 更新覆盖清单/受影响设计/代码索引；真实证据不可用时记录限制并补验证，禁止伪造引用。
5. 如源码被回退、关键契约改变或观察范围不足，相应通过状态改回待验收并说明原因；不为
   保住进度数字保留已经无效的通过记录。
