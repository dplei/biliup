# 日志重构：阶段进度与 session 交接

本文件是新 session 的进度入口，配合 [阶段执行提示词](stage-prompts.md) 使用。
它是证据的索引，不替代代码、测试和运行报告；状态过时或与实现不符时先核实，再更正。

P0–P2 已完成并通过本地受控验收；P3 进行中：12（验收 partial）、13、15、16 均已交付，**14 已接入前六批原生能力、完成未迁移诊断分类清单，第八批调整为加法式双日志验证，第九批闭合两个外部下载器 gap，第十批关闭 FFmpeg 内部分段命名撞车，第十一批闭合最后一个 danmaku gap；源码缺口归零但首轮观察未开始，验收 partial**。
录制域与后处理域已有原生事件并在一场真实开播录制上跑通全链，系统与认证域已有原生事件但只有 Rust CLI 做过真实进程实跑；审计域已通过临时业务库与真实事件 SQLite 的受控重放，FFmpeg 诊断已做受控失败验证。第九批已把 Streamlink 与 YtDlp/YtArchive 接上原生分段身份与有界退出诊断，第十一批把 danmaku 后台任务的终止穿透到辅助失败边界，**源码 `coverage_gap` 归零**；
门槛因此满足，下一步是首轮实际双写观察——观察尚未开始，14 的完成取决于观察结果而不是源码状态；
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
| P0 | 06 | complete | passed | not-required | passed | [P0](../../.archive/structured-logging-p0-p2/receipts/P0.md) |
| P1 | 07、08 | complete | passed | not-required | passed | [P1](../../.archive/structured-logging-p0-p2/receipts/P1.md) |
| P2 | 09、10、11 | complete | passed | not-required | passed | [P2](../../.archive/structured-logging-p0-p2/receipts/P2.md) |
| P3 | 12、13、14、15、16 | in-progress | partial | pending | in-progress | [P3](receipts/P3.md) |
| P4 | 17 | not-started | pending | pending | not-started | — |
| P5 | 18 | not-started | pending | pending | not-started | — |
| P6 | 19 | not-started | pending | not-required | not-started | — |

执行回执在阶段实际开始后按模板建立为 `receipts/P<n>.md`，创建后再补可点击链接。
**P0–P2 的 issue（06–11）与三份回执已按阶段归档到
[`.archive/structured-logging-p0-p2/`](../../.archive/structured-logging-p0-p2/)**：它们的验收
已定稿、后续阶段不再改动，上表链接直接指向归档位置。P3 及以后仍在本目录推进。
P0/P1/P2/P6 没有独立的长观察窗口，但仍有各自必测的真实/受控场景和验收条件。

## 任务检查表

依赖的唯一编排依据是 [阶段计划](rollout-plan.md) 与对应 ticket 的 `Blocked by`；本表不
再抄写一套依赖。每轮结束时，与 ticket 的 `Implementation` 和回执逐项同步。

