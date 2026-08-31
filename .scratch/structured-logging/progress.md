# 日志重构：阶段进度与 session 交接

本文件是新 session 的进度入口，配合 [阶段执行提示词](stage-prompts.md) 使用。
它是证据的索引，不替代代码、测试和运行报告；状态过时或与实现不符时先核实，再更正。

P0–P2 已完成并通过本地受控验收；P3 进行中：12（验收 partial）、13、15、16 均已交付，**14 未开始**。
录制域与后处理域已有原生事件并在一场真实开播录制上跑通全链，系统/认证/审计仍只有桥接；
P4–P6 未开始，旧日志与旧页面保留，新页只是试用入口。

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
| P0 | 06 | complete | passed | not-required | passed | [P0](receipts/P0.md) |
| P1 | 07、08 | complete | passed | not-required | passed | [P1](receipts/P1.md) |
| P2 | 09、10、11 | complete | passed | not-required | passed | [P2](receipts/P2.md) |
| P3 | 12、13、14、15、16 | in-progress | partial | pending | in-progress | [P3](receipts/P3.md) |
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
| [06 基线与契约](issues/06-baseline-contract.md) | P0 | complete | passed | [契约](contract-v1.md)、[基线预算](baseline-budget.md)、[回执](receipts/P0.md) |
| [07 通用事件组件](issues/07-independent-core.md) | P1 | complete | passed | [P1](receipts/P1.md)：快照/脱敏/过滤/队列/故障/并发测试 |
| [08 独立 SQLite](issues/08-sqlite-writer.md) | P1 | complete | passed | [P1](receipts/P1.md)：幂等/只读/附件/维护/备份/强杀与量化负载 |
| [09 入口旁路](issues/09-shadow-integration.md) | P2 | complete | passed | [P2](receipts/P2.md)：5 入口 × 关闭/开启/不可用矩阵、dev server 真实服务、回退演练、双路负载 |
| [10 证据导出](issues/10-evidence-export.md) | P2 | complete | passed | [P2](receipts/P2.md)：6 个证据包 complete + 校验 passed、12 项合成故障回归 |
| [11 Agent 对比](issues/11-agent-reconciliation.md) | P2 | complete | passed | [P2](receipts/P2.md)：三份提示词、视图隔离、桥接传输核对、独立还原 × 2 与交叉复核、一次真实差异闭环 |
| [12 录制试点](issues/12-recording-pilot.md) | P3 | complete | partial | [P3](receipts/P3.md)：身份贯通、受控演练与一场真实开播录制通过；缺断连/重连的真实样本 |
| [13 上传等后处理](issues/13-upload-pilot.md) | P3 | complete | passed | [P3](receipts/P3.md)：决定链受控实跑 + 真实远端上传、人工补传与投稿成功全部通过 |
| [14 全范围覆盖](issues/14-coverage-expansion.md) | P3 | not-started | pending | 待实现、场景验证及首轮观察 |
| [15 查询 API](issues/15-query-api.md) | P3 | complete | passed | [P3](receipts/P3.md)：接口/实时/导出/认证边界与 2 万条负载均实跑通过 |
| [16 试用页面](issues/16-preview-ui.md) | P3 | complete | passed | [P3](receipts/P3.md)：真数据下 13 项页面验收全部通过；默认入口开关初值仍是旧页 |
| [17 默认新页面](issues/17-default-events.md) | P4 | not-started | pending | 待前置通过后切换和第二轮观察 |
| [18 停旧写入](issues/18-stop-legacy-writes.md) | P5 | not-started | pending | 待前置通过后无文件观察及回退验证 |
| [19 移除旧实现](issues/19-remove-legacy.md) | P6 | not-started | pending | 待前置通过后移除与兼容收尾 |

## 当前交接位置

- 最近已验收阶段：P2（09、10、11 全部 passed）。
- 当前实施阶段/任务：P3 进行中；12、13、15、16 已完成，**只剩 14（全范围覆盖）**，单独一轮 session 做。
- 本轮已做（第二轮，任务 16）：新增「日志与事件（试用）」页与两个固定地址（新页 `/log-events`、
  旧页 `/logviewer/legacy`），`/logviewer` 由一个常量决定渲染哪一个、初值仍是旧页；页面接
  `/v1/log-events` 的筛选、命中计数、行内详情、场次范围、实时接续与导出，另有独立的运行进度视图；
  为满足「最新在前」和「分别看信息/警告/错误」给查询接口加了三个加法参数（`levels`、
  `categories`、`order=desc`）并补测试；用一场**真实开播录制**在本机 dev 环境跑通录制→预处理→
  上传→补传→投稿全链，顺带补上 12/13 缺的真实运行样本；回写回执、覆盖清单、代码索引与本表。
- 源码交付状态：`refactor/issue2-260831-153007`；第一轮（12/13/15）与第二轮（16）的改动都已提交。
- 运行/观察记录：受控录制演练、受控入口矩阵、本地 dev server 三态演练、20,000 条双路负载，
  以及本轮**一场真实开播录制到投稿成功**（207 条原生事件，采集健康无丢弃无写入失败）；
  仍**没有长观察窗口**，也没有真实断连样本。
- 已知边界：原生覆盖到 C02–C10（录制域与后处理域），C01、C11–C13 仍只有桥接；断连/重连、
  外部下载器、HLS 与页面/CLI 整场 `upload()` 未有真实样本或未接入；真实 601 限流的
  `reason_code` 落在 `transport`，事件层面分辨不出限流；纯文本桥接在重复旧行与脱敏行上
  不可判定；仅 macOS 本机验证，Windows 未验证。
- 下一次可直接输入（**一轮只做一个任务**，做完回写并提交，然后停下）：
  - `@.scratch/structured-logging/stage-prompts.md 继续 P3，仅处理任务 14`
  - 范围与验收边界见 [P3 回执](receipts/P3.md) 的「后续拆分」一节；14 体量最大，可再按四个批次分轮。

## 每次回写的检查清单

1. 创建/更新 [阶段执行回执](templates/stage-receipt.md)，列明每项证据适用的源码/契约/
   采集配置，以及哪些旧验证因改动失效；公开文档不填真实部署信息。
2. 更新本表阶段行、任务行、当前交接位置；保留未通过项及阻塞原因，不能只记成功项。
3. 更新 ticket `Implementation`，在 `## Comments` 链接已存在的回执；triage 状态按仓库
   约定维护。不把 `Implementation: complete` 当作可以跳过观察的依据。
4. 更新覆盖清单/受影响设计/代码索引；真实证据不可用时记录限制并补验证，禁止伪造引用。
5. 如源码被回退、关键契约改变或观察范围不足，相应通过状态改回待验收并说明原因；不为
   保住进度数字保留已经无效的通过记录。
