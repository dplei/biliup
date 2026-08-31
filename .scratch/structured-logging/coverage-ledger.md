# 事件覆盖与迁移清单

Status: needs-triage
来源：[实施计划](rollout-plan.md)、[对比流程](reconciliation.md)。
P0 已核对并冻结 coverage-v1 / [contract-v1](contract-v1.md)。P3/12–13 起 **C02–C05 的录制域**与
**C06–C10 的后处理域**已有原生事件；P3/16 那一轮在本机 dev 环境用**一场真实开播录制**跑通了
从开播到投稿成功的整条链路，两域的**决定链与执行结果都已有真实样本**（见下方变更记录）。
**C01、C11–C13 仍未接入**，只有桥接文本，不能把 P1 合成载体验证或桥接采集当作业务覆盖。

## 关键事实

| 编号 | 范围与候选事件 | 新来源至少应能回答 | 对照来源/场景 | 接入批次 |
| --- | --- | --- | --- | --- |
| C01 | system：进程启动/退出 | 哪个进程、版本、启动结果；退出是否正常，强杀没有结束事件不能伪造 | 入口输出、受控启动/退出结果 | 09、14 |
| C02 | recording：开始/停止/关闭 | 哪个主播/录制场次、为什么开始/结束 | 旧录制输出、录制身份和租约结果 | 12 |
| C03 | recording：分段创建/关闭/登记 | 稳定分段身份、原始文件、关闭原因、登记结果；登记前也能关联 | 分段生命周期、登记账本、合成切片 | 12 |
| C04 | `recording.dts_backward` | 影响分段、前后时间值/单位、处理决定；汇总次数/首末/极值 | 旧 DTS 行、受控异常流 | 12 |
| C05 | recording：断流/重连/缺口 | 哪次连接、失败点、退避和恢复、估算缺口与不确定性 | 下载诊断、受控断流与双路交错 | 12 |
| C06 | processing：预处理决定/结果 | 执行/跳过/降级原因、原分段与产物、失败详情 | 旧预处理输出、已知输入与退出码 | 13 |
| C07 | upload：排队/开始/失败/完成 | 哪个上传会话/分段/attempt、线路决定、失败影响及后续恢复 | attempt 历史、旧上传输出、业务结果 | 13 |
| C08 | upload：进度状态 | 当前阶段、确认字节、最后更新时间；快照不证明完整历史 | 现有进度快照/心跳，分块与 watchdog 演练 | 13 |
| C09 | upload：恢复资格/补传 | 为什么允许/拒绝/延后、关联原分段与新 attempt | 恢复审计、资格判定和受控重启 | 13 |
| C10 | submission：决定/尝试/结果 | 为什么等待/拒绝/提交；成功、失败、不确定结果分别是什么 | 投稿意图/claim/业务历史；受控不确定结果 | 13 |
| C11 | auth：健康变化/操作失败 | 失败类型与影响，不泄漏账号凭据 | 受控无效认证/恢复，不使用真实凭据 | 14 |
| C12 | audit：关键人工操作/恢复 | 操作与结果及可靠性边界；durable 审计如何投影 | 已有业务审计/outbox，禁止用通用日志替代 | 14 |
| C13 | diagnostics：外部命令失败 | 退出码、首个致命错误、有界尾部、是否截断 | 合成长 stderr、扫描器结果 | 08、13、14 |
| C14 | observability：存储健康/缺口 | 何时不能写、影响级别/范围、何时恢复；强杀窗口可未知 | 独立健康快照/stderr，忙锁/满盘/强杀演练 | 08、09 |

对每项补充：适用入口/平台、必填与可空字段、业务关联方式、脱敏规则、旧调用点或明确
没有旧来源、预期一对多/多对一映射、回归场景、证据引用和待关闭差异。
进度快照 C08 与存储健康 C14 可由独立数据源回答，不强求每次更新成为普通事件。

### v1 原生目录与旧来源映射

