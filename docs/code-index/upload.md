# 上传、投稿与补传

[返回根导航](../../CODE_INDEX.md) · [维护约定](../agents/code-index.md)

分段登记、上传线路、attempt、投稿会话、恢复资格、重试与 B 站协议。

**从这里开始：** 服务端先看 `common/upload.rs`；投稿状态看 `upload_session.rs`，补传扫描看 `recovery_scheduler.rs`，独立命令看 `uploader.rs`。

表内路径均相对仓库根目录。职责与关系继承自已有索引；修改前仍需核对当前源码。

## 文件职责

| 文件 | 主要作用 | 关键符号 |
| --- | --- | --- |
| `crates/biliup/src/uploader/credential.rs` | 账号登录、cookie/token 校验与刷新；token 校验的发送与 JSON 解码错误移除请求 URL，保留 typed 原因供各入口处理。 | `Credential`、`login_by_cookies`、`validate_tokens`、`renew_tokens` |
| `crates/biliup-cli/examples/upload_pilot.rs` | P3/13 的受控后处理演练：本地 sqlite 跑真实登记、投稿判定与补传资格判定，两个 sink 同时输出，并生成证据包请求与预期事实清单。无账号、无网络。 | `drill`、`write_evidence_request` |
| `crates/biliup-cli/src/uploader.rs` | 独立 CLI 上传、配置逐稿投稿、追加与登录工具；保留 checkpoint 和限流锁，上传调用显式传 task，预上传重试分别分配 attempt。 | `upload_by_command`、`upload_by_config`、`append`、`upload_with_task`、`UploadCheckpoint` |
| `crates/stream-gears/src/uploader.rs` | Python 上传参数到上传/封面/投稿的编排，显式传递同一 task，复用服务端线路及持久限流准入。 | `StudioPre`、`upload` |
| `crates/biliup-cli/src/server/common/upload.rs` | 编排直播分段上传、会话级幂等投稿协调、缺失补传、attempt lease/watchdog（分阶段计时 + 传输阶段滑窗判速）、线路决策入口以及远端结果落库；滑窗判慢中止使用独立慢速失败类别，但另有线路正因慢冷却时判为整机出口限速、本次 attempt 锁定为不中止且豁免总时长上限（issue #82），聚合诊断只承诺最近确认分块；两个服务端入口共用预上传瞬时网络重试，分段成功会在事务外唤醒有 durable 投稿意图的父会话。投稿未指定接口时先走 App，只有结构化 21566 明确拒绝才在同一 attempt 降级一次 Web；显式接口严格、非法配置不发请求。明确拒绝走 60 s 起的普通退避；内容类拒绝码（`FINAL_REJECTION_CODES`，21588）或远端拒绝连续 `MAX_SUBMIT_REJECTIONS` 次则停止自动重投（`held`，issue #76），本地前置失败不计入；传输/响应不确定则持久化 `unknown_remote_result` 并保留 claim，禁止自动重投。v2 分段失败按 `retry_delay_for_attempt` 退避，时间戳 `Unfixable`（错误带 `UNFIXABLE_UPLOAD_ERROR`）一次用完自动重试次数。 | `process_with_upload`、`reconcile_session_submission`、`submit_to_bilibili`、`SubmitFailure`、`stops_auto_submit`、`fail_enrolled_attempt_with_outcome`、`preserve_unknown_submission`、`spawn_session_submission`、`persist_segment`、`decide_upload_line`、`pre_upload_with_retry`、`retry_pre_upload_requests`、`upload_enrolled_with_watchdog`、`claim_manual_recovery`、`run_claimed_recovery`、`stop_missing_segment_attempt`、`segment_part_title` |
| `crates/biliup-cli/src/server/common/upload_rate_gate.rs` | 维护服务端进程级预上传节流、601 持久冷却和冷却结束后的单探测状态；每次真实预上传请求都必须单独准入并在返回后收口。 | `before_pre_upload`、`record_success`、`record_rate_limited`、`record_non_rate_limit_failure`、`UploadGateState` |
| `crates/biliup-cli/src/server/common/attempt_lease.rs` | 定义 attempt 的三个阶段与各自的收割判据，提供心跳/阶段落库和 `upload_attempt` 历史表的读写。 | `AttemptPhase`、`classify_stale_lease`、`preprocess_deadline`、`record_heartbeat`、`close_attempt_history` |
| `crates/biliup-cli/src/server/common/upload_line_selection.rs` | 全仓统一的线路规划与解析：AUTO 先探测取回白名单，再放宽范围，保留两轮 typed 失败；最终失败把列表挂到 Report，告警分别描述降级动作与选中线路的已验证取回能力；显式线路只绕过普通 probe 冷却。 | `plan_upload_line`、`resolve_planned_line`、`probe_with_fallback`、`RECOVERABLE_LINES`、`log_line_decision` |
| `crates/biliup-cli/src/server/common/recovery_scheduler.rs` | 到期补传扫描与后台执行：按会话串行、按分段顺序领取已到期且未达自动次数上限的行，领取前选线失败不启动上传、由扫描周期再次尝试；人工恢复会话越过退避与次数上限。 | `start_due_recovery_scan`、`recover_due_segments`、`spawn_claimed_recovery` |
| `crates/biliup-cli/src/server/common/submission_scheduler.rs` | 数据库驱动的待投稿会话启动/周期扫描：只选有持久投稿意图、无 claim 且已到期的会话，以有界并发唤醒统一协调器并分类记录结果；`ok_no_aid`、`submitting`、`unknown_remote_result` 即使异常丢了 claim 也防御性排除，避免重复投稿；`held`（远端拒绝后停止自动重投）同样排除，只等人工恢复。 | `start_submission_reconciliation_scan`、`scan_due_submissions`、`due_submission_session_ids` |
| `crates/biliup-cli/src/server/common/segment_enrollment.rs` | 在有效媒体进入内存队列前原子登记 session/分段 identity，按场次键续接会话（时钟窗口只作缺键兜底），并在数据库不可用时写 fsync outbox。后台 importer 启动时全量分页重放持久恢复审计，运行中重试最近窗口；事件库按稳定 uid 去重。 | `enroll_validated_segment`、`find_or_create_session`、`import_outbox_once`、`spawn_outbox_importer` |
| `crates/biliup-cli/src/server/common/recovery_eligibility.rs` | 统一补扫、静默恢复和人工恢复的只读资格判定，并负责把消失源文件收敛为终态、记录 finalized 审计。审计业务行先持久化稳定 event uid，提交后 best-effort 投影；有界重放会给遗留 NULL uid 回填后始终复用同一 uid，不承诺跨业务库/事件库原子性。 | `check_recovery_eligibility`、`mark_source_missing`、`record_recovery_audit`、`replay_recovery_audits` |
| `crates/biliup-cli/src/server/common/upload_session.rs` | 维护投稿会话恢复（场次键优先、时钟窗口兜底）、单调持久投稿意图与退避时间（远端拒绝后 `hold_session_submit_after_rejection` 置 `held`、就绪预检返回 `Held`；人工恢复 `rearm_session_submit` 越过退避、清零计数、解除 `held`），并以生命周期账本检查完整性、确定性重建分 P 和原子 claim/finalize；下播后 `BLOCKED_RECHECK_INTERVAL` 内仅有在飞行（pending/uploading）的阻塞判为 quiet，不写 `blocked_signature`；自动路径只把关闭态零基线会话终结为 `discarded_empty`，人工路径可在无在飞分段、无投稿 claim/远端身份/不确定结果时原子终结为 `discarded` 并保留账本。 | `SessionCompleteness`、`request_session_submit`、`session_submit_readiness`、`discard_empty_session`、`discard_session`、`schedule_submit_retry`、`mark_submit_anomaly`、`hold_session_submit`、`select_recovery_candidate`、`reusable_streamer_info`、`touch_session_activity`、`claim_complete_session`、`BLOCKED_RECHECK_INTERVAL` |
| `crates/biliup-cli/src/server/common/lifecycle_backfill.rs` | 把历史会话的 videos_json 与遗留 missing 行回填成 v2 生命周期账本，合并重复源、生成 legacy:// 基线并按会话断点续跑。 | `run_lifecycle_backfill`、`plan_session`、`BackfillPlan` |
| `crates/biliup-cli/src/server/common/upload_line_health.rs` | 分类网络错误并持久维护线路冷却和到期探测租约：普通 AUTO 探测错误单列 Probe，明确证书错误走 TLS 冷却；成功吞吐 EWMA 与慢传 watchdog 共用相对基线，其它线路的慢冷却充当整机限速证据；全部线路冷却时按间隔原子领取一条探测失败冷却做半开探测。 | `UploadFailureKind`、`classify_kind`、`acquire_line`、`claim_half_open_probe`、`record_failure`、`record_success` |
| `crates/biliup-cli/src/server/common/missing_segment.rs` | 分段补传状态机：入队、按状态计数与 stale-uploading 健康快照、后台自愈租约（先取消进程内 attempt 再落库）与 v1 重试状态转换；`MAX_AUTO_UPLOAD_ATTEMPTS` 与 `retry_delay_for_attempt` 是 v1/v2 共用的自动重试上限与退避表。`upload_missing_segment` 的 v1 行是失败/补救队列，v2 行是所有已验证分段的持久账本；当前健康以 `status` 为准，`last_error` 只存真实失败诊断，成功转换必须清空它；`pending`、`failed`、`source_missing` 可进入幂等文件清理与删除 claim。 | `missing_segment_health`、`recover_stale_upload_attempts`、`start_stale_attempt_recovery`、`claim_missing_segment_for_delete`、`enqueue_pending_segment`、`mark_retry_success` |
| `crates/biliup/src/uploader/line.rs` | B 站线路索引、过滤后的 AUTO 探测、预上传与确认分块进度：候选为空与全部请求失败分别返回 ProbeFailed 并保留失败列表，探测错误去掉请求 URL 后记录底层原因与耗时；Parcel 在上传前导出取回描述符。 | `Probe::probe_filtered_with_failures`、`Probe::probe_line`、`choose_line_and_failures`、`retained_lines`、`Parcel::recovery` |
| `crates/biliup/src/uploader/line/upos.rs` | upos 协议的分块 PUT 实现：并发窗口、单请求超时与有限重试（每次失败带线路/分块号/耗时的结构化日志），最后汇总分片完成上传。另定义 `UposRecovery`（endpoint + upos_uri + auth）——上传完还能把源对象取回来所需的一切，**只能在 preupload 时拿到**，事后重新申请是 403。**含凭证，不得进日志/事件/告警。** 取回是否可行按线路而定：实测 tx/bda2/alia 的 GET 逐字节一致，bldsa 只给 HEAD 200、GET 403。 | `Upos::upload_stream`、`CHUNK_REQUEST_TIMEOUT`、`Upos::get_ret_video_info`、`UposRecovery`、`UposRecovery::object_url`、`Bucket::recovery` |
| `crates/biliup/src/uploader/bilibili.rs` | App、Web、BCut 三条投稿请求及公共响应结构；非零投稿响应保留为 `SubmitRejected { code, message }`，让上层只对明确拒绝做安全路由，不再解析 Debug 字符串。传输与响应解析错误保持原类型。另有视频合集接口：列合集、列分区内已有 aid、把稿件加入分区。 | `BiliBili::submit_by_app`、`BiliBili::submit_by_web`、`BiliBili::submit_by_bcut_android`、`ResponseData`、`BiliBili::list_seasons`、`BiliBili::list_section_aids`、`BiliBili::add_archive_to_season` |
| `crates/biliup/src/error.rs` | 核心库公共错误边界：保留 Reqwest 原因链、投稿明确拒绝的错误码，以及全部线路探测失败的 typed 诊断列表，供服务端分类与持久健康更新。 | `Kind`、`Kind::ProbeFailed`、`Kind::SubmitRejected` |
| `crates/biliup-cli/src/server/api/bilibili_endpoints.rs` | `/bili/*` B 站代理端点。`/bili/seasons` 查 section_id；`/bili/seasons/backfill` 按主播把 `upload_session` 里的历史 aid 补进其生效配置（全局 + override）的 `season_section_id`，GET 预演、POST 执行，已在合集里的跳过，可重复跑。 | `get_seasons_endpoint`、`season_backfill_preview_endpoint`、`season_backfill_run_endpoint`、`backfill_plan` |

