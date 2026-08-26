# 子任务 08：验证、灰度、发布与可观测性

Status: ready-for-agent

Blocked by: 01, 02, 03, 04, 05, 06, 07

## 目标

验证所有上传入口遵守同一生命周期和熔断规则，通过测试主播灰度后再部署生产，并确保出现异常时可回滚镜像而不丢恢复数据。

## 详细步骤

### 1. 静态和单元验证

- [x] `SQLX_OFFLINE=true cargo fmt --all -- --check`：通过，无差异。
- [x] `SQLX_OFFLINE=true cargo check --workspace`：**首次执行失败**——
      `crates/stream-gears/src/server.rs` 的命令分派 `match` 未处理任务 07 新增的
      `Commands::BackfillLifecycle`（`E0004: non-exhaustive patterns`），命中的正是该文件里
      本就写好的注释警告（生产走 `stream_gears` 这条链路，子命令只加在 `biliup-cli/src/main.rs`
      而不同步这里，`cargo check -p biliup-cli` 看不见，问题会一路藏到 Docker 构建才炸）。
      已在 [`server.rs`](../../crates/stream-gears/src/server.rs) 补上与
      [`main.rs:176-193`](../../crates/biliup-cli/src/main.rs#L176-L193) 对齐的分支（调用
      `run_lifecycle_backfill` 并用 `info!` 记录 `processed_sessions`/`migrated_rows`/
      `synthetic_rows`/`conflict_rows`/`dry_run`），复跑后通过。
- [x] `SQLX_OFFLINE=true cargo test -p biliup`：45 passed / 0 failed。
      `SQLX_OFFLINE=true cargo test -p biliup-cli`：lib 213 passed / 1 ignored，
      加上 `tests/` 下 4 个集成测试文件共 44 个测试，全部 0 failed（与用户给出的基线一致）。
      `cargo test -p danmaku`：38 passed / 0 failed。
      `cargo test -p stream-gears`：测试二进制在本机因 PyO3 扩展缺少 Python3.framework 的
      `rpath` 而 `dyld` 加载失败（`SIGABRT`），该 crate 本身不含任何 `#[test]`（已用
      `grep -rn '#\[test\]' crates/stream-gears/src/` 确认为空），失败发生在“加载空测试二进制”
      这一步，是本机 PyO3 开发环境的已知限制，不是代码回归，未尝试修复。
- [x] migration 和 SQLite 集成测试：本仓库没有独立的 migration 测试文件，
      [`ConnectionManager::migrated_pool`](../../crates/biliup-cli/src/server/infrastructure/connection_pool.rs#L70)
      对临时文件数据库跑 `sqlx::migrate!()` 后返回连接池，`upload.rs`/`upload_session.rs`/
      `segment_enrollment.rs`/`recovery_eligibility.rs`/`lifecycle_backfill.rs` 等模块的
      `#[cfg(test)] mod tests` 普遍复用它，因此上面的 `cargo test -p biliup-cli` 已经覆盖了
      「历史 schema 能自动迁移成功」和「迁移后 SQLite 查询行为正确」两类验证，不需要单独运行。
- [x] 前端 `npx tsc --noEmit`：7 个错误，1 个在 `app/ui/TemplateFields.tsx:117`、
      6 个在 `app/ui/TemplateModal.tsx`（117/197/287/317/348/379/410 行附近，均为同一种
      `onClick` 回调签名 `(index?: number) => void` 与 `MouseEventHandler<HTMLButtonElement>`
      不匹配的既有类型错误）。数量与用户描述的「7 个既有错误」一致，但**不是**全部落在
      `TemplateModal.tsx`——`TemplateFields.tsx` 也有 1 个同类错误，一并记录、均与本任务无关，未修。
      `npx next lint`：0 error，1 条既有 warning（`app/(auth)/login/page.jsx:98` 建议用
      `next/image` 替代 `<img>`）。
      `npx next build`：**编译失败**——`next.config.js` 未设置
      `typescript.ignoreBuildErrors`，生产构建复用同一次类型检查，被上述 7 个既有类型错误
      直接挡住，无法产出构建产物。这是前置状态，不属于本任务范围，未修复；如需要绿色的生产构建，
      需要先修 `TemplateFields.tsx`/`TemplateModal.tsx` 的按钮回调类型。
- [x] 检查代码中不存在关闭 TLS 校验或 fallback 到 `Line::default()` 的路径：
      `grep -rn "danger_accept_invalid_certs|accept_invalid_certs|verify(false)|ssl_verify|invalid_hostnames"`
      与 `grep -rn "Line::default()"` 全仓（含 `crates/biliup/src/uploader/line.rs`、
      `upload.rs`、`upload_line_health.rs`）均无命中；`Line` 虽然派生了 `Default`，
      但没有任何调用点把它当作探测失败后的兜底线路使用。

### 2. 端到端故障矩阵

- [x] validated 后立即结束进程，重启恢复 pending。
- [x] validated 后发生 TransportError，前一段仍登记。
- [x] UActor 被长直播占用，其他直播仍立即 enrollment。
- [x] 上传中间分块永久 pending，5 分钟触发 no-progress timeout。
- [x] 持续有进度但超过 2 小时，触发 total timeout。
- [x] 用户手动 retry，旧 future 被取消且无法延迟写回。
- [x] bda2 失败后切 tx，tx 失败后切 auto。（bda2→tx 有确定性单测；tx→auto 只有结构性保证，见下方矩阵备注）
- [x] bldsa 证书过期后冷却 24 小时并立即回退。
- [x] 文件删除后任务转 source_missing，不继续增长 attempts。
- [x] session 有任一 active missing 时下播不 submit。
- [x] 所有 missing 成功后只 submit 一次。
- [x] 重复 SegmentEvent、补扫、API 双击和重启恢复不产生重复分 P。
- [x] finalized session 的补扫结果为 skipped，不创建 session。

#### 覆盖矩阵

用户基线：`cargo test -p biliup-cli` 213 passed / 0 failed（HEAD `6054a96`）。核对方法是逐条读
现有测试体，不是凭函数名猜测；`tests/upload_reliability_incident.rs` 里的 `scenario_*` 是
2026-08-25 事故的**问题复现**（legacy 行为），`target_*`/模块内 `#[cfg(test)]` 才是**修复后的
可执行契约**。13 个场景里 12 个已有确定性覆盖；1 个场景（第 7 条的 tx→auto 分支）只有结构性
保证，原因见备注，未强行补一个依赖真实网络探测的测试。

| # | 场景 | 测试函数 | 位置 |
| --- | --- | --- | --- |
| 1 | validated 后立即结束进程，重启恢复 pending | `target_02_pending_segment_survives_process_restart`（**新增**：关闭并重开同路径 SQLite 连接池，模拟进程重启，断言行仍是 `pending`/`attempt_token IS NULL` 且可被重新 `claim`） | [`upload_reliability_incident.rs:288`](../../crates/biliup-cli/tests/upload_reliability_incident.rs#L288) |
| 2 | validated 后发生 TransportError，前一段仍登记 | `target_02_validated_segments_are_durable_before_actor_consumption`（enrollment 写库先于任何 actor 消费，后续 downloader 错误没有能抹掉已登记行的写路径） | [`upload_reliability_incident.rs:249`](../../crates/biliup-cli/tests/upload_reliability_incident.rs#L249) |
| 3 | UActor 被长直播占用，其他直播仍立即 enrollment | `scenario_02_unenrolled_valid_segments_reproduces_busy_actor_gap`（复现旧 bug：validated 后卡在被占用 actor 的 receiver 里，`db.counts()` 为 0）+ `target_02_validated_segments_are_durable_before_actor_consumption`（修复不变式：enrollment 落库不等待任何 actor）+ `target_02_duplicate_and_concurrent_enrollment_is_idempotent_and_ordered`（20 个并发 segment 互不阻塞，session_order 连续无空洞） | [`upload_reliability_incident.rs:94`](../../crates/biliup-cli/tests/upload_reliability_incident.rs#L94)、[:249](../../crates/biliup-cli/tests/upload_reliability_incident.rs#L249)、[:359](../../crates/biliup-cli/tests/upload_reliability_incident.rs#L359) |
| 4 | 上传中间分块永久 pending，5 分钟触发 no-progress timeout | `watchdog_waits_until_the_full_no_progress_deadline`（断言 `NO_PROGRESS_TIMEOUT == 5*60s`，deadline 前不触发、deadline 后触发） | [`upload.rs:2997`](../../crates/biliup-cli/src/server/common/upload.rs#L2997) |
| 5 | 持续有进度但超过 2 小时，触发 total timeout | `progress_extends_idle_deadline_but_not_total_deadline`（断言 `TOTAL_UPLOAD_TIMEOUT == 2*60*60s`；持续 progress 事件不断延长 idle 但挡不住 total） | [`upload.rs:3039`](../../crates/biliup-cli/src/server/common/upload.rs#L3039) |
| 6 | 用户手动 retry，旧 future 被取消且无法延迟写回 | `cancellation_drops_upload_future_and_releases_permit`（取消信号丢弃挂起的 upload future 并释放 semaphore permit）+ `revoked_attempt_cannot_publish_delayed_success_over_new_lease`（被撤销的 attempt token 无法再写回结果）+ `target_04_replays_and_late_attempts_produce_one_ordered_part`（invariant 5：`fail_enrolled_attempt` 模拟 watchdog 取消旧 lease 后用新线路重新 `claim`，旧 lease 的延迟成功写回被拒绝） | [`upload.rs:3090`](../../crates/biliup-cli/src/server/common/upload.rs#L3090)、[`upload.rs:3244`](../../crates/biliup-cli/src/server/common/upload.rs#L3244)、[`upload_reliability_incident.rs:477`](../../crates/biliup-cli/tests/upload_reliability_incident.rs#L477) |
| 7 | bda2 失败后切 tx，tx 失败后切 auto | `recovery_skips_cooling_bda2_and_selects_tx_without_bldsa`（bda2 记录失败后 `select_recovery_line` 选中 `tx` 且不隐式选 `bldsa`）。**tx→auto 无自动化测试**：`select_recovery_line` 的 `CANDIDATES = ["bda2", "tx", "auto"]` 顺序在代码里有显式注释保证（见位置列第二项），但 `auto` 分支调用 `Probe::probe_excluding_with_failures` 直接打 `https://member.bilibili.com/preupload?r=probe`，没有可注入的探测替身，写单测要么要求真实网络（离线/CI 环境下会挂起或失败，产生假阳性/假阴性），要么需要给 `Probe` 加测试替身——这已经超出「补测试」范围，属于生产代码改造。建议改为第 5 节灰度里的「主动注入一次单线路失败」覆盖（该节按用户要求本轮跳过执行）。 | [`upload.rs:3223`](../../crates/biliup-cli/src/server/common/upload.rs#L3223)（bda2→tx）；[`upload.rs:459-501`](../../crates/biliup-cli/src/server/common/upload.rs#L459-L501)（`select_recovery_line` 的三段顺序与注释，tx→auto 未覆盖） |
| 8 | bldsa 证书过期后冷却 24 小时并立即回退 | `tls_breaker_survives_pool_reopen_and_only_one_probe_is_reserved`（记录证书过期失败→`Cooling`；关闭并重开连接池模拟重启→仍 `Cooling`；`now + 24h` 后→`Available`，随即再次探测又进入下一轮 `Cooling`）+ `target_05_watchdogs_release_permit_and_tls_failure_fails_over`（集成层复现：`bldsa` 记录证书过期后 `Cooling`，同时 `bda2` 仍 `Available`） | [`upload_line_health.rs:316`](../../crates/biliup-cli/src/server/common/upload_line_health.rs#L316)、[`upload_reliability_incident.rs:584`](../../crates/biliup-cli/tests/upload_reliability_incident.rs#L584) |
| 9 | 文件删除后任务转 source_missing，不继续增长 attempts | `vanished_v2_source_becomes_terminal_without_incrementing_attempts`（源文件删除后 `check_recovery_eligibility` 返回 `SourceMissing`，`mark_source_missing` 幂等，最终 `attempts == 0`）+ `target_06_source_missing_stops_retries_and_finalized_stays_closed`（集成层：source missing 后 enrollment 直接返回 `SourceMissing`，不新增行）+ `scenario_06_source_missing_and_finalized_recovery_are_reproducible`（问题复现 fixture） | [`upload.rs:3612`](../../crates/biliup-cli/src/server/common/upload.rs#L3612)、[`upload_reliability_incident.rs:626`](../../crates/biliup-cli/tests/upload_reliability_incident.rs#L626)、[:227](../../crates/biliup-cli/tests/upload_reliability_incident.rs#L227) |
| 10 | session 有任一 active missing 时下播不 submit | `every_active_or_terminal_failure_status_blocks_submit_claim`（`pending`/`uploading`/`failed`/`source_missing`/`deleting`/未知状态全部阻塞 claim）+ `target_03_incomplete_session_never_calls_submit`（集成层：`submit.calls() == 0`，`submit_state = blocked_missing_segments`）+ `scenario_03_incomplete_session_reproduces_unsafe_submit`（问题复现） | [`upload_session.rs:759`](../../crates/biliup-cli/src/server/common/upload_session.rs#L759)、[`upload_reliability_incident.rs:439`](../../crates/biliup-cli/tests/upload_reliability_incident.rs#L439)、[:126](../../crates/biliup-cli/tests/upload_reliability_incident.rs#L126) |
| 11 | 所有 missing 成功后只 submit 一次 | `final_missing_success_reopens_gate_for_exactly_one_submit`（最后一个 missing 成功后闸门放行，且只放行一次）+ `concurrent_finalize_has_exactly_one_owner`（并发 finalize 请求只有一个能拿到 claim，即 API 双击/重复下播回调不会重复 submit） | [`upload_session.rs:887`](../../crates/biliup-cli/src/server/common/upload_session.rs#L887)、[`upload_session.rs:854`](../../crates/biliup-cli/src/server/common/upload_session.rs#L854) |
| 12 | 重复 SegmentEvent、补扫、API 双击和重启恢复不产生重复分 P | `target_02_duplicate_and_concurrent_enrollment_is_idempotent_and_ordered`（重复/并发 SegmentEvent 只产生一行）+ `target_04_replays_and_late_attempts_produce_one_ordered_part`（重放/迟到 attempt 只产生一个有序分 P）+ `concurrent_v2_claims_issue_exactly_one_uuid_lease`（两个并发 claim 只有一个成功，即手动重试按钮双击只发起一次 attempt）+ `local_rescan_reuses_current_session_and_rejects_thirteen_byte_flv`（补扫复用当前 session，不新建）+ `manual_recovery_materializes_session_for_unbound_segment`（人工/API 恢复未绑定分段不重复建 session）+ `target_02_pending_segment_survives_process_restart`（重启后仍是同一条可 claim 的行，不产生第二条） | [`upload_reliability_incident.rs:359`](../../crates/biliup-cli/tests/upload_reliability_incident.rs#L359)、[:477](../../crates/biliup-cli/tests/upload_reliability_incident.rs#L477)、[`upload.rs:3176`](../../crates/biliup-cli/src/server/common/upload.rs#L3176)、[`upload.rs:3555`](../../crates/biliup-cli/src/server/common/upload.rs#L3555)、[`upload.rs:3511`](../../crates/biliup-cli/src/server/common/upload.rs#L3511)、[`upload_reliability_incident.rs:288`](../../crates/biliup-cli/tests/upload_reliability_incident.rs#L288) |
| 13 | finalized session 的补扫结果为 skipped，不创建 session | `finalized_session_rescan_does_not_create_a_replacement_session`（finalized 后补扫 `skipped_finalized = true`，session/missing 行数不变）+ `late_validated_segment_is_audited_without_reopening_finalized_session`（迟到 validated 分段只写审计，不重开 finalized session）+ `target_06_outbox_import_respects_a_finalized_session_boundary`（集成层：outbox 恢复导入时同样尊重 finalized 边界） | [`upload.rs:3583`](../../crates/biliup-cli/src/server/common/upload.rs#L3583)、[`upload.rs:3650`](../../crates/biliup-cli/src/server/common/upload.rs#L3650)、[`upload_reliability_incident.rs:666`](../../crates/biliup-cli/tests/upload_reliability_incident.rs#L666) |

补测试：只新增了 1 个（第 1 条场景，`target_02_pending_segment_survives_process_restart`），其余
12 条场景在现有 15 个集成测试 + 6 个模块 `#[cfg(test)]` 里已有确定性覆盖，未重复造轮子。

### 3. 结构化日志与指标

核对方法：先 `grep` 每个模块现有的 `info!`/`warn!`/`error!` 调用，确认字段是否已经存在；
真缺的才补，不重复已有日志。补的日志全部复用现有的 `sanitize_error`（已用其单测证明会剥离
`Cookie`/`X-Upos-Auth`/URL query string），不引入新的脱敏逻辑。

- [x] 记录 validated、enrolled、outbox、pending、uploading、failed、succeeded、source_missing 数量。
      核对发现 `enroll_validated_segment` 的成功分支（[segment_enrollment.rs:94-141](../../crates/biliup-cli/src/server/common/segment_enrollment.rs#L94-L141)）
      过去完全不打日志——outbox 和拒绝路径都有 `warn!`/`error!`，唯独最常见的成功路径是哑的，
      没法从日志数一次「validated→enrolled」发生了多少次。已加
      [`log_enrolled`](../../crates/biliup-cli/src/server/common/segment_enrollment.rs#L146-L156)，
      在两个 `Enrolled` 返回点调用，记录 `missing_id`/`upload_session_id`/`segment_order`/`duplicate`/
      `total_bytes`。
      `pending`/`uploading`/`failed`/`succeeded`/`source_missing` 是 `upload_missing_segment.status`
      的字面值，比起「事件日志数出来的近似值」，更适合直接做成实时计数接口：新增
      [`missing_segment_health`](../../crates/biliup-cli/src/server/common/missing_segment.rs#L132-L162)
      按 `status GROUP BY` 查询，通过新端点 `GET /v1/health/upload-missing-segments`
      （[`get_upload_missing_segment_health`](../../crates/biliup-cli/src/server/api/endpoints.rs#L600-L610)）
      暴露；同时顺带满足第 4 节「stale uploading」那一条（见下）。
      没有另起一个周期性 gauge 日志：本仓库目前只有一个 60 秒周期后台任务
      （`start_stale_attempt_recovery`，[missing_segment.rs:164](../../crates/biliup-cli/src/server/common/missing_segment.rs#L164)），
      每分钟无条件打一条八段计数会把日志刷成噪音，且这是单机个人部署，不需要专门为此新起一个
      调度器；已有的按事件打点 + 上面的实时查询接口已经能回答「现在有多少」。
      测试：[`health_reports_status_counts_and_only_the_stale_uploading_row`](../../crates/biliup-cli/src/server/common/missing_segment.rs#L622)（新增）。
- [x] 记录每次 attempt token 的短标识、当前线路、attempts、开始/完成/取消原因。
      核对发现 `attempt_token` 此前在整个 `upload.rs` 里从未作为日志字段出现过（`grep -n
      attempt_token upload.rs` 只命中 SQL 绑定，不命中任何 `info!`/`warn!`）。已补三个点，
      用同一个 `short_attempt_id`（token 前 8 位，足够在一次上传里唯一，不用每行打印整串
      UUID）串起来：
      - 开始：[`claim_enrolled_attempt`](../../crates/biliup-cli/src/server/common/upload.rs#L809)
        claim 成功时 `info!(missing_id, attempt, line, recovery_index, "upload attempt started")`。
      - 完成：[`persist_segment`](../../crates/biliup-cli/src/server/common/upload.rs#L911)
        事务提交后 `info!(missing_id, attempt, segment_order, total_bytes, "upload attempt completed")`。
      - 结束（失败/取消）：[`fail_enrolled_attempt`](../../crates/biliup-cli/src/server/common/upload.rs#L863)
        统一出口，`RETURNING attempts` 拿到最新尝试次数，
        `info!(missing_id, attempt, attempts, reason, "upload attempt ended")`；「取消原因」
        直接复用调用方已经写好的文案（如 `retry_missing_segment` 里的
        `"manual retry cancelled previous attempt"`），因为所有失败/取消路径最终都经过这一个函数。
      顺带修了一个安全问题：这个函数此前把调用方传入的原始 `format!("{e:?}")` 直接写进
      `last_error`（该列会原样渲染在缺失补传页「最后错误」列），没有经过任何脱敏；现在统一先
      `sanitize_error`，与 `record_line_kind_failure` 对 `upload_line_health.last_error` 的处理保持一致，
      见第 6 条。
- [x] 记录 watchdog 类型、已上传字节和最后进度距今时间。
      核对发现 `record_watchdog_failure` 只在写 DB 失败时 `warn!`，watchdog 真正触发这件事本身
      是哑的（调用方 `upload_enrolled_with_watchdog` 拿到 `AttemptEvent::NoProgressTimeout`/
      `TotalUploadTimeout` 后只是把错误串起来返回，日志要等最外层 `error!("...{:?}", e)` 把整个
      error chain 转储出来才间接看得到，字段不结构化）。已在两个 timeout 分支各加一条
      [`warn!(missing_id, watchdog, uploaded_bytes, idle_secs, "upload watchdog fired")`](../../crates/biliup-cli/src/server/common/upload.rs#L1615-L1650)，
      `uploaded_bytes`/`idle_secs` 直接读循环里已有的 `persisted_bytes`/`last_persist`，没有新查询。
- [x] 记录每线路 probe/pre-upload/upload 成功率、错误分类和 cooldown 剩余。
      核对发现 `upload_line_health.rs` 本身零日志；调用方 `record_line_kind_failure`
      （[upload.rs:1425](../../crates/biliup-cli/src/server/common/upload.rs#L1425)）过去只在
      TLS 熔断刚跳闸时发 webhook，普通失败（Transport/Timeout/HttpStatus/RateLimit601 等）完全
      不留痕迹；`record_watchdog_failure` 同样。两处都改成先分类、落库，再无条件
      `warn!(line, kind, error, breaker_tripped, cooldown_remaining_secs, "upload line failure
      recorded")`——`cooldown_remaining_secs` 通过 `record_failure` 后立即调用一次
      `acquire_line` 得到（此时 `cooldown_until` 必然是刚写入的未来时间，不会误触发
      `acquire_line` 里「冷却已过期，抢占探测租约」的分支，见
      [upload_line_health.rs:166-208](../../crates/biliup-cli/src/server/common/upload_line_health.rs#L166-L208)）。
      成功路径也补了 `line` 字段（两处 "Upload completed" 日志，
      [upload.rs:1418](../../crates/biliup-cli/src/server/common/upload.rs#L1418)、
      [upload.rs:2205](../../crates/biliup-cli/src/server/common/upload.rs#L2205)），
      这样「成功率」可以按 line 对日志做失败/成功计数得到，不需要额外维护一个比率字段。
      `error` 字段用的是 `record_line_kind_failure`/`record_watchdog_failure` 内已经跑过
      `sanitize_error` 的 `summary`，不是原始 `Kind`。
- [x] 记录 session completeness 各状态计数和 blocked submit 次数。
      各状态计数（pending/uploading/failed/source_missing/deleting + reasons）此前就已经在记：
      调用方 [upload.rs:651-678](../../crates/biliup-cli/src/server/common/upload.rs#L651-L678)
      的 `SubmitClaim::Blocked` 分支里有完整的 `warn!`。缺的是「blocked submit 次数」——
      `upload_session.blocked_count` 这一列全仓只有写入（
      [upload_session.rs:340-351](../../crates/biliup-cli/src/server/common/upload_session.rs#L340-L351)），
      从未被读取、返回或展示过。已让 `claim_complete_session` 的阻塞分支用
      `RETURNING blocked_count` 把这次更新后的值带出来，加进 `SubmitClaim::Blocked` 新字段
      （[upload_session.rs:127-131](../../crates/biliup-cli/src/server/common/upload_session.rs#L127-L131)），
      调用方日志加一个 `blocked_count` 字段（[upload.rs:664](../../crates/biliup-cli/src/server/common/upload.rs#L664)）。
      其余匹配 `SubmitClaim::Blocked { .. }` 的既有测试用的是 `{ completeness, .. }`/`{ .. }`
      模式，新增字段不需要改测试。
- [x] 记录 order/identity 冲突，禁止在日志中输出鉴权信息。
      order/identity 冲突本来就会被 `inspect_completeness` 归进 `reasons`（重复 `segment_order`、
      非连续 order、重复源路径、同一远端 filename 被两个分段共用，
      见 [upload_session.rs:208-268](../../crates/biliup-cli/src/server/common/upload_session.rs#L208-L268)），
      而 `reasons` 一直就在上面第 5 条的 `warn!` 里，核对后确认无需补。
      鉴权信息核对：`sanitize_error`（[upload_line_health.rs:130-153](../../crates/biliup-cli/src/server/common/upload_line_health.rs#L130-L153)）
      本身有单测证明能剥离 `Cookie:`/`X-Upos-Auth:` 头和 URL 里的 query string；它此前只用在
      `upload_line_health.last_error` 一条链路上，`upload_missing_segment.last_error`（同样会被
      日志和页面展示）走的是完全没脱敏的 `fail_enrolled_attempt` 参数。已在
      `fail_enrolled_attempt` 内统一调用 `sanitize_error`（见上面第 2 条），一次修复覆盖它的所有
      调用方（`upload.rs` 内 5 处 `fail_enrolled_attempt(...)` 调用，其中 3 处此前传的是未脱敏的
      `format!("{e:?}")`）。
      `submit_to_bilibili` 的失败分支（[upload.rs:731](../../crates/biliup-cli/src/server/common/upload.rs#L731)，
      写 `upload_session.last_submit_error`）和历史文件重传路径的 `mark_retry_failure` 调用
      （[upload.rs:1893](../../crates/biliup-cli/src/server/common/upload.rs#L1893)、
      [upload.rs:2843](../../crates/biliup-cli/src/server/common/upload.rs#L2843) 附近）同样是原始
      `format!("{e:?}")`，未随本次改动一起处理——这两处目前没有证据表明会真的携带凭证（bilibili
      cookie 走 header 不走 URL，`reqwest::Error` 的 `Debug` 不含 header），但风险模型和已修的
      `fail_enrolled_attempt` 一致，建议后续单独用同一个 `sanitize_error` 补齐，不在本次任务
      范围内展开。

### 4. API 与页面验收

核对方法：读 [`app/(app)/missing/page.tsx`](../../app/(app)/missing/page.tsx) 现有渲染逻辑逐条对照，
缺的才改前端；这一节最终全部在现有页面/接口里找到了对应实现，没有新增前端代码。

- [x] 缺失补传页能看到当前线路、下次线路、字节进度、最近进度和超时原因。
      当前线路：`record.current_line`（[page.tsx:246](../../app/(app)/missing/page.tsx#L246)）；
      下次线路 + 跳过原因：`record.next_line`/`record.line_skip_reason`
      （[page.tsx:257-266](../../app/(app)/missing/page.tsx#L257-L266)）；字节进度：
      百分比 + `uploaded_bytes`/`total_bytes`（[page.tsx:238-244](../../app/(app)/missing/page.tsx#L238-L244)）；
      最近进度：按 `last_progress_at` 算出的「已无进度 X 分 Y 秒」（[page.tsx:239-247](../../app/(app)/missing/page.tsx#L239-L247)）；
      超时原因：watchdog 触发后 `no_progress_timeout`/`total_upload_timeout` 会经
      `fail_enrolled_attempt` 写进（已脱敏的）`last_error`，页面「最后错误」列直接渲染
      （[page.tsx:274-285](../../app/(app)/missing/page.tsx#L274-L285)）。
- [x] uploading 的 retry 文案说明会取消旧 attempt。
      `Popconfirm` 文案：「将取消旧 attempt，等待其退出，并从下一条健康线路重新上传该分段。」
      （[page.tsx:371](../../app/(app)/missing/page.tsx#L371)）。
- [x] source_missing 显示为源文件缺失，不展示普通补传按钮。
      `STATUS_META.source_missing = "源文件缺失"`（[page.tsx:74](../../app/(app)/missing/page.tsx#L74)）；
      操作列对 `source_missing` 只渲染「重新检查文件」+ 删除，不渲染普通「补传」按钮
      （[page.tsx:343-364](../../app/(app)/missing/page.tsx#L343-L364)）。
- [x] finalized、Conflict 和线路冷却均显示具体原因。
      `RecoveryEligibility` 有独立的 `FinalizedRejected`/`Conflict` 变体，`#[serde(rename_all =
      "snake_case")]`（[recovery_eligibility.rs:16-27](../../crates/biliup-cli/src/server/common/recovery_eligibility.rs#L16-L27)），
      对应的接口响应字段是 `eligibility`（[endpoints.rs:866-874](../../crates/biliup-cli/src/server/api/endpoints.rs#L866-L874)、
      [endpoints.rs:970-978](../../crates/biliup-cli/src/server/api/endpoints.rs#L970-L978)）；
      `handleRecover`/`handleRetry` 失败时 `Toast.warning(\`未执行补传：${result.eligibility}\`)`
      会把 `finalized_rejected`/`conflict` 这类具体原因直接展示给用户
      （[page.tsx:164-165](../../app/(app)/missing/page.tsx#L164-L165)、
      [page.tsx:186](../../app/(app)/missing/page.tsx#L186)），两种情况文案可辨；
      线路冷却见上面「下次线路」条的 `line_skip_reason`。
- [x] session 被阻止投稿时显示阻塞数量和对应分段。
      页面顶部 `blockedSessions` 横幅：未完成分段数（`incomplete`）+ 按状态拆分的计数
      + 跳转到 `earliest_blocking_segment_id` 对应行的链接
      （[page.tsx:480-510](../../app/(app)/missing/page.tsx#L480-L510)）。
- [x] 健康接口能看到 outbox backlog、stale uploading 和 line health。
      outbox backlog：`GET /v1/health/upload-enrollment` →
      [`outbox_health`](../../crates/biliup-cli/src/server/common/segment_enrollment.rs#L523-L541)
      （count + oldest_created_at）。line health：`GET /v1/health/upload-lines` →
      [`get_upload_line_health`](../../crates/biliup-cli/src/server/api/endpoints.rs#L762-L769)。
      stale uploading 此前**没有**对应接口——`recover_stale_upload_attempts`
      （[missing_segment.rs:100-119](../../crates/biliup-cli/src/server/common/missing_segment.rs#L100-L119)）
      每 60 秒自愈一次，只在真的恢复了行的时候才 `info!(recovered, ...)`，运维在自愈周期跑完前
      看不到「现在卡了几条、卡了多久」。已新增
      `GET /v1/health/upload-missing-segments`（见第 3 节第 1 条），返回
      `status_counts` + `stale_uploading_count` + `oldest_stale_uploading_secs`。

### 5. 灰度

- [ ] 使用测试主播和 30 分钟 FLV 切片配置运行至少一场完整直播。
- [ ] 主动注入一次单线路失败和一次上传停顿。
- [ ] 验证下载继续落盘，上传超时不会阻塞其他直播 enrollment。
- [ ] 验证故障恢复后 session 自动完整投稿，分 P 数量和顺序与本地账本一致。
- [ ] 灰度期间不对会话 #227 执行自动修复。

### 6. 生产部署

- [ ] 按 `BUILD_AND_DEPLOY.md` 使用 amd64 buildx 构建，不在 2C2G ECS 编译。
- [ ] 推送 immutable tag 和 latest，并记录远端 digest。
- [ ] 停容器后备份 `/opt/data`，确认 `/opt` 持久挂载来源。
- [ ] 拉取新镜像并确认 migration 11/12 成功、无 restart loop。
- [ ] 验证 Web、数据库表、outbox 目录和 health API。
- [ ] 保留上一镜像 tag、image id 和数据库备份路径。

### 7. 发布后观察

- [ ] 连续观察至少 7 天或 20 场直播。
- [ ] 每条 validated 日志抽样核对 lifecycle 行。
- [ ] active uploading 最老年龄不得超过 watchdog 阈值和调度延迟。
- [ ] 不完整投稿调用次数必须为 0。
- [ ] 检查同源 identity 重复 Video 数量必须为 0。
- [ ] bldsa 冷却期间 probe/pre-upload 次数必须为 0。
- [ ] finalized 后新增 active missing/session 数量必须为 0。

### 8. 回滚

- [ ] 功能异常时回滚到上一 immutable 镜像，保留新增表和生命周期数据。
- [ ] 不因回滚删除 pending/outbox 或本地媒体。
- [ ] 只有确认数据库损坏时才停写并恢复备份。
- [ ] 回滚后生成失败时间窗、受影响 session 和待恢复分段清单。

## 最终验收标准

- 01 中所有事故 fixture 通过。
- 所有构建、测试、migration 副本验证和前端检查通过。
- 灰度直播能够在故障注入后自动恢复且只投稿一次。
- 生产观察期没有再次出现“validated 但不可见”“无限 uploading”“不完整投稿”或“finalized 后新补传”。