共同字段见契约；R=服务端 live_streamer_id+streamer_info_id；T=独立命令/嵌入/页面上传 task_id；
S=segment_id+original_file；U=upload_session_id；DA/UA=download/upload_attempt_id。
启动时尚未建立的业务身份允许 null；所有缺失保持未知，不按 file/session 文本推断。
路径必须 basename 脱敏，显示名可空；数值时长单位 ms、大小 bytes。结果/原因是结构化字段，
不能再从中文摘要拆取。下表原因是各域首批枚举，后续添加需同步契约/回归，不是自由错误文本。

| 项 | v1 事件名 / 适用入口 | 必填关联/载荷（其余可空） | outcome / reason_code | 旧调用点与映射 / 场景 |
| --- | --- | --- | --- | --- |
| C01 | system.started、system.stopped / 全入口 | process_run_id、app_version；CLI含T | executed/failed/cancelled；startup/shutdown | main.rs、server::_main、lib.rs局部作用域；Rust无显式启动行，不能以缺行推失败；S07 |
| C02 | recording.started、recording.stopped / server及下载命令 | R或T、DA、reason_code | executed/cancelled/failed；live_detected/offline/user_cancel/lease_expired | download::start_download_workflow、monitor::check_room_once；多旧行→一状态变化；S01 |
| C03 | recording.segment_created、recording.segment_closed、recording.segment_enrolled / 所有下载路径 | R或T、S；关闭含size_bytes、reason_code；登记后U/missing_id | executed/failed；split_limit/stream_end/unknown/enrollment_failed | LifecycleFile::create、SegmentEventProcessor、enroll_validated_segment；创建时尚无稳定S，是P3缺口；S03 |
| C04 | recording.dts_backward / 原生HTTP-FLV | R或T、S、previous_ms/current_ms；汇总含count/first_ms/last_ms/max_backward_ms | executed；timestamp_backward | httpflv::parse_flv DTS警告；原文无S，初期1:1，启用汇总才N:1；S03 |
| C05 | recording.disconnected、recording.retry_scheduled、recording.reconnected / 全下载 | R或T、DA；delay_ms/silent_ms/gap_ms（测不到gap可空） | failed/waiting/recovered；read_timeout/transport_error/stream_end | Connection::read_frame、StreamGears::start_download、download重试；外部进程无FLV gap测量，N:1；S02/S03 |
| C05 | recording.hls_gap、recording.hls_discontinuity / 原生 HLS | R或T、DA、S、media_sequence；gap另含previous_media_sequence/missing_segments | executed；media_sequence_gap/hls_discontinuity | hls::download_inner 的旧 skipped/discontinuity 警告；序列缺口不是毫秒缺口，不连续指向新文件；S03 |
| C06 | processing.decided、processing.completed / server上传预处理 | R、S、UA；artifact_file可空、duration_ms可空 | executed/skipped/fallback/failed；disabled/no_audio/low_disk/probe_failed/invalid_output | normalize_for_upload、normalize_timestamps、process_segment_event；不同工具分别stage，N:1；S04 |
| C07 | upload.queued、upload.started、upload.failed、upload.completed、upload.line_decided / server/CLI/Python | R或T、S、U（CLI可空）、UA（排队可空）；line可空 | executed/failed/succeeded/fallback；configured/automatic/cooldown/network_error | upload::upload_enrolled_with_watchdog、upload_single_file、decide_upload_line、Parcel/Upos；1个attempt多事件；S05 |
| C08 | 不追加普通事件 / 上传入口 | S、UA、phase、confirmed_bytes、updated_at_ms来自业务快照 | 不由日志推断终态 | UploadActivity、record_heartbeat；旧16MiB/5秒INFO为N:1快照映射；S01/S05 |
| C09 | upload.recovery_decided、upload.recovery_started / 服务补传 | R、S、U、missing_id；开始有新UA | executed/skipped/waiting/failed；eligible/source_missing/lease_active/retry_due | check_recovery_eligibility、claim_manual_recovery、run_claimed_recovery；原分段不换ID；S05 |
| C10 | submission.decided、submission.started、submission.completed / server/CLI/Python | U或T；R可空；pending_count可空 | waiting/failed/succeeded/unknown；pending_segments/no_intent/remote_error/missing_remote_id | reconcile_session_submission、submit_failed/submit_ok_with_aid/submit_ok_no_aid；桥接不得保存resp原文，N:1；S06 |
| C11 | auth.health_changed、auth.operation_failed / server/登录CLI/嵌入 | platform、stage；T可空 | failed/recovered；authentication_failed/authentication_recovered | cookie_health::record_error/record_success、Python login helpers（无局部subscriber，继承宿主）；不写账号、cookie路径；S10 |
| C12 | audit.operation_projected / server | 稳定event_uid、stage；关联随操作存在 | executed/failed；manual_recovery/finalized/source_missing | record_recovery_audit及业务outbox为权威，旧文本可能无独立对应；幂等投影N:1；S05/S07 |
| C13 | processing.command_failed / 外部工具 | stage、exit_code（信号退出可空）、diagnostic_id；S可空 | failed/fallback；process_failed | ffmpeg_scan::run_scanning_stderr 有界尾部；首致命信息并非旧扫描器保证，需新采集；S10 |
| C14 | 独立health（可投影system.storage_recovered） / 全入口 | queue_depth/bytes、dropped分级、storage_failures、last_commit_ms、committed_id | unknown/recovered；storage_unavailable/queue_full | 旧无统一来源；强杀范围未知；S09 |