## 高信号关系

以下箭头包含控制流、数据流和跨语言边界，具体含义以边后的说明为准；不等同于完整函数调用图。

- `crates/biliup-cli/src/server/common/upload.rs` → `crates/biliup-cli/src/server/common/upload_line_selection.rs`（`decide_upload_line`）：各上传入口共用同一决策，成功与失败路径都经 `finish_line_probe` 持久记录去重失败，保留 TLS 优先级并跳过本次已恢复的选中线路。
- `crates/biliup-cli/src/server/common/upload.rs` → `crates/biliup-cli/src/server/common/upload_rate_gate.rs`、`crates/biliup/src/uploader/line.rs`（`pre_upload_with_retry`、`before_pre_upload`、`Line::pre_upload`）：两个服务端上传入口共用 typed 瞬时网络重试；每次真实预上传请求都单独经过并收口全局 gate，预上传失败不再污染所选 UPOS 线路的 breaker。
- `crates/biliup-cli/src/server/common/upload_line_selection.rs` → `crates/biliup-cli/src/server/common/upload_line_health.rs`（`cooling_lines`）：决策把持久冷却状态当作唯一的「这条线不能用」依据（全部冷却时经 `claim_half_open_probe` 半开试探一条）；成功、网络错误或传输阶段的 watchdog 超时反过来更新它。
- `crates/biliup-cli/src/server/common/missing_segment.rs` → `crates/biliup-cli/src/server/common/attempt_lease.rs`（`classify_stale_lease`）：收割循环与健康接口共用同一份分阶段判据，避免页面显示与后台行为不一致。
- `crates/biliup-cli/src/server/common/missing_segment.rs` → `crates/biliup-cli/src/server/common/upload.rs`（`cancel_registered_attempt`）：收割一条仍被本进程持有的租约时，先真正取消并等它退出，再 CAS 落库，杜绝幽灵上传。
- `crates/biliup-cli/src/server/common/recovery_scheduler.rs` → `crates/biliup-cli/src/server/common/upload.rs`（`claim_manual_recovery`）：主动扫描与按会话恢复复用手动补传的资格判定和 claim，只是把执行搬到后台任务。
- `crates/biliup-cli/src/server/common/submission_scheduler.rs` → `crates/biliup-cli/src/server/common/upload.rs`（`reconcile_session_submission`）：启动与周期扫描按数据库投稿意图选出到期会话，以有界并发唤醒同一协调器；跨事件/扫描去重仍由持久 submit claim 保证。
- `crates/biliup-cli/src/server/common/upload.rs` → `crates/biliup-cli/src/server/common/upload_session.rs`（`session_submit_readiness`、`claim_complete_session`、`schedule_submit_retry`）：分段成功和多类结束事件只唤醒统一协调器；协调器按意图/退避预检并共用严格完整性 claim 闸门，明确失败释放 claim，不确定远端结果保留 claim。
- `crates/biliup-cli/src/server/common/segment_enrollment.rs` → `crates/biliup-cli/src/server/common/upload_session.rs`（`submit_claim_token`）：提交 claim 关闭 enrollment 写入窗口；迟到分段在 finalized 边界写入审计而不污染正在投稿的分 P 快照。
- `crates/biliup-cli/src/server/common/upload.rs` → `crates/biliup-cli/src/server/common/segment_enrollment.rs`（`rescan_local_valid_segments` → `enroll_validated_segment`）：本地补扫先只读验证候选，首条有效未知媒体才通过统一 enrollment 事务创建或复用会话；零有效候选不写会话表。
- `crates/biliup-cli/src/server/common/upload.rs` → `crates/biliup-cli/src/server/common/recovery_eligibility.rs`（`check_recovery_eligibility`）：补扫、静默补传和人工操作共用 finalized/source-missing/succeeded 的准入结果，防止 closed session 产生新任务。
- `crates/biliup-cli/src/server/common/recovery_eligibility.rs` → `crates/biliup-cli/src/observe/audit.rs` → 事件 SQLite（`record_recovery_audit`、`replay_recovery_audits`、`operation_projected`）：业务审计事务先保存稳定 uid，提交后立即投影；importer 启动全量、周期最近窗口重放同一 uid，事件库唯一约束收敛重复，业务审计不依赖事件留存。
- `crates/biliup-cli/src/server/common/lifecycle_backfill.rs` → `crates/biliup-cli/src/server/common/upload_session.rs`（`session_completeness`）：回填的目标就是让历史会话的账本能被严格完整性闸门判为完整，冲突行则以未知状态持续阻塞投稿。
- `crates/biliup-cli/src/server/common/upload.rs` → `crates/biliup/src/uploader/line.rs`（`Probe::probe_excluding`、`Parcel::upload_with_observer`、`Parcel::recovery`）：恢复与自动模式把冷却线路排除在实际探测请求之外；上传返回的 `Video` 标题由上传文件名兜底，而喂进去的是响度标准化/时间戳修复的中间件，因此 `upload_single_file_with_repair` 必须用原始录像名覆盖分P标题。另外 preupload 一成功就把 UPOS 取回描述符经 `UploadActivity::UposRecovery` 送到 watchdog 循环落库——拿到它的 `upload_single_file` 手上没有 `missing_id`，而 watchdog 两样都有；落库列是 `upload_missing_segment.upos_recovery_json`，明文 + 写入时顺带清理超 TTL 的行（migration 24）。
- `crates/biliup-cli/src/server/common/upload.rs` → `crates/biliup-cli/src/server/common/upload_line_health.rs`（`baseline_mbps`、`record_success`）：watchdog 在进入传输阶段时读一次全库吞吐基线并缓存，`classify_transfer_rate` 用它判「在动但在爬」；上传成功回写的 EWMA 又是下一次的基线来源，两条慢判据共用同一个 `SLOW_RATIO`。判慢时再经 `active_cooldowns` + `machine_wide_slowness` 查别的线路是否也因慢被冷却，有则忍住不中止——换线无益时不作废已传字节。
- `crates/biliup/src/uploader/line.rs` → `crates/biliup/src/uploader/line/upos.rs`（`Upos::upload_stream`）：线路对象把实际分块传输委派给 upos 协议实现，观察者回调据此产生已确认字节进度。
- `crates/biliup-cli/src/server/common/upload.rs` → `crates/biliup-cli/src/server/common/audio_normalization.rs`、`crates/biliup-cli/src/server/common/timestamp_repair.rs`（`normalize_for_upload`、`normalize_timestamps`）：预处理顺序是先标准化、后时间戳检测，检测的对象是标准化产物而不是原片。标准化的测量遍已经完整 demux 过原片并顺带扫了时间戳，原片干净时 `upload_single_file_with_repair` 跳过对产物的整片扫描；诊断缺失或原片异常时照常走完整的检测/修复链路。判据抽在 `timestamp_repair_decision`：**归一化没产出就没有「源已验干净」这个结论可用**，`Original` 的每一个 reason 都必须扫（有回归测试锁住，别给它补乐观默认）。两个系统 runner 都从本次 `UploadIdentity` 构造显式 context，`processing.command_failed` 与完成事件按同一分段/attempt 关联。
- `crates/stream-gears/src/uploader.rs` → `crates/biliup-cli/src/server/common/upload.rs`（`upload_with_task`）：Python 传入 task 关联底层传输与后续投稿。
- `crates/biliup-cli/src/uploader.rs`、`crates/stream-gears/src/uploader.rs` → `crates/biliup-cli/src/observe/standalone.rs`（`UploadTask`）：早退记录投稿未开始的原因，远端请求返回后单独区分成功、失败与不确定结果。

## 跨领域继续阅读

- [媒体预处理](processing.md)
- [日志与观测](observability.md)
