# 2026-08-26 补传与录制事故（第二轮）：问题评估与修复摘要

Status: implemented

来源清单：`~/fsdownload/2026-08-26-upload-incident-bugs.md`。其中「P1：音量标准化被上传
watchdog 错杀」的进程内部分已由 `3308d6d`（watchdog 推迟到 transfer 才启动）修复，本文不再重复，
但它遗留了一个同源的数据库侧缺陷，见 A-2。

本文只做只读评估，不含实现。每节给出：现象 → 根因（已在源码确认）→ 影响 → 修复方向 → 验收。

---

## A. 上传租约与卡死（对应 P1「上传到约 50% 后停止推进」）

### A-1 手动补传的 watchdog 与 HTTP 请求同生共死

`recover_missing_upload` / `retry_missing_upload`
（[endpoints.rs:861](crates/biliup-cli/src/server/api/endpoints.rs:861)、
[endpoints.rs:965](crates/biliup-cli/src/server/api/endpoints.rs:965)）在 handler 内直接
`await` 整段上传；watchdog 不是独立任务，而是
[`upload_enrolled_with_watchdog`](crates/biliup-cli/src/server/common/upload.rs:1581) 内部的
`tokio::select!` 分支，与上传 future 处在同一个 future 树上。

反向代理 504 后关闭上游连接 → axum drop 掉 handler future → 上传 future 和 watchdog 一起被
drop → `fail_enrolled_attempt` 永远没有机会执行。`AttemptGuard::drop`
（[upload.rs:134](crates/biliup-cli/src/server/common/upload.rs:134)）只清理进程内注册表，不写库。
结果就是数据库停在 `uploading`、`last_progress_at` 不再前进，页面持续显示「补传中」，
这与事故里的 `stale_uploading_count=1` 完全吻合。

**修复方向**：补传工作必须从 HTTP 请求生命周期里剥离（`tokio::spawn`，同 `post_uploads`
[endpoints.rs:648](crates/biliup-cli/src/server/api/endpoints.rs:648) 已有的做法），接口只做
claim + 返回 attempt 标识。与 B 节是同一处改动。

### A-2 数据库侧 stale 收割仍然按 5 分钟无网络进度判定（新发现，高危）

[`recover_stale_upload_attempts`](crates/biliup-cli/src/server/common/missing_segment.rs:100)
每 60 秒把 `COALESCE(last_progress_at, upload_started_at, updated_at) <= now-5min` 的 `uploading`
行改成 `failed`、清空 `attempt_token`。而 `claim_enrolled_attempt`
（[upload.rs:809](crates/biliup-cli/src/server/common/upload.rs:809)）在**预处理开始之前**就把
`last_progress_at` 置为 claim 时刻，其后依次是：音量标准化 → 时间戳修复 → 等待全局上传 permit
（[upload.rs:1317](crates/biliup-cli/src/server/common/upload.rs:1317)，容量为 1）→ pre_upload →
才发出 `TransferStarted`。

也就是说 `3308d6d` 只让进程内 watchdog 学会了等 transfer，数据库侧的收割器没有跟着改。
3.32 GB 分段的标准化耗时远超 5 分钟，于是：

1. 收割器把仍在正常工作的 attempt 判成 `stale_uploading_lease`，置 `failed`、`line_index+1`、
   `next_retry_at = now`；
2. 进程内那次上传毫不知情地继续跑（收割不取消 `CancellationToken`），成为幽灵 attempt；
   它最终 `persist_segment`（[upload.rs:911](crates/biliup-cli/src/server/common/upload.rs:911)）
   时 token CAS 失败，整段上传作废；
3. 与此同时该行立刻到期，另一条恢复路径领走并起第二次上传；因为全局 permit 只有 1 个，
   新 attempt 卡在 permit 等待上，`TransferStarted` 迟迟不发，进程内 watchdog 按 `3308d6d`
   的语义保持 paused，5 分钟后再次被收割 —— 形成「一直 uploading、没有新 acknowledged、
   也不真正进入重试」的自激循环，正是清单里描述的现象。

**修复方向**：
- 给生命周期行引入显式阶段（如 `attempt_phase`：`preprocessing` / `queued` / `transferring`）
  或独立的 `last_heartbeat_at`，预处理与排队阶段定期续租；收割只对 `transferring` 且无网络
  进度的行按 5 分钟判定，其余阶段用各自更宽的上限（预处理建议按文件大小给上限，排队建议
  单独超时并记录原因）。