Python login/send_sms/qrcode helpers 没有独立初始化；作为宿主继承路径列入C11，不能假定
与Python upload共享subscriber。弹幕、封面抓取、hook失败作为 recording/processing 诊断，
P3/14须补原生事件及真实场景；P0不删除任何未迁移输出。

## 支持入口与路径

06 核对实际支持范围，不因「当前只用一种」跳过其他对外入口；未支持范围必须公开注明。

| 入口/路径 | 必查边界 | 当前状态 |
| --- | --- | --- |
| wheel CLI 主循环 | 全局subscriber；stdout + ds_update日期.log，daily最多7个，RUST_LOG（缺省info）总过滤，秒级本地时间；guard退出flush，Web可reload总过滤 | P2实测：旁路三态通过，旧输出不变；桥接文本重复行不可判定 |
| Rust CLI | 全局subscriber；仅stdout，--rust-log缺省tower_http=debug,info，秒级本地时间，Web可reload，无文件guard | P2实测：旁路三态通过；无持久旧文件，受控对照用wrapper stdout |
| Python 下载函数 | 每次with_default局部subscriber，stdout + download.log不轮转，默认INFO，秒级；guard退出flush，worker绑定同一dispatch | P2实测：重叠调用共享run、不覆盖宿主subscriber；顺序调用排空后新建run，旧文件照常 |
| Python 上传函数 | 每次with_default局部subscriber，stdout + upload.log不轮转，默认INFO，秒级；current-thread runtime，guard退出flush | P3/14：独立任务事件已接入，重复调用的缺凭据/三态/回退实跑通过；远端正常链待补验 |
| 页面整场上传 | `POST /v1/uploads` 接受请求后后台执行；返回 task 并传至每个输入/attempt/投稿，首文件反查只用于模板 | P3/14 第二批：Rust/wheel HTTP 三态、并发、关联查询和回退通过；Rust 页面真实上传/私密投稿通过；不改变页面默认或旧 sink |
| HTTP-FLV/HLS | Rust CLI按扩展名选FLV/HLS；Python/server 的已知m3u8/ts直接HLS，其余保留FLV探测回落 | P3/14第三批：HLS复用S，新增序列缺口/不连续与失败事件；三下载入口受控三态/回退通过，CLI提取结果为fixture；服务执行器的媒体后重连与取消通过，真实平台HLS及服务整链待验 |
| 外部下载 | server from_type仅显式Ffmpeg走外部，其余落StreamGears；Streamlink/YtDlp运行时实现存在，但当前from_type不选；CLI对这些hint明确拒绝 | 不冒称可用；P3/14须复核实际可达性 |
| 正常上传与重启/补扫/人工补传 | durable enrollment有missing/U/order，attempt各自独立；正常分段也登记；日志不能代替账本 | 源码核对；P3验原生 |

