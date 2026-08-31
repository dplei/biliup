# 事件覆盖与迁移清单

Status: needs-triage
来源：[实施计划](rollout-plan.md)、[对比流程](reconciliation.md)。
P0 已核对并冻结 coverage-v1 / [contract-v1](contract-v1.md)。以下业务原生事件仍均为
**未接入、未通过业务运行验证**，不能把 P1 合成载体验证当作业务覆盖。

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

共同字段见契约；R=服务端 live_streamer_id+streamer_info_id；T=独立命令 task_id；
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
| Python 下载函数 | 每次with_default局部subscriber，stdout + download.log不轮转，默认INFO，秒级；guard退出flush，worker绑定同一dispatch | P2实测：多次调用共享run、不覆盖宿主subscriber；旧文件照常 |
| Python 上传函数 | 每次with_default局部subscriber，stdout + upload.log不轮转，默认INFO，秒级；current-thread runtime，guard退出flush | P2实测：缺凭据早退路径通过，多次调用不重复消费；旧文件照常 |
| HTTP-FLV/HLS | Rust CLI按扩展名选FLV/HLS；Python读头失败回落HLS；server StreamGears同样探测。稳定segment_id尚缺 | 源码核对；P3验流/回调 |
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
业务原生覆盖目前没有通过记录。新增路径、新事件或必填字段变更后，相关条目回到待验证。

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