- 收割前先查进程内注册表：本进程仍持有该 attempt 时应走 `cancel_registered_attempt`
  （[upload.rs:1745](crates/biliup-cli/src/server/common/upload.rs:1745)）真正取消，再落库；
  只有确认无人持有（跨进程遗留）才直接 CAS 置 `failed`。当前实现二者完全脱钩。
- 全局上传 permit 的等待要么计入独立超时，要么在拿到 permit 前不 claim attempt。

### A-3 分块请求超时存在，但缺少「卡住的分块」诊断字段

分块 PUT 已有 240 秒总超时并重试 3 次
（[upos.rs:123](crates/biliup/src/uploader/line/upos.rs:123)，`retry` 默认 3 次见
[lib.rs:15](crates/biliup/src/lib.rs:15)），连接超时 60 秒
（[client.rs:25](crates/biliup/src/client.rs:25)）。所以「每个分块必须有明确超时」这条期望
在实现上已经满足，单块最坏阻塞约 13 分钟；清单里的现象不是缺超时，而是 A-1/A-2。

仍缺的是诊断：分块级失败只在 `retry` 内打印，不带线路、分块号、耗时，也不落库；页面只有
`last_error` 一个字段。建议在 attempt 维度记录「最后一次分块开始时间 / 分块号 / 线路 /
最近一次分块错误」，并把 `no_progress_timeout` 触发时的这些值写进 `last_error`。

### A-4 `no_progress_timeout` 会连坐线路熔断

[`record_watchdog_failure`](crates/biliup-cli/src/server/common/upload.rs:1481) 把 watchdog 超时
统一记成 `RequestTimeout` 并累计该线路的 `consecutive_failures`
（[upload_line_health.rs:155](crates/biliup-cli/src/server/common/upload_line_health.rs:155) 起）。
在 A-2 的自激循环下，本地预处理慢会被记成远端线路故障，把 `bda2` 一路冷却到 1 小时档，
再叠加 C 节的硬编码顺序，进一步把补传推向不该用的线路。修 A-2 后需要复核：只有真正在
`transferring` 阶段发生的无进度才计线路失败。

**A 节验收**：模拟一个分块永久无响应，以及一次耗时 20 分钟的音量标准化。前者必须在阈值内
释放租约、换线重试；后者全程不得被收割成 `failed`，最终上传的必须是标准化后的成片，且任何
时刻同一 `missing_id` 只有一个进程内 attempt 在跑。

---

## B. 补传接口同步阻塞导致 504（对应 P1「补传控制页」的后端部分）

`manual_recover_missing_segment`（[upload.rs:2563](crates/biliup-cli/src/server/common/upload.rs:2563)）
与 `retry_missing_segment`（[upload.rs:2914](crates/biliup-cli/src/server/common/upload.rs:2914)）
都在调用线程上跑完整段上传，handler 直接 `await`。3.32 GB 分段必然超过任何反向代理的读超时。

前端 [`handleResponse`](app/lib/api-streamer.ts:40) 在 `!res.ok` 时把响应体原文 `throw`，
Toast 于是弹出整段 OpenResty HTML。

**修复方向**：
- 接口改为「同步 claim、异步执行」：同步部分只做资格判定与 attempt claim，返回
  `{missing_id, attempt_token, status}`；上传在 `tokio::spawn` 里跑，进度经既有的
  `uploaded_bytes / last_progress_at / current_line` 落库，前端沿用现有 5 秒轮询即可。
- spawn 出去的任务必须自带失败落库（`fail_enrolled_attempt`），不能再依赖 handler 存活。
- 前端错误处理统一：非 JSON 响应体不透传，按 status 映射成中文提示（504 → 「服务端处理超时，
  任务可能仍在后台执行，请刷新查看状态」）。

**验收**：手动触发大文件补传，接口 1 秒内返回；页面进度持续刷新；反代超时不再影响任务本身。

---

## C. 线路选择策略三套并存（对应 P1「补传恢复路径忽略线路配置」）

当前三条路径各用各的：