CLI 命令集合：login/renew/upload/append/show/comments/reply/dump-flv/download/server/
cover-preview/list/backfill-lifecycle（以Cli::Commands为准）；wheel同枚举，非server不能依赖
业务库来初始化日志。CLI extractor注册：Acfun/AfreecaTV/Bigo/Bilibili/CC/Douyin/Douyu/Huya/
Inke/Kilakila/Kuaishou/Missevan/Niconico/Picarto/TTingLive/Twitcasting/TwitchVideos/Twitch/
Youtube/YY/General；注册不等于外部运行时路径已可达，不宣称平台线上验证通过。

旧读取入口：ws::ws_logs白名单映射ds_update/download/upload，末尾50行+500ms轮询；
logviewer按文件tab，静态下载及developer日志设置均保持不变。没有stdout持久化的保证。

## 变更记录格式

每批接入后追加 `覆盖项 / 入口 / 契约版本 / 接入任务 / 对比批次别名 / 结论 / 未解决差异`。
真实映射和运行细节不写入本文件；公开结论只引用脱敏场景与匿名报告编号。
业务原生覆盖自 P3/12 起有第一条通过记录（录制域）。新增路径、新事件或必填字段变更后，相关条目回到待验证。

- C01–C14 / 合成调用者 / v1 / 06–08 / synthetic-v1：P0来源映射和P1载体/存储能力通过，
  [P0](receipts/P0.md)、[P1](receipts/P1.md)；不计为业务迁移覆盖，所有实际入口仍待P2/P3。
- C13 / 独立DiagnosticCapture / v1 / 08 / synthetic-v1：有界长行/尾部、跨chunk凭据脱敏、
  首致命信息、7天附件清理和超限回滚通过。旧ffmpeg扫描器未改，实际工具接入待13/14。
- C14 / 独立Runtime+SQLite / v1 / 07–08 / synthetic-v1：队列计损、忙锁/只读/满页/低盘/WAL
  降级及恢复通过，强杀已提交可查、未提交窗口明确未知；真实入口健康暴露待09。
- C01–C14 / 5个支持入口 + 本地dev server / v1 / 09–11 / shadow-v1：仅**桥接**链路通过，
  [P2](receipts/P2.md)。开启/关闭/新库不可用三态、回退、真实HTTP与旧日志页、双路负载、
  证据导出与确定性校验均通过；所有 manifest 固定 native_coverage=not-started，
  **本行不为任何覆盖项计原生分**，C01–C14 的业务原生事件仍未接入，待 P3/12–14。
- 比较规则边界 / 桥接文本 / reconciliation-v1 / 11 / shadow-v1：逐字重复的旧行与脱敏后的
  `[REDACTED]` 行无法按文本配对，工具判 insufficient（0 缺失）。这是已知限制，不是缺陷；
  原生事件带稳定身份后不再依赖文本配对。
- 证据包元数据 / 导出器 / evidence-v1 / 10–11 / shadow-v1：独立还原与交叉复核发现声明时区被
  按路径规则脱敏、两源时间无法对齐（差异 D04）。已改为受控时区标识符（合法值保留、非法值判
  unknown 并计入不完整原因）、补回归用例并重新采包复核通过；修复前的包保留原结论。