| 任务 | 所属阶段 | 交付 | 验收 | 有效证据 / 待完成项 |
| --- | --- | --- | --- | --- |
| [06 基线与契约](../../.archive/structured-logging-p0-p2/issues/06-baseline-contract.md) | P0 | complete | passed | [契约](contract-v1.md)、[基线预算](baseline-budget.md)、[回执](../../.archive/structured-logging-p0-p2/receipts/P0.md) |
| [07 通用事件组件](../../.archive/structured-logging-p0-p2/issues/07-independent-core.md) | P1 | complete | passed | [P1](../../.archive/structured-logging-p0-p2/receipts/P1.md)：快照/脱敏/过滤/队列/故障/并发测试 |
| [08 独立 SQLite](../../.archive/structured-logging-p0-p2/issues/08-sqlite-writer.md) | P1 | complete | passed | [P1](../../.archive/structured-logging-p0-p2/receipts/P1.md)：幂等/只读/附件/维护/备份/强杀与量化负载 |
| [09 入口旁路](../../.archive/structured-logging-p0-p2/issues/09-shadow-integration.md) | P2 | complete | passed | [P2](../../.archive/structured-logging-p0-p2/receipts/P2.md)：5 入口 × 关闭/开启/不可用矩阵、dev server 真实服务、回退演练、双路负载 |
| [10 证据导出](../../.archive/structured-logging-p0-p2/issues/10-evidence-export.md) | P2 | complete | passed | [P2](../../.archive/structured-logging-p0-p2/receipts/P2.md)：6 个证据包 complete + 校验 passed、12 项合成故障回归 |
| [11 Agent 对比](../../.archive/structured-logging-p0-p2/issues/11-agent-reconciliation.md) | P2 | complete | passed | [P2](../../.archive/structured-logging-p0-p2/receipts/P2.md)：三份提示词、视图隔离、桥接传输核对、独立还原 × 2 与交叉复核、一次真实差异闭环 |
| [12 录制试点](issues/12-recording-pilot.md) | P3 | complete | partial | [P3](receipts/P3.md)：身份贯通、受控演练与一场真实开播录制通过；缺断连/重连的真实样本 |
| [13 上传等后处理](issues/13-upload-pilot.md) | P3 | complete | passed | [P3](receipts/P3.md)：决定链受控实跑 + 真实远端上传、人工补传与投稿成功全部通过 |
| [14 全范围覆盖](issues/14-coverage-expansion.md) | P3 | in-progress | partial | [第十一批回执](receipts/P3.md#任务-14-第十一批弹幕后台任务终止)：源码 `coverage_gap` 已归零（第十批同时关闭了 FFmpeg 内部分段命名撞车），剩余全部是验收工作——首轮实际双写观察与差异复核未开始，未自然触发的路径保持待观察 |
| [15 查询 API](issues/15-query-api.md) | P3 | complete | passed | [P3](receipts/P3.md)：接口/实时/导出/认证边界与 2 万条负载均实跑通过 |
| [16 试用页面](issues/16-preview-ui.md) | P3 | complete | passed | [P3](receipts/P3.md)：真数据下 13 项页面验收通过；默认入口仍是旧页。**合并前复核更正**：`FilterBar.tsx` 是本 effort 新增文件而非历史遗留，在本机 pnpm 树（Semi 2.102）上构建失败，已修；镜像构建走 npm 树（Semi 2.89.1）不受影响，见回执「发版前复核」 |
| [17 默认新页面](issues/17-default-events.md) | P4 | not-started | pending | 待前置通过后切换和第二轮观察 |
| [18 停旧写入](issues/18-stop-legacy-writes.md) | P5 | not-started | pending | 待前置通过后无文件观察及回退验证 |
| [19 移除旧实现](issues/19-remove-legacy.md) | P6 | not-started | pending | 待前置通过后移除与兼容收尾 |

## 当前交接位置

- 最近一轮仅处理任务 14 第十一批：`danmaku` 后台 recorder 在 `start()` 成功之后才发生的终止，
  也是最后一个源码 `coverage_gap`。沿用 `refactor/issue2-260831-153007`，父提交为第十批
  `633f123`；改动集中在 `danmaku` crate、服务端弹幕客户端与 effort 文档，未 push。
- `DanmakuRecorder` 增加终止观察回调，由宿主注入，`danmaku` crate 不依赖采集组件；没有观察者
  时行为与从前完全一致。回调恰好触发一次，含 panic 与被取消（记 `danmaku_aborted`）。
- 正常收尾不产生事件；失败按**错误类型**映射到 output/connection/protocol/internal 四个稳定
  原因码，不解析错误文本，事件不带原文、路径或 URL。stage 固定 `danmaku_runtime`，与既有的
  `danmaku_start`/`danmaku_stop`/`danmaku_roll` 三个同步边界区分开。
- 范围有意收窄：逐条 `write_event` 与逐帧解码失败归 `no_persistence`，丢的是单条弹幕不是
  整场结果，且频率可达弹幕级。记录但未修的既有缺陷：YouTube 轮询分支缺重连、
  `RecorderHandle::stop()` 吞掉发送错误——两者现在都会被本事件暴露。
- 同时更正第七批「运行中失败完全不可见」的措辞：recorder 死后下一次 `rolling()` 会因 channel
  断开发出 `danmaku_roll` 事件，但要等到下一次分段（单段场次永不触发）且归因错误。
- 录制身份此前根本没传进弹幕客户端，本批把本场 `RecordingIdentity` 的构造提前一步并显式
  克隆进去，事件层不从文件名或时间反推。
- 本批验证：真起一个输出不可写的 recorder，`download()` 如常成功，随后**恰好一条**事件落进
  独立采集器并核对了 stage/outcome/reason_code/级别/身份/脱敏（2 项集成测试）；
  `danmaku` 42 passed、`biliup-cli` lib 344 passed / 6 ignored、录制域 6 passed、
  底座 22 passed、工作区构建通过、真实 `biliup dump-flv` 开关对照业务输出逐字节一致、
  触达文件格式与 clippy 通过、分类漂移 62 files / 555 sites 且 **`coverage_gap` 1 → 0**。
- **实跑只覆盖了终止的一种**（输出不可写）。真实断连耗尽、协议失败、panic 与被取消都还没有
  自然样本，保持待观察；真实直播场次的弹幕录制本轮没有跑过。第九批的限制原样有效：本机没有
  streamlink/yt-dlp/ytarchive，那两条路径仍无任何实跑样本。旧 601 `transport` 差异与任务 12
  的真实断连样本仍是待处理项。14 仍 in-progress / partial，当前不能进入 P4。
- 下一轮入口：`@.scratch/structured-logging/stage-prompts.md 继续 P3，仅处理任务 14`。
  下一轮是**首轮实际双写观察**：以日常运行自然产生的成功、并发与异常样本做旧日志与新事件的
  双源核对，写观察报告与差异处置。未自然触发的路径仍不能写 passed；观察未完成之前 14 不标
  complete。

## 每次回写的检查清单

1. 创建/更新 [阶段执行回执](templates/stage-receipt.md)，列明每项证据适用的源码/契约/
   采集配置，以及哪些旧验证因改动失效；公开文档不填真实部署信息。
2. 更新本表阶段行、任务行、当前交接位置；保留未通过项及阻塞原因，不能只记成功项。
3. 更新 ticket `Implementation`，在 `## Comments` 链接已存在的回执；triage 状态按仓库
   约定维护。不把 `Implementation: complete` 当作可以跳过观察的依据。
4. 更新覆盖清单/受影响设计/代码索引；真实证据不可用时记录限制并补验证，禁止伪造引用。
5. 如源码被回退、关键契约改变或观察范围不足，相应通过状态改回待验收并说明原因；不为
   保住进度数字保留已经无效的通过记录。