| 路径 | 入口 | 线路来源 |
| --- | --- | --- |
| 录制期自动上传 | [`initialize_upload_context`](crates/biliup-cli/src/server/common/upload.rs:351) | `select_configured_line(config.lines)`，遵从配置 |
| 页面整场上传 | [`post_uploads`](crates/biliup-cli/src/server/api/endpoints.rs:648) → [`upload`](crates/biliup-cli/src/server/common/upload.rs:2130) | `UploadLine::from_str(config.lines)`，遵从配置 |
| 静默补传 / 手动补传 | [`recover_due_missing_segments`](crates/biliup-cli/src/server/common/upload.rs:1766)、`manual_recover_missing_segment` | [`select_recovery_line`](crates/biliup-cli/src/server/common/upload.rs:459)，**硬编码 `bda2 → tx → auto`** |

`select_recovery_line` 的 `CANDIDATES` 常量完全不读 `config.lines`，`row.line_index` 只是个失败
计数（`fail_enrolled_attempt` 每次失败 `line_index+1`），与配置无关。配置 `lines = alia` 时补传
必然落到 `bda2`，与事故观察一致。页面「下次线路」列
（[endpoints.rs:826](crates/biliup-cli/src/server/api/endpoints.rs:826) 附近）把同一份
`["bda2","tx","auto"]` 又硬编码了一遍，所以页面显示与实际选择"一致地错"。

另外：录制期整场只在 `initialize_upload_context` 选一次线路，中途线路劣化不会换线，只能等分段
失败后进入补传队列。

**修复方向**：
- 抽出单一线路决策函数，输入 `(config.lines, 强制指定线路, 线路健康快照, 重试轮次)`，
  输出 `(最终线路, 候选序列, 选择原因)`；自动/静默补传/手动补传/页面上传全部改调它。
- 显式配置严格优先：配置线路不在冷却中就必须使用；冷却时才按候选序列回退，且候选序列以
  配置线路为首、`auto` 兜底，不再以 `bda2` 打头。
- 决策结果（配置线路、候选、最终、原因）同时写日志与接口返回，页面「下次线路」直接用后端
  给出的值，不再自己推算。

**验收**：配置 `alia` 后，下一次补传的 pre-upload 与分块日志、`current_line` 字段、页面显示
三者一致为 `alia`；只有在 `alia` 明确冷却时才回退，且回退原因可见。

---

## D. 重启后同一场直播分裂成两个会话（对应 P1「重启后会话不连续」）

事故链条已定位为两个独立条件同时成立：

1. **重启必然产生新的 `streamer_info` 行**：`monitor` 每次检测到 `LiveStatus::Live` 都无条件
   `StreamerInfo::builder().insert()`（[monitor.rs:121](crates/biliup-cli/src/server/core/monitor.rs:121)），
   没有「这场直播已经有 streamer_info」的复用判断。于是重启后 `ctx.id()` 变了。
2. **会话复用的两条路都因此失效**：
   [`find_or_create_session`](crates/biliup-cli/src/server/common/segment_enrollment.rs:340)
   先按 `(live_streamer_id, streamer_info_id)` 精确匹配（重启后必 miss），再按
   `live_streamer_id + updated_at >= now-30min` 兜底。而 `upload_session.updated_at` 只在
   `persist_segment`（[upload.rs:977](crates/biliup-cli/src/server/common/upload.rs:977)）、
   enroll 走窗口分支时（[segment_enrollment.rs:379](crates/biliup-cli/src/server/common/segment_enrollment.rs:379)）
   和投稿相关操作里刷新 —— **录制中和上传中都没有心跳**。一段 3.32 GB 分段传一个多小时期间
   `updated_at` 完全不动，30 分钟窗口早已过期。

两条同时失效 → 建出空会话 229。`select_recovery_candidate`
（[upload_session.rs:19](crates/biliup-cli/src/server/common/upload_session.rs:19)）用同一个窗口
判据，所以 `prepare_archive` 也救不回来。`rescan_local_valid_segments`
（[upload.rs:2320](crates/biliup-cli/src/server/common/upload.rs:2320)）在按新
`streamer_info_id` 找不到会话时同样会**再建一个**，人工补扫会加剧分裂。

**修复方向**（按性价比排序）：
- 给会话续接引入不依赖时钟窗口的「同场身份」：以 `live_streamer_id + 直播场次标识`
  （开播时间/平台 room session）为准，重启后先尝试复用未 finalize 的同场 `streamer_info`
  而不是无条件插入新行；至少要让 `upload_session` 记录它归属的场次键。