- C02–C05 / Python 下载函数 + 服务端拉流执行器 / v1 / 12 / recording-native-v1：**原生**通过。
  分段身份在 `LifecycleFile::create` 分配，经关闭回调、`SegmentInfo` 与登记事务（加法迁移、
  可空列）持久化；受控演练覆盖两次完整过程、双路交错、连续切片、传输失败断连与人工构造的
  DTS 异常，证据包 complete、校验 passed、7 条预期事实全部 confirmed，见 [P3](receipts/P3.md)。
  **未解决差异**：服务端整场循环（`recording.started/stopped/retry_scheduled`）缺真实开播样本，
  只有编译与集成测试证据；外部下载器与 HLS 路径未接入身份（保留待接入标记，归任务 14）；
  C01、C06–C13 不因本行计任何原生分。
- 切片关闭原因 / 原生 FLV 路径 / v1 / 12 / recording-native-v1：既有缺陷——原因在计数复位之后
  才取值，配置切片的关闭原因恒为 `Unknown`。已改为复位前取值并加回归。**业务可见副作用**：
  线路健康的 `completed_configured_segment` 此前恒为假，修复后完成配置分段才计为稳定尝试。
- 契约细化 / Fields / contract-v1 / 12 / recording-native-v1：空字符串 ID 表示「调用方没有该身份」，
  既不入库也不计 rejected；格式非法的 ID 仍计 rejected。已加双向回归。
- 导出目录形态 / 导出器 / evidence-v1 / 12 / recording-native-v1：目录改为「多种合法形态取其一」，
  `recording.dts_backward` 的 1:1 与汇总形态都合法，半个形态仍判缺字段；已加回归。
- C06–C10 / 服务端后处理链 / v1 / 13 / upload-native-v1：**原生事件已接入，验收 partial**。
  `UploadIdentity` 从登记结果或 lifecycle 行构造，分段身份不被 attempt 覆盖，失败事件不被
  后来的成功改写。受控演练（本地 sqlite、无账号、无网络）跑通登记 → 投稿判定
  （no_intent / pending_segments 含 pending_count）→ 补传资格（eligible / source_missing），
  证据包 complete、校验 passed、5 条预期事实全部 confirmed，见 [P3](receipts/P3.md)。
  **未解决差异**：预处理执行结果、上传排队/线路/开始/失败/完成、投稿 started/completed
  只有编译与单元证据，**没有真实远端运行样本**（需要账号与实际投稿，应在 dev 环境实跑）；
  页面/CLI 的整场 `upload()` 入口尚未接入，归任务 14。C08 按设计不追加普通事件。
- 原因词表扩展 / 预处理与补传 / contract-v1 / 13 / upload-native-v1：C06 追加
  `measure_failed`、`invalid_measurement`、`transcode_failed`、`low_disk_aborted`、
  `source_missing`、`normalized`；C09 追加 `already_succeeded`、`legacy_finalized_edit`、
  `invalid_media`、`conflict`、`manual_recovery`、`retry_due`；C07 的失败原因直接复用既有
  `UploadFailureKind` 与 watchdog 三类超时代码；C10 追加 `claimed_elsewhere`、`finalized`、
  `discarded_empty`、`retry_backoff`、`writeback_failed`、`submitted`、`precondition_failed`。
  全部由映射函数集中维护并有冻结性回归用例，不是自由错误文本。
- 查询与认证边界 / 事件库只读接口 / log-events-v1 / 15 / query-api-v1：`/v1/log-events` 列表、
  附件详情、SSE 实时接续与 JSONL/CSV 导出通过；默认只回原生、桥接需显式请求、采集关闭与
  「查到 0 条」明确区分；2 万条负载下五类查询均在冻结的 250ms 预算内且零丢弃。
  **行为变更**：旧 `/v1/ws/logs` 由守卫外挪入守卫内——守卫态五个入口（含旧 ws）全部 401，
  关闭认证的部署行为不变。**未解决差异**：导出并发上限未设、WAL 回收未量化观测。
