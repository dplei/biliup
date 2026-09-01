# 日志重构：阶段进度与 session 交接

本文件是新 session 的进度入口，配合 [阶段执行提示词](stage-prompts.md) 使用。
它是证据的索引，不替代代码、测试和运行报告；状态过时或与实现不符时先核实，再更正。

P0–P2 已完成并通过本地受控验收；P3 进行中：12（验收 partial）、13、15、16 均已交付，**14 已接入前六批原生能力、完成未迁移诊断分类清单，第八批调整为加法式双日志验证，第九批闭合两个外部下载器 gap，验收 partial**。
录制域与后处理域已有原生事件并在一场真实开播录制上跑通全链，系统与认证域已有原生事件但只有 Rust CLI 做过真实进程实跑；审计域已通过临时业务库与真实事件 SQLite 的受控重放，FFmpeg 诊断已做受控失败验证。第九批已把 Streamlink 与 YtDlp/YtArchive 接上原生分段身份与有界退出诊断，剩余原生覆盖缺口只有 danmaku 异步 recorder 的运行中失败；
剩余调用点不再以本地假命令/入口矩阵为前置，源码闭合并通过编译、基本启动和关闭新采集不影响业务的最低门槛后进入实际双写观察；
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
| [14 全范围覆盖](issues/14-coverage-expansion.md) | P3 | in-progress | partial | [第九批回执](receipts/P3.md#任务-14-第九批外部下载器streamlink--ytdlp)：外部下载器已接入，只剩 danmaku 异步失败 1 个源码 gap，预计 1 个实现 session / 1.5–3 小时；本地矩阵改为实际双写自然采样，首轮观察仍待完成 |
| [15 查询 API](issues/15-query-api.md) | P3 | complete | passed | [P3](receipts/P3.md)：接口/实时/导出/认证边界与 2 万条负载均实跑通过 |
| [16 试用页面](issues/16-preview-ui.md) | P3 | complete | passed | [P3](receipts/P3.md)：真数据下 13 项页面验收通过；补传入口样式补修已复核；默认入口仍是旧页。当前全仓类型检查限制见回执 |
| [17 默认新页面](issues/17-default-events.md) | P4 | not-started | pending | 待前置通过后切换和第二轮观察 |
| [18 停旧写入](issues/18-stop-legacy-writes.md) | P5 | not-started | pending | 待前置通过后无文件观察及回退验证 |
| [19 移除旧实现](issues/19-remove-legacy.md) | P6 | not-started | pending | 待前置通过后移除与兼容收尾 |

## 当前交接位置

- 最近一轮仅处理任务 14 第九批：服务端 Streamlink 与 YtDlp/YtArchive 的加法式原生接入。
  沿用 `refactor/issue2-260831-153007`，父提交 `ce40c48`；改动集中在两个下载器源码与
  effort 文档，未 push。
- Streamlink：目标 `.part` 由本进程 `--output` 选定，创建与关闭都是真实观测，带 S/DA、
  关闭原因与有界退出诊断；新增取消标记，`stop()` 先写标记再动进程。退出后没有 `.part` 或
  改名失败如实记 `segment_closed` failed，不冒充已关闭的分段。
- YtDlp/YtArchive：外部工具自己创建、命名并搬运文件，创建时刻不可观测，因此只发
  `segment_closed`；spawn 失败记 `spawn_failed`（无退出码），非零退出记 `process_failed`
  带退出码与有界脱敏附件；原先塞进自由错误的完整 combined output 改成有界脱敏摘要。
  内置封面失败补 `recording.auxiliary_failed`。
- 未改子进程管理、文件搬运、清理、重试或业务状态机；yt-dlp 的 `stop()` 只置标记不杀进程
  这一既有语义原样保留，被停止的调用仍不发分段事件。
- 剩余源码缺口只有 danmaku spawned recorder 的运行中 recorder/write/decode 失败，
  预计 1 个实现 session、1.5–3 小时；它闭合后才能开始首轮实际双写观察。
- 本批验证：4 项新单元测试（判定口径 2 + 隐私/容量边界 2）、`biliup-cli` lib
  339 passed / 6 ignored、`biliup --test recording_events` 6 passed、
  `biliup-observability` 13 passed、真实 `biliup dump-flv` 二进制的采集开/关对照实跑、
  触达文件 clippy 与 rustfmt 通过、分类漂移 62 files / 554 sites 且 `coverage_gap` 3 → 1。
  **本机没有安装 streamlink/yt-dlp/ytarchive，两条路径没有任何实跑样本**，正常下载、切片、
  取消与真实失败全部保持待观察。旧 601 `transport` 差异与 FFmpeg 秒级命名碰撞仍是待处理项。
- 下一轮入口：`@.scratch/structured-logging/stage-prompts.md 继续 P3，仅处理任务 14`。
  下批只把 `danmaku/src/client.rs` 里 spawned recorder 的失败穿透到既有 auxiliary 边界，
  旧错误行保留，不改弹幕协议、重试或录制状态机；之后才进入首轮实际双写观察，
  未自然触发的路径仍不能写 passed。

## 每次回写的检查清单

1. 创建/更新 [阶段执行回执](templates/stage-receipt.md)，列明每项证据适用的源码/契约/
   采集配置，以及哪些旧验证因改动失效；公开文档不填真实部署信息。
2. 更新本表阶段行、任务行、当前交接位置；保留未通过项及阻塞原因，不能只记成功项。
3. 更新 ticket `Implementation`，在 `## Comments` 链接已存在的回执；triage 状态按仓库
   约定维护。不把 `Implementation: complete` 当作可以跳过观察的依据。
4. 更新覆盖清单/受影响设计/代码索引；真实证据不可用时记录限制并补验证，禁止伪造引用。
5. 如源码被回退、关键契约改变或观察范围不足，相应通过状态改回待验收并说明原因；不为
   保住进度数字保留已经无效的通过记录。