- 录制/上传期间给 `upload_session.updated_at` 加心跳（分段 enroll、attempt claim、
  进度落库都应刷新），使 30 分钟窗口回到「真的静默了 30 分钟」的语义。
- `rescan_local_valid_segments` 在找不到会话时不要新建，而是按 `live_streamer_id` 找同场
  未 finalize 会话；确实没有时才新建，并在返回值里说明。
- 提高 `recovery_window_minutes` 只是缓解，不作为主修复。

**验收**：直播中重启服务并继续录制若干切片，所有分段落在同一 `upload_session`、
`segment_order` 连续唯一，最终只产生一个 BV 并按顺序追加分 P。

---

## E. 待补传任务不会主动恢复（对应 P1「待补传任务恢复不主动」）

`recover_due_missing_segments` 只有两个调用点，都在 `process_with_upload` 内部
（[upload.rs:278](crates/biliup-cli/src/server/common/upload.rs:278)、
[upload.rs:1028](crates/biliup-cli/src/server/common/upload.rs:1028)），也就是**必须有新的直播
事件**才会触发。启动时 [`app.rs:44`](crates/biliup-cli/src/server/app.rs:44) 只拉起了
`start_stale_attempt_recovery`（把卡死租约收敛成 `failed`），没有任何一处会去领取到期的
`pending/failed` 行。于是重启后到期任务只能干等下一场直播。

路由面（[router.rs:84](crates/biliup-cli/src/server/router.rs:84) 起）也只有
`missing`、`missing/rescan`、`missing/{id}/recover|retry|delete`，没有「按会话恢复」入口；
`post_uploads` 那条则完全绕开生命周期会话、自建稿件，不能用于修复。

**修复方向**：
- 启动后拉起一个到期扫描循环（与 stale 收割同级），按 `segment_order` 顺序领取到期行，
  复用会话已有 `aid/bvid`：首段建稿、后续 append；扫描必须走
  [`check_recovery_eligibility`](crates/biliup-cli/src/server/common/recovery_eligibility.rs)
  以免碰 finalized 会话。
- 新增 `POST /v1/uploads/sessions/{id}/recover`：对指定会话重新扫描并恢复，不新建投稿会话，
  返回被领取的分段列表。
- 恢复动作全部异步执行（与 B 节共用同一套 spawn + 进度落库机制）。

**验收**：重启后已到期的 `failed/pending` 分段无需新直播事件即可自动开始；手动按会话恢复
立即生效且保持同一 BV。

---

## F. 补传控制页缺少线路与任务控制（对应 P1「补传控制页」的前端部分）

现状（[app/(app)/missing/page.tsx](app/(app)/missing/page.tsx)）：已有进度百分比、
`current_line`、无进度秒数、开始时间、下次线路与跳过原因、会话完整性横幅。缺的是控制能力：

- 没有线路选择器，接口 `recover` / `retry` 也不接受任何线路参数；
- `uploading` 行只有「重新补投」（走 `retry` → 取消旧 attempt → 立刻重传），没有「只停止、
  不重传」的动作，也就无法释放一个卡住的任务后再决定怎么办；
- 没有线路切换历史（`current_line` 只有当前值，`line_index` 只是失败计数）；
- 上传健康数据 `/v1/health/upload-lines` 已存在但这个页面没有消费，`bldsa` 证书熔断在补传页
  不可见。

**修复方向**：
- 接口层：`recover`/`retry` 接受可选 `line` 参数并严格遵从（与 C 节的统一决策函数对接）；
  新增「停止当前 attempt」接口，内部走 `cancel_registered_attempt` + `fail_enrolled_attempt`，
  终态为 `failed` 而不是继续 `uploading`。
- 数据层：记录每次 attempt 的线路与结束原因（可用一张轻量 attempt 历史表，或在现有行上追加
  一个有上限的 JSON 历史字段），供页面展示「切换历史」。
- 页面：每行加线路下拉（默认「跟随配置」）+「停止」按钮 + 「换线重试」；顶部展示线路健康
  横幅（含 `bldsa` 冷却剩余时间）；错误提示走 B 节的统一映射。

**验收**：能为单个任务指定线路并在日志与页面同时看到该线路生效；停止后状态不再是
`uploading`；`bldsa` 的熔断状态在本页可见。

---

## 建议实施顺序与依赖