- 查询集合过滤与倒序 / 事件库只读接口 / log-events-v1 / 16 / preview-ui-v1：加法参数
  `levels`、`categories`（精确集合，`level IN (...)`，空元素报 400）与 `order=desc`
  （响应给 `next_until_id` 往回翻）。集合大小与元素长度在 `push_filters` 统一设界，
  `count` 与分页受同一约束；**实时接续与导出恒为正序**，旧调用不改一字仍是原行为。
  加这三个参数是因为页面文稿要求「默认最新在前」和「分别看信息/警告/错误」，
  原来的 `id > after_id ORDER BY id` 与 `min_level` 表达不了，见 [P3](receipts/P3.md)。
- C02–C10 / 本机 dev server 一场真实开播录制 / v1 / 12–13 / dev-live-v1：**原生通过**，
  补上第一轮列为待补验的两项。真实平台、真实账号、仅自己可见模板：`recording.started/
  stopped` 各 1、17 个分段的创建/关闭/登记、16 条真实 DTS 倒退、真实响度标准化的
  `processing.decided/completed`、18 次上传排队/线路/开始与 17 次完成、1 次真实失败、
  1 次真实人工补传（`eligible` → `manual_recovery`）、`submission.started/completed`
  各 1（`succeeded/submitted`）。采集健康无丢弃、无写入失败。见 [P3](receipts/P3.md)。
  **未解决差异**：本场没有断连，`recording.disconnected/retry_scheduled/reconnected`
  仍无真实样本；真实 601 限流失败的 `reason_code` 落在 `transport`，具体原因只在自由文本
  `error` 里——「新来源至少应能回答失败类型」这一条上分辨不出限流与网络错误，归 13/14 处理；
  外部下载器、HLS 与页面/CLI 整场 `upload()` 仍未接入，归 14。
- 试用页面 / 新事件页 / log-events-v1 / 16 / preview-ui-v1：**页面验收通过**（真数据、浏览器实测）。
  级别颜色/文字/图标、联合筛选与整段命中数、行内详情、场次范围与返回原位置、阅读冻结与
  新事件缓冲、暂停与游标补齐、历史翻页、按当前筛选导出、运行进度与跳转、窄屏与深浅主题、
  键盘可用，以及「采集未开启 / 连不上服务端 / 查到 0 条」三态区分均实测通过。
  默认入口开关初值仍是旧页（`LOG_EVENTS_IS_DEFAULT`），17 只改这一个常量即可切换与回退。
  **有意的取舍**：同一维度多选只覆盖级别与业务类型（关联字段后端只支持单值精确匹配）；
  关联筛选必须带运行实例（后端约束，页面取最近事件所在实例并说明作用范围）；
  界面不对来源已汇总的事件再折叠一层；用 500 条有界缓存而非虚拟列表。
- 存储 schema / 事件库 / v1 / 15 / query-api-v1：加 `capture_kind`、`message` 两列并从 payload
  回填（加法迁移，旧版本写出的库仍可查）。否则「默认只看原生」与关键词过滤只能全表扫 JSON。