1. **A-2 + A-1/B**（租约阶段化 + 接口异步化）：其余修复都建立在「任务不会假死、不会双开」
   之上，必须先做。A-2 与 B 都要动 attempt 状态机，建议同一批改。
2. **C**（统一线路决策）：独立性好，可与 1 并行；但页面线路选择（F）依赖它。
3. **E**（启动扫描 + 按会话恢复接口）：依赖 1 的异步执行机制。
4. **D**（会话连续性）：改动面在 enrollment/monitor，与 1–3 无强耦合，可独立推进；
   涉及数据语义，建议单独设计并补回归测试。
5. **F**（前端控制页）：依赖 B（异步接口）、C（线路参数）、以及 A 节新增的停止语义。

## 已决策的取舍（主人 2026-08-27 定）

整体取向：保守 + 可观测，宁可多等，不可误杀。

- **预处理超时**：按文件大小折算（`10 分钟 + 每 GB 10 分钟`）作硬上限，叠加输出文件字节增长
  心跳；**permit 等待加独立超时**（2 小时，与总时长取齐），保留「先 claim 再排队」以便排队
  期间任务在页面可见。详见 [01](issues/01-attempt-phases-and-lease-convergence.md)。
- **强制线路允许回退**：显式配置线路不在冷却中必须使用，仅冷却时按候选序列回退，且回退原因
  必须在日志与页面留痕。详见 [04](issues/04-unified-line-selection.md)。
- **同场识别用平台 session id**：比开播时间保险。补充事实——**当前所有平台的 `stream.date`
  都是 `Utc::now()`（检测时刻）而非开播时间**（22 处，抖音见
  [douyin.rs:148](crates/biliup/src/downloader/live/douyin.rs:148)），所以开播时间这条路本来
  就不通。场次键的采集单独拆为 [07](issues/07-live-session-key.md)，续接语义改造为
  [09](issues/09-session-continuity.md)。抖音直接用 `room.id_str`，不做跨场实测——本地只留当日
  录像，且续接只认未 finalize 的会话，跨场误合并的窗口本就很窄。
- **attempt 历史建表**：新建 `upload_attempt` 表 + migration，可查可审计。详见
  [08](issues/08-missing-page-controls.md)。

---

## 任务清单

拆分后的 ticket 在 [`issues/`](issues/)。`Blocked by` 为空的可立即开工。

| # | 任务 | 对应本文 | 阻塞于 | 建议模型 |
|---|---|---|---|---|
| [01](issues/01-attempt-phases-and-lease-convergence.md) | attempt 阶段化与租约收割统一 | A-2、A-4 | — | Opus 5 |
| [02](issues/02-async-recovery-endpoints.md) | 补传接口异步化与后台执行落库 | A-1、B（后端） | 01 | Sonnet 5 |
| [03](issues/03-frontend-error-mapping.md) | 前端错误提示统一 | B（前端） | — | Haiku 4.5 |
| [04](issues/04-unified-line-selection.md) | 统一上传线路决策 | C | — | Sonnet 5 |
| [05](issues/05-attempt-diagnostics.md) | 卡住分块的可诊断字段 | A-3 | 01 | Sonnet 5 |
| [06](issues/06-proactive-recovery-and-session-endpoint.md) | 启动主动扫描与按会话恢复接口 | E | 02 | Opus 5 |
| [07](issues/07-live-session-key.md) | 平台场次标识的提取与落库 | D（前置） | — | Sonnet 5 |
| [08](issues/08-missing-page-controls.md) | 补传控制页的线路与任务控制 | F | 02、04 | Sonnet 5 |
| [09](issues/09-session-continuity.md) | 会话续接改用场次键 + 心跳 | D | 07 | Opus 5 |

四个取舍已于 2026-08-27 拍板，见上一节；所有 ticket 的「待确认」段均已替换为决策内容，
无阻塞项。

**2026-08-27 全部实现完毕**，九个 ticket 均已 `Status: resolved`，各自的 `## Answer` 记录了落地方式。
新增两张 migration：`16_add_attempt_phase_and_history.sql`（attempt 阶段/心跳/分块诊断/`upload_attempt` 表）
与 `17_add_live_session_key.sql`（场次键）。

上线前提醒：09 改变了生产库的会话归属语义（哪些分段算同一场），建议先在测试主播上灰度一场再上生产。