- 进度页补传入口 / 试用页面 / log-events-v1 / 16 / 样式补修：两个导航链接统一主题按钮外观，
  浏览器复核深浅主题、桌面/360px、访问后样式、键盘焦点与跳转；无新增观测能力，C01–C14
  覆盖结论及默认页面开关不变。验证边界见 [P3 回执](receipts/P3.md#任务-16-样式补修)。

- C07/C09/C10 / Rust CLI、wheel CLI、Python 上传函数 / contract-v1 / 14 入口上传批次 /
  standalone-upload-v1：**实现已接入，验收 partial**。CLI 命令上传、配置逐稿上传、append
  与 Python 上传分别持有 task；预上传重试单独 UA，排队/线路/开始/失败/完成可关联。
  文件缺少录制账本时用 T + original_file + segment_order + UA，序号只在当前任务有效；
  有录制账本的服务端 S 形态不变。checkpoint_reused 只表示复用，不补造本次成功事件。
  新增理由：preparing_upload/files_ready/no_input/config_dispatch/config_failed/cover_failed/
  target_lookup_failed/upload_failed/authentication_failed/storage_unavailable/line_selection_failed/
  awaiting_pre_upload/source_io/lock_failed/rate_gate_unavailable/rate_limited/invalid_response/
  remote_error/checkpoint_reused/transferred/append_ready/appended/request_failed；既有理由继续有效。
  缺凭据三入口 × 三态、CLI 三种命令各两次、每入口关闭回退、三份双源包确定性校验通过；
  合成载体验证成功/失败/不确定结果与交错身份，**不等于远端链路已运行**。
  页面整场上传仍走无 task 的旧入口；录制期上传的 `transport` 限流差异仍未改，本批不计
  C01/C11/C12/C13 覆盖。详细证据及限制见 [本批回执](receipts/P3.md#任务-14-第一批独立上传入口)。
- 入口生命周期文字更正 / P2–P3：`Shadow` 只在 guard 重叠时共享运行实例；最后一个 guard
  退出会排空，之后的顺序调用新建 process_run_id。历史「多次调用共享run」不适用于这种
  顺序调用；本批重新实测了此边界，不更改底座或伪造相同 run。

- C07/C10 / 页面整场上传 / contract-v1 / 14 第二批 / page-upload-v1：task 在请求进入时分配，
  在响应和后台生命周期间关联；`upload_with_task` 复用首批传输事件，`UploadTask::submit`
  复用成功/未知结果判断。新增原因 `studio_build_failed/no_videos`。任务接受不等于完成，
  无输入与模板构建失败的完整后续分支尚无运行样本；测试只覆盖空输入在认证失败前的旧行为。
  Rust/wheel 各三态、并发重复调用、关闭回退与两份证据包确定性校验通过；一次 Rust 页面
  实际远端上传→投稿成功链通过，同一 task/UA/输入序号可答，远端只读回查确认仅自己可见。
  首批 CLI/Python 的远端成功/恢复仍待补验，不因共用代码而代计；仍不计 C01/C11–C13。
- 隐私必要补修 / 桥接与证据导出 / contract-v1 / 14 第二批：真实成功样本揭示
  `ResponseData` 中数字稿件编号未被旧正则清理；现按原始响应整值脱敏，保留其他独立成功行。
  合成回归通过，原包/视图已标记撤回、在私有目录用修复后的导出器重导；原生成功样本采集
  早于这次脱敏补修，不当作最终采集版本的长观察证据。见
  [P3 第二批](receipts/P3.md#任务-14-第二批页面整场上传)。
- C02/C03/C05 / HLS 的 Rust CLI、wheel CLI、Python 下载及服务执行器 / contract-v1 / 14 第三批 /
  hls-entries-v2：原生序列缺口/不连续/失败与 T/DA/S 关联通过；三下载入口各正常两次、非法
  列表、HTTP 错误 × 采集三态、关闭回退及三份包校验通过。CLI General 消费受控提取结果，
  不宣称 yt-dlp 或在线平台通过。服务执行器测试 R/DA/S 回调、实收媒体后重连、用户取消。
  `media_sequence/previous_media_sequence/missing_segments` 为片段序列/数量，不是时间；
  追加原因 media_sequence_gap/hls_discontinuity/invalid_playlist/http_error/source_io，
  通用传输错误/read_timeout 保留。独立 stopped 仍以 T 表示终态，不要求 DA。
  旧 `SKIPPED` / `DISCONTINUITY` 文案保留；新源可区分丢片数量和输出文件，人工抽查通过，
  未做新的三次隔离还原。HLS 真平台/服务整链/真实断连恢复、双路并发和量化负载待补；
  C04 不因 TS 事件算 DTS 验证，C01/C11–C13 和外部下载器不计覆盖，见
  [P3 第三批](receipts/P3.md#任务-14-第三批hls-下载)。
