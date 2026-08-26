use crate::UploadLine;
use crate::server::common::audio_normalization::{
    AudioSampleStore, NormalizationOutcome, SystemAudioFfmpeg, TempArtifact,
    maybe_capture_reference_sample, normalize_for_upload,
};
use crate::server::common::cookie_health::notify_alert;
use crate::server::common::cover_generator::{
    Background, CoverOptions, render_to_tempfile, split_template_lines,
};
use crate::server::common::missing_segment::{
    due_missing_segments_for_session, enqueue_pending_segment, mark_retry_failure,
    mark_retry_success, next_missing_segment_order, patch_studio_videos,
};
use crate::server::common::path_safety::single_segment_name;
use crate::server::common::recovery_eligibility::{
    RecoveryEligibility, check_recovery_eligibility, finalized_session_for_streamer_info,
    mark_source_missing, record_recovery_audit,
};
use crate::server::common::segment_enrollment::{
    EnrollmentOutcome, EnrollmentRequest, EnrollmentStore, enroll_validated_segment,
    normalize_segment_path,
};
use crate::server::common::timestamp_repair::{RepairOutcome, SystemFfmpeg, normalize_timestamps};
use crate::server::common::upload_line_health::{
    self, LineAvailability, UploadFailureKind, classify_kind, sanitize_error,
};
use crate::server::common::upload_rate_gate::{self, UploadRateGateSettings};
use crate::server::common::upload_session::{
    LiveArchive, SubmitClaim, active_sessions_for_room, claim_complete_session, get_streamer_info,
    insert_session_video_at_order, insert_uploading_session, mark_submit_anomaly, mark_submitted,
    parse_videos, reattach_session, release_submit_claim, select_recovery_candidate,
    select_stale_session_indices, submit_claim_is_owned, submit_state_label,
};
use crate::server::common::util::{FileValidator, MediaValidation, Recorder};
use crate::server::config::Config;
use crate::server::core::downloader::{SegmentEnrollment, SegmentInfo};
use crate::server::errors::{AppError, AppResult};
use crate::server::infrastructure::connection_pool::ConnectionPool;
use crate::server::infrastructure::context::{Context, Stage, WorkerStatus};
use crate::server::infrastructure::models::hook_step::{
    HookStep, process_video, process_video_paths,
};
use crate::server::infrastructure::models::live_streamer::LiveStreamer;
use crate::server::infrastructure::models::upload_streamer::UploadStreamer;
use crate::server::infrastructure::models::{
    FileItem, StreamerInfo, UploadMissingSegment, UploadSession,
};
use async_channel::Receiver;
use biliup::bilibili::Vid;
use biliup::bilibili::{BiliBili, ResponseData, Studio, Video};
use biliup::client::StatelessClient;
use biliup::credential::login_by_cookies;
use biliup::error::Kind;
use biliup::uploader::line::UploadProgress;
use biliup::uploader::line::{Line, Probe};
use biliup::uploader::util::SubmitOption;
use biliup::uploader::{VideoFile, line};
use error_stack::ResultExt;
use futures::StreamExt;
use ormlite::Model;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use struct_patch::Patch;
use tokio::pin;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing::{error, info, warn};

// 辅助结构体
struct UploadContext {
    bilibili: BiliBili,
    line: Line,
    threads: usize,
    client: StatelessClient,
    rate_gate: UploadRateGateSettings,
    pool: ConnectionPool,
    line_key: String,
    health_webhook: Option<String>,
}

static GLOBAL_UPLOAD_SEMAPHORE: OnceLock<std::sync::Arc<Semaphore>> = OnceLock::new();

const NO_PROGRESS_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const TOTAL_UPLOAD_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);
const CANCEL_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const PROGRESS_PERSIST_INTERVAL: Duration = Duration::from_secs(5);
const PROGRESS_PERSIST_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone)]
struct AttemptRegistration {
    attempt_token: String,
    cancellation: CancellationToken,
    completion: Arc<AttemptCompletion>,
}

#[derive(Default)]
struct AttemptCompletion {
    done: AtomicBool,
    notify: Notify,
}

impl AttemptCompletion {
    async fn wait(&self) {
        if self.done.load(Ordering::Acquire) {
            return;
        }
        let notified = self.notify.notified();
        if self.done.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }
}

static ATTEMPT_REGISTRY: OnceLock<Mutex<HashMap<i64, AttemptRegistration>>> = OnceLock::new();

fn attempt_registry() -> &'static Mutex<HashMap<i64, AttemptRegistration>> {
    ATTEMPT_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

struct AttemptGuard {
    missing_id: i64,
    attempt_token: String,
    completion: Arc<AttemptCompletion>,
}

impl Drop for AttemptGuard {
    fn drop(&mut self) {
        let mut registry = attempt_registry()
            .lock()
            .expect("attempt registry poisoned");
        if registry
            .get(&self.missing_id)
            .is_some_and(|entry| entry.attempt_token == self.attempt_token)
        {
            registry.remove(&self.missing_id);
        }
        self.completion.done.store(true, Ordering::Release);
        self.completion.notify.notify_waiters();
    }
}

fn register_attempt(missing_id: i64, attempt_token: &str) -> (AttemptGuard, CancellationToken) {
    let cancellation = CancellationToken::new();
    let completion = Arc::new(AttemptCompletion::default());
    let registration = AttemptRegistration {
        attempt_token: attempt_token.to_string(),
        cancellation: cancellation.clone(),
        completion: completion.clone(),
    };
    attempt_registry()
        .lock()
        .expect("attempt registry poisoned")
        .insert(missing_id, registration);
    (
        AttemptGuard {
            missing_id,
            attempt_token: attempt_token.to_string(),
            completion,
        },
        cancellation,
    )
}

async fn acquire_global_upload_permit() -> OwnedSemaphorePermit {
    GLOBAL_UPLOAD_SEMAPHORE
        .get_or_init(|| std::sync::Arc::new(Semaphore::new(1)))
        .clone()
        .acquire_owned()
        .await
        .expect("global upload semaphore should not be closed")
}

/// 重启续接默认时间窗口（分钟）
const DEFAULT_RECOVERY_WINDOW_MINUTES: u64 = 30;

pub async fn process_with_upload(
    rx: Receiver<SegmentInfo>,
    ctx: &Context,
    upload_config: &UploadStreamer,
) -> AppResult<()> {
    info!(upload_config=?upload_config, "Starting process with upload");
    let segment_processors: Vec<HookStep> = ctx
        .live_streamer()
        .segment_processor
        .clone()
        .unwrap_or_default();
    // 先落本地会话，再做 cookie 登录/线路探测。这样网络初始化即使长时间重试，
    // 本场也已经有可绑定的 durable session；初始化最终失败时直接复用它登记 rx。
    let deferred_archive = prepare_deferred_archive(ctx).await;
    let upload_context = match initialize_upload_context(
        &ctx.config(),
        ctx.stateless_client(),
        upload_config,
        ctx.pool(),
    )
    .await
    {
        Ok(upload_context) => upload_context,
        Err(init_error) => {
            // Stop the producer from adding more events to this failed pipeline. Buffered events
            // remain receivable and are durably queued below; the next segment will observe the
            // closed sender and create a fresh pipeline, which retries uploader initialization.
            rx.close();
            let reason = format!("upload context initialization failed: {init_error:?}");
            let (session_row_id, first_order) = match deferred_archive {
                Ok(archive) => {
                    let row_id = archive.session_row_id;
                    let first_order = if let Some(row_id) = row_id {
                        match next_missing_segment_order(ctx.pool(), row_id, archive.videos.len())
                            .await
                        {
                            Ok(order) => order,
                            Err(error) => {
                                error!(?error, row_id, "读取缺失分段顺序失败，从已上传段数继续");
                                i64::try_from(archive.videos.len()).unwrap_or(i64::MAX)
                            }
                        }
                    } else {
                        0
                    };
                    (row_id, first_order)
                }
                Err(error) => {
                    error!(
                        ?error,
                        "上传初始化失败后无法创建本地投稿会话，将以未绑定状态登记分段"
                    );
                    (None, 0)
                }
            };
            let summary = defer_segments_after_upload_init_failure(
                rx,
                ctx.pool(),
                ctx.worker_id(),
                ctx.id(),
                session_row_id,
                first_order,
                &reason,
                &segment_processors,
                &AudioSampleStore::for_working_directory(
                    std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                ),
            )
            .await;
            error!(
                url = ctx.live_streamer().url,
                session_row_id,
                received = summary.received,
                queued = summary.queued,
                queue_failures = summary.queue_failures,
                "上传初始化失败，已消费剩余分段并登记待补传"
            );
            return Err(init_error);
        }
    };

    // 方案B：录制期间只上传分段并把 Video 引用累积落库（uploading 态），不投稿；
    // 下播后一次性提交（整场只审一次，避免「过审后追加→重新审核」）。
    let span = tracing::info_span!("session", session = tracing::field::Empty);
    async {
        let archive =
            pipeline_upload_videos(rx, &upload_context, upload_config, &segment_processors, ctx)
                .await?;

        if let Some(mut archive) = archive
            && let Some(row_id) = archive.session_row_id
        {
            tracing::Span::current().record("session", row_id);
            if let Err(e) =
                recover_due_missing_segments(&upload_context, ctx, row_id, &mut archive).await
            {
                error!(?e, row_id, "静默补传缺失分段失败，继续提交已成功分段");
            }
            let config = ctx.config();
            if let Err(e) = submit_session(
                &upload_context,
                ctx.pool(),
                upload_config,
                ctx.live_streamer().cover_background.as_deref(),
                config.season_section_id,
                config.submit_api.as_deref(),
                ctx.streamer_info(),
                row_id,
            )
            .await
            {
                error!(?e, "下播一次性提交失败，保持 uploading 待下次补提交");
            }
        }
        Ok::<(), error_stack::Report<AppError>>(())
    }
    .instrument(span)
    .await
}

/// 把稿件加入合集，带重试。新稿件审核中，view 接口取 cid 可能有几十秒延迟，故重试。
async fn add_archive_to_season_with_retry(bilibili: &BiliBili, section_id: i64, aid: u64) {
    for attempt in 1..=5u32 {
        match bilibili.add_archive_to_season(section_id, aid).await {
            Ok(()) => {
                info!(aid, section_id, "稿件已加入合集");
                return;
            }
            Err(e) => {
                warn!(aid, section_id, attempt, "加入合集失败，将重试: {:?}", e);
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
        }
    }
    error!(
        aid,
        section_id, "加入合集多次失败，已放弃（可稍后在创作中心手动添加）"
    );
}

/// 带退避重试的 B站 cookie 登录。
///
/// 失败时把**完整错误链**（`{:?}`）打进日志——此前 `change_context(AppError::Unknown)` + 用
/// `{}`(Display) 打印会把底层原因（是超时/连接重置还是 cookie 真失效）全部吞掉，只剩 "Unknown Error"。
async fn login_with_retry(cookie_file: &str) -> AppResult<BiliBili> {
    const MAX_ATTEMPTS: u32 = 4;
    let mut backoff = std::time::Duration::from_secs(3);
    for attempt in 1..=MAX_ATTEMPTS {
        match login_by_cookies(cookie_file, None).await {
            Ok(bilibili) => return Ok(bilibili),
            Err(e) => {
                if attempt == MAX_ATTEMPTS {
                    error!(cookie_file, attempt, "B站 cookie 登录最终失败: {e:?}");
                    return Err(e).change_context(AppError::Unknown);
                }
                warn!(
                    cookie_file,
                    attempt, "B站 cookie 登录失败，{backoff:?} 后重试: {e:?}"
                );
                tokio::time::sleep(backoff).await;
                backoff *= 2;
            }
        }
    }
    unreachable!("login_with_retry 循环必然在最后一次尝试 return")
}

async fn initialize_upload_context(
    config: &Config,
    client: &StatelessClient,
    upload_config: &UploadStreamer,
    pool: &ConnectionPool,
) -> AppResult<UploadContext> {
    // 登录处理
    let cookie_file = upload_config
        .user_cookie
        .clone()
        .unwrap_or("cookies.json".to_string());
    // login_by_cookies 内部会向 B站 发请求校验 token，瞬时网络抖动（超时/连接重置）
    // 会直接返回 Err。此处带退避重试，避免一次网络抽风就把整场上传  rx 提前 drop、报销整场录像。
    let bilibili = login_with_retry(&cookie_file).await?;

    // 获取上传线路。显式配置也必须服从持久熔断；冷却时按恢复序列回退。
    let selected = select_configured_line(
        pool,
        &client.client,
        &config.lines,
        config.cookie_health_webhook.as_deref(),
    )
    .await?;

    Ok(UploadContext {
        bilibili,
        line: selected.line,
        threads: config.threads as usize,
        client: client.clone(),
        rate_gate: UploadRateGateSettings::from(config),
        pool: pool.clone(),
        line_key: selected.key,
        health_webhook: config.cookie_health_webhook.clone(),
    })
}

fn explicit_upload_line(line: &str) -> Option<Line> {
    match line {
        "bda2" => Some(line::bda2()),
        "bda" => Some(line::bda()),
        "tx" => Some(line::tx()),
        "txa" => Some(line::txa()),
        "bldsa" => Some(line::bldsa()),
        "alia" => Some(line::alia()),
        _ => None,
    }
}

#[derive(Clone)]
struct SelectedLine {
    line: Line,
    key: String,
    recovery_index: Option<i64>,
}

async fn probe_healthy_line(
    pool: &ConnectionPool,
    client: &reqwest::Client,
    webhook: Option<&str>,
) -> AppResult<SelectedLine> {
    let excluded = upload_line_health::active_cooldowns(pool, chrono::Utc::now())
        .await?
        .into_iter()
        .map(|row| row.line_key)
        .collect::<Vec<_>>();
    let (line, failures) = Probe::probe_excluding_with_failures(client, &excluded)
        .await
        .change_context(AppError::Unknown)?;
    for failure in failures {
        record_line_kind_failure(pool, &failure.line_key, webhook, &failure.error).await;
    }
    let key = line.key().to_string();
    Ok(SelectedLine {
        line,
        key,
        recovery_index: None,
    })
}

async fn select_configured_line(
    pool: &ConnectionPool,
    client: &reqwest::Client,
    configured: &str,
    webhook: Option<&str>,
) -> AppResult<SelectedLine> {
    if let Some(line) = explicit_upload_line(configured) {
        match upload_line_health::acquire_line(pool, configured, chrono::Utc::now()).await? {
            LineAvailability::Available => {
                return Ok(SelectedLine {
                    line,
                    key: configured.to_string(),
                    recovery_index: None,
                });
            }
            LineAvailability::Cooling { until, reason } => warn!(
                line = configured,
                %until,
                ?reason,
                "configured upload line is cooling; overriding to the next healthy line"
            ),
        }
    } else if configured.eq_ignore_ascii_case("auto") || configured.is_empty() {
        return probe_healthy_line(pool, client, webhook).await;
    }
    select_recovery_line(pool, client, 0, webhook).await
}

/// Recovery order is deliberately bda2 -> tx -> auto. bldsa is never an implicit candidate.
async fn select_recovery_line(
    pool: &ConnectionPool,
    client: &reqwest::Client,
    start_index: i64,
    webhook: Option<&str>,
) -> AppResult<SelectedLine> {
    const CANDIDATES: [&str; 3] = ["bda2", "tx", "auto"];
    let start = start_index.rem_euclid(CANDIDATES.len() as i64) as usize;
    for offset in 0..CANDIDATES.len() {
        let index = (start + offset) % CANDIDATES.len();
        let key = CANDIDATES[index];
        if key == "auto" {
            match probe_healthy_line(pool, client, webhook).await {
                Ok(mut selected) => {
                    selected.recovery_index = Some(index as i64);
                    return Ok(selected);
                }
                Err(error) => {
                    warn!(?error, "healthy upload line auto probe failed");
                    continue;
                }
            }
        }
        match upload_line_health::acquire_line(pool, key, chrono::Utc::now()).await? {
            LineAvailability::Available => {
                return Ok(SelectedLine {
                    line: explicit_upload_line(key).expect("fixed recovery line"),
                    key: key.to_string(),
                    recovery_index: Some(index as i64),
                });
            }
            LineAvailability::Cooling { until, reason } => info!(
                line = key,
                %until,
                ?reason,
                "skipping cooling upload line"
            ),
        }
    }
    Err(error_stack::Report::new(AppError::Custom(
        "no healthy upload line is currently available; attempt remains due".to_string(),
    )))
}

pub(crate) fn segment_paths(event: &SegmentInfo) -> Vec<PathBuf> {
    let mut paths = vec![event.prev_file_path.clone()];
    if let Some(danmaku_file_path) = &event.danmaku_file_path {
        paths.push(danmaku_file_path.clone());
    }
    paths
}

async fn index_recorded_segment(
    pool: &ConnectionPool,
    streamer_info_id: i64,
    event: &SegmentInfo,
) -> AppResult<()> {
    let file = event.prev_file_path.display().to_string();
    sqlx::query(
        "INSERT INTO filelist (file, streamer_info_id) \
         SELECT ?1, ?2 WHERE NOT EXISTS \
         (SELECT 1 FROM filelist WHERE file = ?1 AND streamer_info_id = ?2)",
    )
    .bind(file)
    .bind(streamer_info_id)
    .execute(pool)
    .await
    .change_context(AppError::Unknown)?;
    Ok(())
}

#[derive(Debug, Default, PartialEq, Eq)]
struct DeferredSegmentSummary {
    received: usize,
    queued: usize,
    queue_failures: usize,
}

#[allow(clippy::too_many_arguments)]
async fn defer_segments_after_upload_init_failure(
    rx: Receiver<SegmentInfo>,
    pool: &ConnectionPool,
    live_streamer_id: i64,
    streamer_info_id: i64,
    upload_session_id: Option<i64>,
    first_order: i64,
    reason: &str,
    segment_processors: &[HookStep],
    sample_store: &AudioSampleStore,
) -> DeferredSegmentSummary {
    // `process_with_upload` closes it before local session preparation. Close again so this helper
    // is safe in isolation and drains a finite snapshot even while producers still own senders.
    rx.close();
    let mut summary = DeferredSegmentSummary::default();
    pin!(rx);
    while let Some(event) = rx.next().await {
        let offset = i64::try_from(summary.received).unwrap_or(i64::MAX);
        let segment_order = first_order.saturating_add(offset);
        summary.received += 1;

        let mut paths = segment_paths(&event);
        let queue_reason = if !segment_processors.is_empty()
            && let Err(error) = process_video_paths(&mut paths, segment_processors).await
        {
            error!(
                ?error,
                file = %event.prev_file_path.display(),
                "上传初始化失败后的 segment_processor 执行失败；登记原始文件待人工处理"
            );
            paths = segment_paths(&event);
            format!("{reason}; segment_processor failed: {error:?}")
        } else {
            reason.to_string()
        };
        let queued_path = paths
            .first()
            .cloned()
            .unwrap_or_else(|| event.prev_file_path.clone());

        // 样片截取不依赖 B 站登录或上传线路。即使上传初始化失败，完整分段仍可满足
        // 用户已提交的“截取下一段”请求；失败开放，不影响待补传登记。
        maybe_capture_reference_sample(&queued_path, sample_store).await;

        let queued = if let Some(enrollment) = &event.enrollment {
            sqlx::query(
                "UPDATE upload_missing_segment SET last_error = ?1, updated_at = ?2 \
                 WHERE id = ?3 AND lifecycle_version = 2 AND status = 'pending'",
            )
            .bind(&queue_reason)
            .bind(chrono::Utc::now())
            .bind(enrollment.missing_id)
            .execute(pool)
            .await
            .map(|_| ())
            .change_context(AppError::Unknown)
        } else {
            enqueue_pending_segment(
                pool,
                live_streamer_id,
                streamer_info_id,
                upload_session_id,
                None,
                &queued_path,
                event.danmaku_file_path.as_deref(),
                segment_order,
                queue_reason,
                chrono::Utc::now(),
            )
            .await
        };
        match queued {
            Ok(()) => summary.queued += 1,
            Err(error) => {
                summary.queue_failures += 1;
                error!(
                    ?error,
                    file = %queued_path.display(),
                    segment_order,
                    "上传初始化失败后登记待补传失败；本地文件保持不动"
                );
            }
        }

        // The missing queue is the recovery source of truth. Index the recording only after the
        // queue write attempt so a crash between these two independent writes cannot leave a
        // filelist-only segment that still has no recovery action.
        if let Err(error) = index_recorded_segment(pool, streamer_info_id, &event).await {
            error!(
                ?error,
                file = %event.prev_file_path.display(),
                "上传初始化失败后写入 filelist 失败；待补传记录仍可独立恢复"
            );
        }
    }
    summary
}

/// 一次性提交一个会话累积的全部分段：构建 studio → submit_by_app → 写回 aid 并 finalize → 加合集。
/// 仅在成功拿到 aid 时才 finalize；失败保持 uploading，留待下次开播补提交。
/// `streamer_info` 决定标题/时间，补提交废弃会话时传该会话当时的 streamer_info。
async fn submit_session(
    upload_context: &UploadContext,
    pool: &ConnectionPool,
    upload_config: &UploadStreamer,
    streamer_background: Option<&str>,
    season_section_id: Option<i64>,
    submit_api: Option<&str>,
    streamer_info: &StreamerInfo,
    session_row_id: i64,
) -> AppResult<()> {
    let (claim_token, videos) = match claim_complete_session(pool, session_row_id).await? {
        SubmitClaim::Claimed { token, videos } => (token, videos),
        SubmitClaim::Blocked {
            completeness,
            changed,
        } => {
            warn!(
                session = session_row_id,
                incomplete = completeness.incomplete_count(),
                pending = completeness.pending,
                uploading = completeness.uploading,
                failed = completeness.failed,
                source_missing = completeness.source_missing,
                deleting = completeness.deleting,
                reasons = ?completeness.reasons,
                "session submit blocked by incomplete lifecycle ledger"
            );
            if changed {
                notify_alert(
                    upload_context.health_webhook.as_deref(),
                    "投稿已暂停：存在未完成分段",
                    &format!(
                        "会话 #{session_row_id} 因 {} 个未完成或异常分段暂停投稿；请在缺失补传页面处理。",
                        completeness.incomplete_count()
                    ),
                );
            }
            return Ok(());
        }
        SubmitClaim::AlreadyClaimed => {
            info!(
                session = session_row_id,
                "session submit already claimed; skipping duplicate finalize"
            );
            return Ok(());
        }
        SubmitClaim::Finalized => {
            info!(
                session = session_row_id,
                "session already finalized; skipping submit"
            );
            return Ok(());
        }
    };
    let bilibili = &upload_context.bilibili;
    let recorder = Recorder::new(upload_config.title.clone(), streamer_info.clone());
    let studio = match build_studio(
        upload_config,
        streamer_background,
        bilibili,
        videos.clone(),
        &recorder,
    )
    .await
    {
        Ok(studio) => studio,
        Err(error) => {
            let _ = release_submit_claim(
                pool,
                session_row_id,
                &claim_token,
                format!("build_studio failed: {error:?}"),
            )
            .await;
            return Err(error);
        }
    };
    if !submit_claim_is_owned(pool, session_row_id, &claim_token).await? {
        return Err(error_stack::Report::new(AppError::Custom(
            "submit claim was lost before remote request".to_string(),
        )));
    }
    info!(
        n_videos = videos.len(),
        title = %recorder.format_filename(),
        "submit_attempt：开始下播一次性投稿"
    );
    let resp = match submit_to_bilibili(bilibili, &studio, submit_api).await {
        Ok(resp) => resp,
        Err(e) => {
            let msg = format!("{e:?}");
            let state = submit_state_label(None, true); // "failed"
            error!(error = %msg, "submit_failed：投稿接口失败，保持 uploading 待补提交");
            if let Err(db) =
                mark_submit_anomaly(pool, session_row_id, &claim_token, state, msg, true).await
            {
                error!(?db, "写回 submit_state=failed 失败");
            }
            return Err(e);
        }
    };
    let aid = resp
        .data
        .as_ref()
        .and_then(|d| d.get("aid"))
        .and_then(|a| a.as_u64());
    let bvid = resp
        .data
        .as_ref()
        .and_then(|d| d.get("bvid"))
        .and_then(|b| b.as_str())
        .map(|s| s.to_string());

    match aid {
        Some(aid_val) => {
            // 提交成功即 finalize（mark_submitted 内部写 submit_state="ok_with_aid"）；
            // 写回失败仅告警（稿件已在 B 站，重复提交风险大于收益）。
            if let Err(e) =
                mark_submitted(pool, session_row_id, &claim_token, aid_val, bvid.clone()).await
            {
                error!(
                    ?e,
                    aid = aid_val,
                    "aid_writeback_fail：提交成功但写回 upload_session 失败"
                );
            } else {
                info!(aid = aid_val, bvid = ?bvid, "submit_ok_with_aid：投稿成功并已写回 aid");
            }
            if let Some(section_id) = season_section_id {
                add_archive_to_season_with_retry(bilibili, section_id, aid_val).await;
            }
        }
        None => {
            let state = submit_state_label(aid, false); // "ok_no_aid"
            let msg = format!("submit_ok_no_aid: {resp:?}");
            error!(resp = ?resp, "submit_ok_no_aid：投稿 code==0 但响应缺少 aid，未 finalize（待下次开播补提交）");
            // The remote accepted the request but returned no stable id. Preserve the claim so a
            // restart cannot blindly create a duplicate submission.
            if let Err(db) =
                mark_submit_anomaly(pool, session_row_id, &claim_token, state, msg, false).await
            {
                error!(?db, "写回 submit_state=ok_no_aid 失败");
            }
        }
    }
    Ok(())
}

/// The outcome of competing for the single valid attempt on a lifecycle row (invariant 5).
#[derive(Debug, PartialEq, Eq)]
pub enum AttemptClaim {
    Claimed(String),
    AlreadyRunning,
    AlreadyCompleted,
    NotDue,
    NotClaimable(String),
}

#[cfg(test)]
impl AttemptClaim {
    fn unwrap(self) -> String {
        match self {
            Self::Claimed(token) => token,
            other => panic!("expected claimed attempt, got {other:?}"),
        }
    }
}

pub async fn claim_enrolled_attempt(
    pool: &ConnectionPool,
    enrollment: &SegmentEnrollment,
    line: &str,
    recovery_index: Option<i64>,
) -> AppResult<AttemptClaim> {
    let token = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    let result = sqlx::query(
        "UPDATE upload_missing_segment \
         SET status = 'uploading', attempt_token = ?1, current_line = ?2, \
             line_index = COALESCE(?3, line_index), upload_started_at = ?4, \
             last_progress_at = ?4, uploaded_bytes = 0, updated_at = ?4 \
         WHERE id = ?5 AND lifecycle_version = 2 AND status IN ('pending', 'failed') \
           AND next_retry_at <= ?4 AND attempt_token IS NULL",
    )
    .bind(&token)
    .bind(line)
    .bind(recovery_index)
    .bind(now)
    .bind(enrollment.missing_id)
    .execute(pool)
    .await
    .change_context(AppError::Unknown)?;
    if result.rows_affected() == 1 {
        return Ok(AttemptClaim::Claimed(token));
    }
    let state = sqlx::query_as::<_, (String, Option<String>, chrono::DateTime<chrono::Utc>)>(
        "SELECT status, attempt_token, next_retry_at FROM upload_missing_segment WHERE id = ?",
    )
    .bind(enrollment.missing_id)
    .fetch_optional(pool)
    .await
    .change_context(AppError::Unknown)?;
    Ok(match state {
        Some((status, token, _)) if status == "uploading" || token.is_some() => {
            AttemptClaim::AlreadyRunning
        }
        Some((status, _, _)) if status == "succeeded" => AttemptClaim::AlreadyCompleted,
        Some((status, _, due)) if matches!(status.as_str(), "pending" | "failed") && due > now => {
            AttemptClaim::NotDue
        }
        Some((status, _, _)) => AttemptClaim::NotClaimable(status),
        None => AttemptClaim::NotClaimable("not_found".to_string()),
    })
}

pub async fn fail_enrolled_attempt(
    pool: &ConnectionPool,
    missing_id: i64,
    attempt_token: &str,
    error: String,
    now: chrono::DateTime<chrono::Utc>,
) -> AppResult<bool> {
    let updated = sqlx::query(
        "UPDATE upload_missing_segment \
         SET status = 'failed', attempts = attempts + 1, line_index = line_index + 1, \
             next_retry_at = ?1, last_error = ?2, attempt_token = NULL, current_line = NULL, \
             updated_at = ?3 \
         WHERE id = ?4 AND lifecycle_version = 2 AND status = 'uploading' \
           AND attempt_token = ?5",
    )
    .bind(now + chrono::Duration::minutes(10))
    .bind(error)
    .bind(now)
    .bind(missing_id)
    .bind(attempt_token)
    .execute(pool)
    .await
    .change_context(AppError::Unknown)?;
    Ok(updated.rows_affected() == 1)
}

/// Commit the remote Video, lifecycle success and session ordering atomically. The lifecycle row
/// is permanent: it is the idempotency identity for later event replays and rescans.
pub async fn persist_segment(
    pool: &ConnectionPool,
    archive: &mut LiveArchive,
    video: Video,
    enrollment: &SegmentEnrollment,
    attempt_token: &str,
) -> AppResult<()> {
    let video_json = serde_json::to_string(&video).change_context(AppError::Unknown)?;
    let now = chrono::Utc::now();
    let total_bytes = i64::try_from(enrollment.total_bytes).unwrap_or(i64::MAX);
    let mut tx = pool.begin().await.change_context(AppError::Unknown)?;
    let updated = sqlx::query(
        "UPDATE upload_missing_segment \
         SET video_json = ?1, status = 'succeeded', uploaded_bytes = ?2, \
             last_progress_at = ?3, last_error = NULL, attempt_token = NULL, updated_at = ?3 \
         WHERE id = ?4 AND upload_session_id = ?5 AND lifecycle_version = 2 \
           AND attempt_token = ?6 AND status = 'uploading'",
    )
    .bind(video_json)
    .bind(total_bytes)
    .bind(now)
    .bind(enrollment.missing_id)
    .bind(enrollment.upload_session_id)
    .bind(attempt_token)
    .execute(&mut *tx)
    .await
    .change_context(AppError::Unknown)?;
    if updated.rows_affected() != 1 {
        return Err(error_stack::Report::new(AppError::Custom(
            "upload attempt lease no longer owns lifecycle row".to_string(),
        )));
    }
    let session_json = sqlx::query_scalar::<_, String>(
        "SELECT videos_json FROM upload_session WHERE id = ? AND status != 'finalized'",
    )
    .bind(enrollment.upload_session_id)
    .fetch_one(&mut *tx)
    .await
    .change_context(AppError::Unknown)?;
    let mut videos = parse_videos(&session_json);
    let baseline_count = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT MIN(segment_order) FROM upload_missing_segment \
         WHERE upload_session_id = ? AND lifecycle_version = 2",
    )
    .bind(enrollment.upload_session_id)
    .fetch_one(&mut *tx)
    .await
    .change_context(AppError::Unknown)?
    .and_then(|value| usize::try_from(value).ok())
    .unwrap_or(videos.len())
    .min(videos.len());
    videos.truncate(baseline_count);
    let rows = sqlx::query_as::<_, (i64, String)>(
        "SELECT segment_order, video_json FROM upload_missing_segment \
         WHERE upload_session_id = ? AND lifecycle_version = 2 AND status = 'succeeded' \
           AND video_json IS NOT NULL ORDER BY segment_order ASC, id ASC",
    )
    .bind(enrollment.upload_session_id)
    .fetch_all(&mut *tx)
    .await
    .change_context(AppError::Unknown)?;
    for (_order, json) in rows {
        let video: Video = serde_json::from_str(&json).change_context(AppError::Unknown)?;
        videos.push(video);
    }
    let videos_json = serde_json::to_string(&videos).change_context(AppError::Unknown)?;
    sqlx::query("UPDATE upload_session SET videos_json = ?1, updated_at = ?2 WHERE id = ?3")
        .bind(videos_json)
        .bind(now)
        .bind(enrollment.upload_session_id)
        .execute(&mut *tx)
        .await
        .change_context(AppError::Unknown)?;
    tx.commit().await.change_context(AppError::Unknown)?;
    archive.session_row_id = Some(enrollment.upload_session_id);
    archive.videos = videos;
    Ok(())
}

/// 开播时准备本场会话状态：
/// 1) 先把「已废弃」会话（超窗口仍未提交）一次性补提交并 finalize，避免分段永远滞留未投稿；
/// 2) 再续接「窗口内」的会话（重启打断的同一场）继续累积，下播统一提交；
/// 3) 都没有则全新开始。
async fn prepare_archive(
    upload_context: &UploadContext,
    upload_config: &UploadStreamer,
    ctx: &Context,
) -> AppResult<LiveArchive> {
    let room_id = ctx.worker_id();
    let config = ctx.config();
    let window = config
        .recovery_window_minutes
        .unwrap_or(DEFAULT_RECOVERY_WINDOW_MINUTES) as i64;
    // read-then-reattach 非事务，但安全：本架构中每个 live_streamer_id（room）只有一个 worker。
    let sessions = active_sessions_for_room(ctx.pool(), room_id).await?;
    let now = chrono::Utc::now();

    // (1) 补提交废弃会话（用各自当时的 streamer_info 构建标题/时间）。
    for idx in select_stale_session_indices(&sessions, room_id, now, window) {
        let stale = &sessions[idx];
        let mut archive = LiveArchive {
            session_row_id: Some(stale.id),
            aid: stale.aid.map(|aid| aid as u64),
            bvid: stale.bvid.clone(),
            videos: parse_videos(&stale.videos_json),
        };
        // Initialization-failure sessions can be stale with an empty videos_json and all useful
        // data in the missing queue. Recover those rows before deciding whether there is anything
        // to submit, otherwise crossing the recovery window makes them permanently unreachable.
        if let Err(error) =
            recover_due_missing_segments(upload_context, ctx, stale.id, &mut archive).await
        {
            error!(
                ?error,
                row_id = stale.id,
                "补提交废弃会话：先行补传缺失分段失败，继续处理已成功分段"
            );
        }
        let streamer_info = match get_streamer_info(ctx.pool(), stale.streamer_info_id).await {
            Ok(si) => si,
            Err(e) => {
                warn!(
                    ?e,
                    row_id = stale.id,
                    "补提交废弃会话：取 StreamerInfo 失败，跳过"
                );
                continue;
            }
        };
        info!(
            row_id = stale.id,
            n = archive.videos.len(),
            "补提交上一场未提交的废弃会话"
        );
        if let Err(e) = submit_session(
            upload_context,
            ctx.pool(),
            upload_config,
            ctx.live_streamer().cover_background.as_deref(),
            config.season_section_id,
            config.submit_api.as_deref(),
            &streamer_info,
            stale.id,
        )
        .await
        {
            warn!(?e, row_id = stale.id, "补提交废弃会话失败，下次开播再试");
        }
    }

    // (2) 续接窗口内的会话。
    if let Some(idx) = select_recovery_candidate(&sessions, room_id, now, window) {
        let candidate = sessions[idx].clone();
        let videos = parse_videos(&candidate.videos_json);
        info!(
            row_id = candidate.id,
            room_id,
            n = videos.len(),
            "重启续接：继续累积本场分段，下播统一提交"
        );
        let row = reattach_session(ctx.pool(), candidate, ctx.id()).await?;
        Ok(LiveArchive {
            session_row_id: Some(row.id),
            aid: None,
            bvid: None,
            videos,
        })
    } else {
        Err(error_stack::Report::new(AppError::Custom(
            "upload pipeline started without an enrollment-created session".to_string(),
        )))
    }
}

/// Upload initialization can fail before `prepare_archive` gets a chance to consume the receiver.
/// Create or reattach a local session without doing any network I/O so deferred segments remain
/// recoverable instead of becoming untracked files.
async fn prepare_deferred_archive(ctx: &Context) -> AppResult<LiveArchive> {
    let room_id = ctx.worker_id();
    let window = ctx
        .config()
        .recovery_window_minutes
        .unwrap_or(DEFAULT_RECOVERY_WINDOW_MINUTES) as i64;
    let sessions = active_sessions_for_room(ctx.pool(), room_id).await?;
    let now = chrono::Utc::now();

    if let Some(idx) = select_recovery_candidate(&sessions, room_id, now, window) {
        let candidate = sessions[idx].clone();
        let videos = parse_videos(&candidate.videos_json);
        let row = reattach_session(ctx.pool(), candidate, ctx.id()).await?;
        info!(
            row_id = row.id,
            room_id,
            n = videos.len(),
            "上传初始化失败：续接本地投稿会话用于登记待补传"
        );
        Ok(LiveArchive {
            session_row_id: Some(row.id),
            aid: row.aid.map(|aid| aid as u64),
            bvid: row.bvid,
            videos,
        })
    } else {
        Err(error_stack::Report::new(AppError::Custom(
            "upload initialization failed without an enrollment-created session".to_string(),
        )))
    }
}

async fn pipeline_upload_videos(
    rx: Receiver<SegmentInfo>,
    upload_context: &UploadContext,
    upload_config: &UploadStreamer,
    segment_processors: &[HookStep],
    ctx: &Context,
) -> AppResult<Option<LiveArchive>> {
    let mut archive = prepare_archive(upload_context, upload_config, ctx).await?;
    pin!(rx);
    while let Some(event) = rx.next().await {
        let Some(enrollment) = event.enrollment.clone() else {
            error!(
                file = %event.prev_file_path.display(),
                "upload pipeline rejected segment without durable enrollment"
            );
            continue;
        };
        if archive.session_row_id != Some(enrollment.upload_session_id) {
            let session = UploadSession::select()
                .where_("id = ?")
                .bind(enrollment.upload_session_id)
                .fetch_one(ctx.pool())
                .await
                .change_context(AppError::Unknown)?;
            archive = LiveArchive {
                session_row_id: Some(session.id),
                aid: session.aid.map(|aid| aid as u64),
                bvid: session.bvid,
                videos: parse_videos(&session.videos_json),
            };
        }
        let recovery_source_paths = event.recovery_source_paths.clone();
        let mut paths = segment_paths(&event);
        if !segment_processors.is_empty()
            && let Err(e) = process_video_paths(&mut paths, segment_processors).await
        {
            let reason = format!("segment_processor failed before upload: {e:?}");
            sqlx::query(
                "UPDATE upload_missing_segment SET last_error = ?1, updated_at = ?2 \
                 WHERE id = ?3 AND lifecycle_version = 2 AND status = 'pending'",
            )
            .bind(reason)
            .bind(chrono::Utc::now())
            .bind(enrollment.missing_id)
            .execute(ctx.pool())
            .await
            .change_context(AppError::Unknown)?;
            error!(file = ?event.prev_file_path, missing_id = enrollment.missing_id, "segment_processor failed; durable pending lifecycle row retained: {:?}", e);
            continue;
        }
        let original_path = paths
            .first()
            .cloned()
            .unwrap_or_else(|| event.prev_file_path.clone());

        // 样片截取是一次性、失败开放的旁路，不改变当前分段的上传结果。
        let sample_store = AudioSampleStore::for_working_directory(
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        );
        maybe_capture_reference_sample(&original_path, &sample_store).await;

        let AttemptClaim::Claimed(attempt_token) =
            claim_enrolled_attempt(ctx.pool(), &enrollment, &upload_context.line_key, None).await?
        else {
            info!(
                missing_id = enrollment.missing_id,
                "segment lifecycle row was already claimed or completed; skipping replay"
            );
            continue;
        };

        let effective_config = ctx.config();
        let repair_enabled = effective_config.timestamp_repair.unwrap_or(true);
        match upload_enrolled_with_watchdog(
            &original_path,
            upload_context,
            repair_enabled,
            effective_config.audio_normalization_enabled,
            effective_config.effective_audio_target_lufs(),
            ctx.pool(),
            enrollment.missing_id,
            &attempt_token,
        )
        .await
        {
            Ok((video, outcome, _normalization_artifact, _attempt_guard)) => {
                // 上传成功后落库累积。落库失败则保留本地文件（不删），保证「未 durable 不删」。
                if let Err(e) =
                    persist_segment(ctx.pool(), &mut archive, video, &enrollment, &attempt_token)
                        .await
                {
                    error!(file = ?original_path, "落库累积失败，保留本地文件: {:?}", e);
                    // Repaired 的临时修复件未 durable，清理掉避免残留。
                    if let RepairOutcome::Repaired(fixed) = &outcome {
                        let _ = tokio::fs::remove_file(fixed).await;
                    }
                    continue;
                }
                // durable，按 outcome 处理本地文件。
                match outcome {
                    RepairOutcome::Unfixable => {
                        // 保留本地原文件 + 告警，跳过自动删除。
                        error!(file = ?original_path, "时间戳无法修复，保留本地文件待手动处理");
                        notify_alert(
                            ctx.config().cookie_health_webhook.as_deref(),
                            "biliup 时间戳修复失败",
                            &format!(
                                "分段 {} 时间戳异常且无法自动修复，已保留本地文件，请手动处理（B 站「修改视频」重传）。",
                                original_path.display()
                            ),
                        );
                    }
                    RepairOutcome::Repaired(fixed) => {
                        // 先删修复临时件，再删原始 paths（原片+弹幕）。
                        let _ = tokio::fs::remove_file(&fixed).await;
                        paths.extend(recovery_source_paths);
                        if let Err(e) = execute_postprocessor(paths, ctx).await {
                            error!(file = ?original_path, "per-segment postprocessor failed: {:?}", e);
                        }
                    }
                    RepairOutcome::Clean => {
                        paths.extend(recovery_source_paths);
                        if let Err(e) = execute_postprocessor(paths, ctx).await {
                            error!(file = ?original_path, "per-segment postprocessor failed: {:?}", e);
                        }
                    }
                }
            }
            Err(e) => {
                let err = format!("{e:?}");
                fail_enrolled_attempt(
                    ctx.pool(),
                    enrollment.missing_id,
                    &attempt_token,
                    err,
                    chrono::Utc::now(),
                )
                .await?;
                error!(file = ?original_path, segment_order = enrollment.segment_order, "upload_single_file failed; lifecycle row retained: {:?}", e);
            }
        }
    }
    Ok(if archive.videos.is_empty() {
        None
    } else {
        Some(archive)
    })
}

/// 上传前时间戳检测/修复 + 上传。返回 (Video, RepairOutcome) 供调用方决定本地文件清理与告警。
/// repair_enabled=false 时跳过 ffmpeg，等价 Clean。上传失败时会清理自身产生的临时修复件。
async fn upload_single_file_with_repair(
    original_path: &Path,
    context: &UploadContext,
    repair_enabled: bool,
    normalization_enabled: bool,
    target_lufs: f64,
    progress_tx: Option<mpsc::UnboundedSender<UploadProgress>>,
) -> AppResult<(Video, RepairOutcome, Option<TempArtifact>)> {
    let normalization = if normalization_enabled {
        normalize_for_upload(original_path, target_lufs, &SystemAudioFfmpeg::default()).await
    } else {
        NormalizationOutcome::Original {
            reason: crate::server::common::audio_normalization::OriginalReason::NoAudio,
        }
    };
    let normalization_artifact = match normalization {
        NormalizationOutcome::Normalized { artifact, .. } => Some(artifact),
        NormalizationOutcome::Original { reason } => {
            if normalization_enabled {
                info!(audio_normalization = "fallback", file = %original_path.display(), ?reason);
            }
            None
        }
    };
    let normalized_path = normalization_artifact
        .as_ref()
        .map(TempArtifact::path)
        .unwrap_or(original_path);
    let outcome = if repair_enabled {
        normalize_timestamps(normalized_path, &SystemFfmpeg).await
    } else {
        RepairOutcome::Clean
    };
    let upload_path = match &outcome {
        RepairOutcome::Repaired(fixed) => fixed.clone(),
        _ => normalized_path.to_path_buf(),
    };
    let result = {
        // CPU/磁盘处理不占网络上传 permit。
        let _permit = acquire_global_upload_permit().await;
        upload_single_file(&upload_path, context, progress_tx).await
    };
    match result {
        Ok(video) => Ok((video, outcome, normalization_artifact)),
        Err(e) => {
            if let RepairOutcome::Repaired(fixed) = &outcome {
                let _ = tokio::fs::remove_file(fixed).await;
            }
            if let Some(artifact) = &normalization_artifact {
                artifact.cleanup().await;
            }
            Err(e)
        }
    }
}

async fn upload_single_file(
    file_path: &Path,
    context: &UploadContext,
    progress_tx: Option<mpsc::UnboundedSender<UploadProgress>>,
) -> AppResult<Video> {
    let video_path = file_path;
    let UploadContext {
        bilibili,
        line,
        threads: limit,
        client,
        rate_gate,
        pool,
        line_key,
        health_webhook,
    } = context;

    info!(
        "开始上传文件：{:?}",
        video_path
            .canonicalize()
            .change_context(AppError::Unknown)?
            .to_str()
    );
    info!("线路选择：{line:?}");
    let video_file = VideoFile::new(video_path).change_context(AppError::Unknown)?;
    let total_size = video_file.total_size;
    let file_name = video_file.file_name.clone();
    upload_rate_gate::before_pre_upload(*rate_gate, pool).await?;
    let uploader = match line.pre_upload(bilibili, video_file).await {
        Ok(uploader) => {
            upload_rate_gate::record_success(*rate_gate, pool).await;
            uploader
        }
        Err(Kind::RateLimit { code: 601, message }) => {
            let until = upload_rate_gate::record_rate_limited(*rate_gate, pool).await?;
            return Err(error_stack::Report::new(AppError::Custom(format!(
                "Bilibili pre_upload rate limited (601: {message}); global cooldown until {until}"
            ))));
        }
        Err(error) => {
            upload_rate_gate::record_non_rate_limit_failure(*rate_gate).await;
            record_line_kind_failure(pool, line_key, health_webhook.as_deref(), &error).await;
            return Err(error_stack::Report::new(error).change_context(AppError::Unknown));
        }
    };

    let instant = Instant::now();

    let video = match uploader
        .upload_with_observer(
            client.clone(),
            *limit,
            |vs| {
                vs.map(|vs| {
                    let chunk = vs?;
                    let len = chunk.len();
                    Ok((chunk, len))
                })
            },
            move |progress| {
                if let Some(tx) = &progress_tx {
                    let _ = tx.send(progress);
                }
            },
        )
        .await
    {
        Ok(video) => video,
        Err(error) => {
            record_line_kind_failure(pool, line_key, health_webhook.as_deref(), &error).await;
            return Err(error_stack::Report::new(error).change_context(AppError::Unknown));
        }
    };
    if let Err(error) = upload_line_health::record_success(pool, line_key).await {
        warn!(
            ?error,
            line = line_key,
            "failed to clear upload line breaker after success"
        );
    }
    let t = instant.elapsed().as_millis();
    info!(
        "Upload completed: {file_name} => cost {:.2}s, {:.2} MB/s.",
        t as f64 / 1000.,
        total_size as f64 / 1000. / t as f64
    );
    Ok(video)
}

async fn record_line_kind_failure(
    pool: &ConnectionPool,
    line_key: &str,
    webhook: Option<&str>,
    error: &Kind,
) {
    let kind = classify_kind(error);
    let summary = sanitize_error(&format!("{error:?}"));
    match upload_line_health::record_failure(pool, line_key, kind, &summary, chrono::Utc::now())
        .await
    {
        Ok(true) => notify_alert(
            webhook,
            "biliup 上传线路 TLS 熔断",
            &format!(
                "B 站上游需续签 {line_key} 证书；该线路已冷却 24 小时，上传会自动换线。请勿关闭 TLS 证书验证。"
            ),
        ),
        Ok(false) => {}
        Err(db_error) => warn!(
            ?db_error,
            line = line_key,
            "failed to persist upload line failure"
        ),
    }
}

async fn record_watchdog_failure(context: &UploadContext, kind: UploadFailureKind, summary: &str) {
    if let Err(error) = upload_line_health::record_failure(
        &context.pool,
        &context.line_key,
        kind,
        summary,
        chrono::Utc::now(),
    )
    .await
    {
        warn!(
            ?error,
            line = context.line_key,
            "failed to persist watchdog line failure"
        );
    }
}

async fn persist_attempt_progress(
    pool: &ConnectionPool,
    missing_id: i64,
    attempt_token: &str,
    progress: UploadProgress,
) -> AppResult<bool> {
    let uploaded_bytes = i64::try_from(progress.uploaded_bytes).unwrap_or(i64::MAX);
    let total_bytes = i64::try_from(progress.total_bytes).unwrap_or(i64::MAX);
    let now = chrono::Utc::now();
    let updated = sqlx::query(
        "UPDATE upload_missing_segment \
         SET uploaded_bytes = ?1, total_bytes = ?2, last_progress_at = ?3, updated_at = ?3 \
         WHERE id = ?4 AND lifecycle_version = 2 AND status = 'uploading' \
           AND attempt_token = ?5",
    )
    .bind(uploaded_bytes)
    .bind(total_bytes)
    .bind(now)
    .bind(missing_id)
    .bind(attempt_token)
    .execute(pool)
    .await
    .change_context(AppError::Unknown)?;
    Ok(updated.rows_affected() == 1)
}

enum AttemptEvent<T> {
    Completed(T),
    Cancelled,
    NoProgressTimeout,
    TotalUploadTimeout,
    Progress(UploadProgress),
    ProgressClosed,
}

async fn next_attempt_event<F>(
    mut upload: Pin<&mut F>,
    cancellation: &CancellationToken,
    mut no_progress: Pin<&mut tokio::time::Sleep>,
    mut total: Pin<&mut tokio::time::Sleep>,
    progress_rx: &mut mpsc::UnboundedReceiver<UploadProgress>,
    progress_open: bool,
) -> AttemptEvent<F::Output>
where
    F: Future,
{
    tokio::select! {
        result = &mut upload => AttemptEvent::Completed(result),
        _ = cancellation.cancelled() => AttemptEvent::Cancelled,
        _ = &mut no_progress => AttemptEvent::NoProgressTimeout,
        _ = &mut total => AttemptEvent::TotalUploadTimeout,
        progress = progress_rx.recv(), if progress_open => match progress {
            Some(progress) => AttemptEvent::Progress(progress),
            None => AttemptEvent::ProgressClosed,
        },
    }
}

#[allow(clippy::too_many_arguments)]
async fn upload_enrolled_with_watchdog(
    original_path: &Path,
    context: &UploadContext,
    repair_enabled: bool,
    normalization_enabled: bool,
    target_lufs: f64,
    pool: &ConnectionPool,
    missing_id: i64,
    attempt_token: &str,
) -> AppResult<(
    Video,
    RepairOutcome,
    Option<TempArtifact>,
    Option<AttemptGuard>,
)> {
    let (guard, cancellation) = register_attempt(missing_id, attempt_token);
    let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
    let upload = upload_single_file_with_repair(
        original_path,
        context,
        repair_enabled,
        normalization_enabled,
        target_lufs,
        Some(progress_tx),
    );
    pin!(upload);

    let no_progress = tokio::time::sleep(NO_PROGRESS_TIMEOUT);
    let total = tokio::time::sleep(TOTAL_UPLOAD_TIMEOUT);
    pin!(no_progress);
    pin!(total);
    let mut last_persist = Instant::now();
    let mut persisted_bytes = 0u64;
    let mut progress_open = true;

    loop {
        match next_attempt_event(
            upload.as_mut(),
            &cancellation,
            no_progress.as_mut(),
            total.as_mut(),
            &mut progress_rx,
            progress_open,
        )
        .await
        {
            AttemptEvent::Completed(result) => {
                return result
                    .map(|(video, outcome, artifact)| (video, outcome, artifact, Some(guard)));
            }
            AttemptEvent::Cancelled => {
                return Err(error_stack::Report::new(AppError::Custom(
                    "upload attempt cancelled by manual retry".to_string(),
                )));
            }
            AttemptEvent::NoProgressTimeout => {
                record_watchdog_failure(
                    context,
                    UploadFailureKind::RequestTimeout,
                    "no_progress_timeout",
                )
                .await;
                return Err(error_stack::Report::new(AppError::Custom(
                    "no_progress_timeout".to_string(),
                )));
            }
            AttemptEvent::TotalUploadTimeout => {
                record_watchdog_failure(
                    context,
                    UploadFailureKind::RequestTimeout,
                    "total_upload_timeout",
                )
                .await;
                return Err(error_stack::Report::new(AppError::Custom(
                    "total_upload_timeout".to_string(),
                )));
            }
            AttemptEvent::ProgressClosed => progress_open = false,
            AttemptEvent::Progress(progress) => {
                no_progress
                    .as_mut()
                    .reset(tokio::time::Instant::now() + NO_PROGRESS_TIMEOUT);
                let should_persist = persisted_bytes == 0
                    || progress.uploaded_bytes.saturating_sub(persisted_bytes)
                        >= PROGRESS_PERSIST_BYTES
                    || last_persist.elapsed() >= PROGRESS_PERSIST_INTERVAL;
                if should_persist
                    && persist_attempt_progress(pool, missing_id, attempt_token, progress).await?
                {
                    persisted_bytes = progress.uploaded_bytes;
                    last_persist = Instant::now();
                    info!(
                        missing_id,
                        chunk = progress.chunk_index,
                        uploaded_bytes = progress.uploaded_bytes,
                        total_bytes = progress.total_bytes,
                        "upload chunk acknowledged"
                    );
                }
            }
        }
    }
}

enum CancelAttemptResult {
    Exited,
    TimedOut,
    NotRegistered,
}

async fn cancel_registered_attempt(missing_id: i64, attempt_token: &str) -> CancelAttemptResult {
    let registration = attempt_registry()
        .lock()
        .expect("attempt registry poisoned")
        .get(&missing_id)
        .filter(|entry| entry.attempt_token == attempt_token)
        .cloned();
    let Some(registration) = registration else {
        return CancelAttemptResult::NotRegistered;
    };
    registration.cancellation.cancel();
    if tokio::time::timeout(CANCEL_WAIT_TIMEOUT, registration.completion.wait())
        .await
        .is_ok()
    {
        CancelAttemptResult::Exited
    } else {
        CancelAttemptResult::TimedOut
    }
}

async fn recover_due_missing_segments(
    upload_context: &UploadContext,
    ctx: &Context,
    session_row_id: i64,
    archive: &mut LiveArchive,
) -> AppResult<()> {
    let now = chrono::Utc::now();
    let rows = due_missing_segments_for_session(ctx.pool(), session_row_id, now).await?;
    for mut row in rows {
        match check_recovery_eligibility(ctx.pool(), &row, None, now).await? {
            RecoveryEligibility::Eligible => {}
            RecoveryEligibility::SourceMissing => {
                mark_source_missing(
                    ctx.pool(),
                    row.id,
                    "source file is no longer a regular file during silent recovery",
                    now,
                )
                .await?;
                info!(missing_id = row.id, file = %row.file_path, "silent recovery stopped: source missing");
                continue;
            }
            decision => {
                info!(
                    missing_id = row.id,
                    ?decision,
                    "silent recovery skipped ineligible segment"
                );
                continue;
            }
        }
        let selected = match select_recovery_line(
            ctx.pool(),
            &upload_context.client.client,
            row.line_index,
            upload_context.health_webhook.as_deref(),
        )
        .await
        {
            Ok(selected) => selected,
            Err(error) => {
                warn!(
                    missing_id = row.id,
                    ?error,
                    "no recovery line available; keeping row due"
                );
                continue;
            }
        };
        let file_path = row.file_path.clone();
        let segment_order = row.segment_order;
        let row_id = row.id;
        let v2_enrollment = (row.lifecycle_version == 2).then(|| SegmentEnrollment {
            missing_id: row.id,
            upload_session_id: session_row_id,
            segment_order: row.segment_order,
            normalized_file_path: PathBuf::from(
                row.normalized_file_path
                    .clone()
                    .unwrap_or_else(|| row.file_path.clone()),
            ),
            total_bytes: row
                .total_bytes
                .and_then(|value| u64::try_from(value).ok())
                .unwrap_or(0),
            duplicate: false,
        });
        let attempt_token = if let Some(enrollment) = &v2_enrollment {
            let AttemptClaim::Claimed(token) = claim_enrolled_attempt(
                ctx.pool(),
                enrollment,
                &selected.key,
                selected.recovery_index,
            )
            .await?
            else {
                continue;
            };
            Some(token)
        } else {
            row.status = "uploading".to_string();
            row.updated_at = chrono::Utc::now();
            row = row
                .update_all_fields(ctx.pool())
                .await
                .change_context(AppError::Unknown)?;
            None
        };

        let recovery_context = UploadContext {
            bilibili: upload_context.bilibili.clone(),
            line: selected.line,
            threads: upload_context.threads,
            client: upload_context.client.clone(),
            rate_gate: upload_context.rate_gate,
            pool: upload_context.pool.clone(),
            line_key: selected.key,
            health_webhook: upload_context.health_webhook.clone(),
        };

        let path = PathBuf::from(&file_path);
        let effective_config = ctx.config();
        let repair_enabled = effective_config.timestamp_repair.unwrap_or(true);
        let result = if let (Some(enrollment), Some(token)) = (&v2_enrollment, &attempt_token) {
            upload_enrolled_with_watchdog(
                &path,
                &recovery_context,
                repair_enabled,
                effective_config.audio_normalization_enabled,
                effective_config.effective_audio_target_lufs(),
                ctx.pool(),
                enrollment.missing_id,
                token,
            )
            .await
        } else {
            upload_single_file_with_repair(
                &path,
                &recovery_context,
                repair_enabled,
                effective_config.audio_normalization_enabled,
                effective_config.effective_audio_target_lufs(),
                None,
            )
            .await
            .map(|(video, outcome, artifact)| (video, outcome, artifact, None))
        };

        match result {
            Ok((video, outcome, _normalization_artifact, _attempt_guard)) => {
                if let (Some(enrollment), Some(token)) = (&v2_enrollment, &attempt_token) {
                    persist_segment(ctx.pool(), archive, video, enrollment, token).await?;
                } else {
                    let updated = insert_session_video_at_order(
                        ctx.pool(),
                        session_row_id,
                        video,
                        segment_order,
                    )
                    .await?;
                    archive.videos = updated;
                    mark_retry_success(&mut row, chrono::Utc::now());
                    row.update_all_fields(ctx.pool())
                        .await
                        .change_context(AppError::Unknown)?;
                }
                match outcome {
                    RepairOutcome::Unfixable => {
                        error!(row_id, file = ?path, "补传分段时间戳无法修复，保留本地文件待手动处理");
                        notify_alert(
                            ctx.config().cookie_health_webhook.as_deref(),
                            "biliup 时间戳修复失败",
                            &format!(
                                "补传分段 {} 时间戳异常且无法自动修复，已保留本地文件，请手动处理。",
                                path.display()
                            ),
                        );
                    }
                    RepairOutcome::Repaired(fixed) => {
                        let _ = tokio::fs::remove_file(&fixed).await;
                        if let Err(e) = execute_postprocessor(vec![path], ctx).await {
                            error!(
                                row_id,
                                "postprocessor failed after missing segment recovery: {:?}", e
                            );
                        }
                    }
                    RepairOutcome::Clean => {
                        if let Err(e) = execute_postprocessor(vec![path], ctx).await {
                            error!(
                                row_id,
                                "postprocessor failed after missing segment recovery: {:?}", e
                            );
                        }
                    }
                }
            }
            Err(e) => {
                if let Some(token) = &attempt_token {
                    fail_enrolled_attempt(
                        ctx.pool(),
                        row.id,
                        token,
                        format!("{e:?}"),
                        chrono::Utc::now(),
                    )
                    .await?;
                } else {
                    mark_retry_failure(&mut row, format!("{e:?}"), chrono::Utc::now());
                    row.update_all_fields(ctx.pool())
                        .await
                        .change_context(AppError::Unknown)?;
                }
            }
        }
    }
    Ok(())
}

pub async fn submit_to_bilibili(
    bilibili: &BiliBili,
    studio: &Studio,
    submit_api: Option<&str>,
) -> AppResult<ResponseData> {
    let submit_option = match submit_api {
        Some(submit) => SubmitOption::from_str(submit).unwrap_or(SubmitOption::App),
        _ => SubmitOption::App,
    };

    let result = match submit_option {
        SubmitOption::BCutAndroid => bilibili
            .submit_by_bcut_android(studio, None)
            .await
            .change_context(AppError::Unknown)?,
        SubmitOption::Web => bilibili
            .submit_by_web(studio, None)
            .await
            .change_context(AppError::Unknown)?,
        _ => bilibili
            .submit_by_app(studio, None)
            .await
            .change_context(AppError::Unknown)?,
    };
    info!("Submit successful");
    Ok(result)
}

// 解析投稿的「转载来源」(source) 字段。
// 前端表单留空时会把 copyright_source 提交为空字符串 `Some("")`，
// 若直接透传则 B 站接口收到空 source，且不会回退到直播间地址。
// 这里把 None 以及空白字符串都视作「未填写」，统一回退到直播间地址，
fn resolve_source(copyright_source: Option<&str>, fallback_url: &str) -> String {
    match copyright_source.map(str::trim) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => fallback_url.to_string(),
    }
}

/// 封面背景图所在目录。与数据库同在 `data/` 下，因此随现有备份一起被覆盖。
///
/// 与数据库路径（`data/data.sqlite3`）一样是相对工作目录的——容器里工作目录即挂载卷，
/// 换挂载点或迁移部署时数据库里存的文件名无需跟着改。
pub(crate) const BACKGROUND_DIR: &str = "data/cover-backgrounds";

// 把库里存的一个值变成可用的背景图路径，值不可用时返回 None（等同于「没填」）。
//
// 空值语义与上面的 resolve_source 一致——NULL 和空白字符串都算「没填」，
// 因为前端表单留空提交的是 Some("")，不做 trim 就会当成配了一张名为空的图。
//
// 库里存的必须是「一个文件名」。带目录、`..`、绝对路径一律按没填处理：
// Path::join 碰上绝对路径会把基路径整段丢掉（join("/etc/x") 就是 "/etc/x"），
// 不在这里拦住，库里一个值就能让渲染器去读目录外的文件。
fn background_path(value: Option<&str>) -> Option<PathBuf> {
    let name = value.map(str::trim).filter(|s| !s.is_empty())?;

    match single_segment_name(name) {
        Some(file_name) => Some(Path::new(BACKGROUND_DIR).join(file_name)),
        // 配了值却用不了，比压根没配更难排查：不留一行日志的话，
        // 用户只会看到封面莫名其妙变黑，还以为是自己没保存成功。
        None => {
            warn!(value = name, "封面背景图配置的不是一个文件名，按未配置处理");
            None
        }
    }
}

// 解析封面背景，三级回退：主播 → 模板 → 纯黑。
//
// 不可用的值等同于没填，继续往下一级走而不是断在原地——主播那一级写错一个路径，
// 不该把模板配好的背景一起废掉。
//
// 纯函数：只做路径拼接，不碰数据库也不读文件；图存不存在由渲染器判断（读不到会退回纯黑）。
fn resolve_background(
    streamer_background: Option<&str>,
    template_background: Option<&str>,
) -> Background {
    background_path(streamer_background)
        .or_else(|| background_path(template_background))
        .map_or(Background::Black, Background::Image)
}

/// `streamer_background` 是主播那一级的背景图文件名，覆盖模板的同名设置；
/// 取不到主播行时传 None，行为与只有模板级时一致。
pub(crate) async fn build_studio(
    upload_config: &UploadStreamer,
    streamer_background: Option<&str>,
    bilibili: &BiliBili,
    videos: Vec<Video>,
    recorder: &Recorder,
) -> AppResult<Studio> {
    // 使用 Builder 模式简化构建
    let mut studio: Studio = Studio::builder()
        .desc(recorder.format(&upload_config.description.clone().unwrap_or_default()))
        .maybe_dtime(upload_config.dtime)
        .maybe_copyright(upload_config.copyright)
        .cover(upload_config.cover_path.clone().unwrap_or_default())
        .dynamic(upload_config.dynamic.clone().unwrap_or_default())
        .source(resolve_source(
            upload_config.copyright_source.as_deref(),
            &recorder.streamer_info.url,
        ))
        .tag(upload_config.tags.join(","))
        .maybe_tid(upload_config.tid)
        .title(recorder.format_filename())
        .videos(videos)
        .dolby(upload_config.dolby.unwrap_or_default())
        // .lossless_music(upload_config.)
        .no_reprint(upload_config.no_reprint.unwrap_or_default())
        .charging_pay(upload_config.charging_pay.unwrap_or_default())
        .up_close_reply(upload_config.up_close_reply.unwrap_or_default())
        .up_selection_reply(upload_config.up_selection_reply.unwrap_or_default())
        .up_close_danmu(upload_config.up_close_danmu.unwrap_or_default())
        .maybe_is_only_self(upload_config.is_only_self)
        .maybe_desc_v2(None)
        .extra_fields(
            serde_json::from_str(&upload_config.extra_fields.clone().unwrap_or_default())
                .unwrap_or_default(), // 处理额外字段
        )
        .build();
    // 自动封面：cover_template 非空则生成黑底封面，覆盖 studio.cover；
    // _auto_cover_tmp 持有临时文件，build_studio 返回（上传完成后）时自动删除。
    let mut _auto_cover_tmp: Option<tempfile::NamedTempFile> = None;
    if let Some(tpl) = upload_config
        .cover_template
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let text = recorder.format(tpl);
        let lines = split_template_lines(&text);
        let opts = CoverOptions {
            background: resolve_background(
                streamer_background,
                upload_config.cover_background.as_deref(),
            ),
            ..CoverOptions::default()
        };
        match render_to_tempfile(&lines, &opts) {
            Ok(f) => {
                studio.cover = f.path().to_string_lossy().into_owned();
                _auto_cover_tmp = Some(f);
            }
            Err(e) => error!(e=?e, "生成自动封面失败，回退到 cover_path"),
        }
    }
    // 处理封面上传
    if !studio.cover.is_empty()
        && let Ok(c) = &std::fs::read(&studio.cover).inspect_err(|e| error!(e=?e))
        && let Ok(url) = bilibili.cover_up(c).await.inspect_err(|e| error!(e=?e))
    {
        studio.cover = url;
    };

    Ok(studio)
}

pub async fn execute_postprocessor(video_paths: Vec<PathBuf>, ctx: &Context) -> AppResult<()> {
    if let Some(processor) = &ctx.live_streamer().postprocessor {
        let paths: Vec<&Path> = video_paths.iter().map(|p| p.as_path()).collect();
        process_video(&paths, processor).await?;
    }
    Ok(())
}

pub async fn upload(
    cookie_file: impl AsRef<Path>,
    proxy: Option<&str>,
    line: Option<UploadLine>,
    video_paths: &[PathBuf],
    limit: usize,
    config: &Config,
    pool: &ConnectionPool,
) -> AppResult<(BiliBili, Vec<Video>)> {
    let bilibili = login_by_cookies(&cookie_file, proxy).await;
    let bilibili = match bilibili {
        Err(Kind::IO(_)) => bilibili.change_context_lazy(|| {
            AppError::Custom(format!(
                "open cookies file: {}",
                &cookie_file.as_ref().to_string_lossy()
            ))
        })?,
        _ => bilibili.change_context_lazy(|| AppError::Unknown)?,
    };

    let client = StatelessClient::default();
    let mut videos = Vec::new();
    let requested_line = match line {
        Some(UploadLine::Bldsa) => line::bldsa(),
        Some(UploadLine::Cnbldsa) => line::cnbldsa(),
        Some(UploadLine::Andsa) => line::andsa(),
        Some(UploadLine::Atdsa) => line::atdsa(),
        Some(UploadLine::Bda2) => line::bda2(),
        Some(UploadLine::Cnbd) => line::cnbd(),
        Some(UploadLine::Anbd) => line::anbd(),
        Some(UploadLine::Atbd) => line::atbd(),
        Some(UploadLine::Tx) => line::tx(),
        Some(UploadLine::Cntx) => line::cntx(),
        Some(UploadLine::Antx) => line::antx(),
        Some(UploadLine::Attx) => line::attx(),
        // Some(UploadLine::Bda) => line::bda(),
        Some(UploadLine::Txa) => line::txa(),
        Some(UploadLine::Alia) => line::alia(),
        _ => {
            probe_healthy_line(
                pool,
                &client.client,
                config.cookie_health_webhook.as_deref(),
            )
            .await?
            .line
        }
    };
    let requested_key = requested_line.key().to_string();
    let selected = match upload_line_health::acquire_line(pool, &requested_key, chrono::Utc::now())
        .await?
    {
        LineAvailability::Available => SelectedLine {
            line: requested_line,
            key: requested_key,
            recovery_index: None,
        },
        LineAvailability::Cooling { until, reason } => {
            warn!(line = requested_key, %until, ?reason, "explicit upload line is cooling; overriding");
            select_recovery_line(
                pool,
                &client.client,
                0,
                config.cookie_health_webhook.as_deref(),
            )
            .await?
        }
    };
    let line = selected.line;
    let line_key = selected.key;
    for video_path in video_paths {
        println!(
            "{:?}",
            video_path
                .canonicalize()
                .change_context_lazy(|| AppError::Unknown)?
                .to_str()
        );
        info!("{line:?}");
        let video_file = VideoFile::new(video_path).change_context_lazy(|| AppError::Unknown)?;
        let total_size = video_file.total_size;
        let file_name = video_file.file_name.clone();
        let settings = UploadRateGateSettings::from(config);
        upload_rate_gate::before_pre_upload(settings, pool).await?;
        let uploader = match line.pre_upload(&bilibili, video_file).await {
            Ok(uploader) => {
                upload_rate_gate::record_success(settings, pool).await;
                uploader
            }
            Err(Kind::RateLimit { code: 601, message }) => {
                let until = upload_rate_gate::record_rate_limited(settings, pool).await?;
                return Err(error_stack::Report::new(AppError::Custom(format!(
                    "Bilibili pre_upload rate limited (601: {message}); global cooldown until {until}"
                ))));
            }
            Err(error) => {
                upload_rate_gate::record_non_rate_limit_failure(settings).await;
                record_line_kind_failure(
                    pool,
                    &line_key,
                    config.cookie_health_webhook.as_deref(),
                    &error,
                )
                .await;
                return Err(error_stack::Report::new(error).change_context(AppError::Unknown));
            }
        };

        let instant = Instant::now();

        let video = match uploader
            .upload(client.clone(), limit, |vs| {
                vs.map(|vs| {
                    let chunk = vs?;
                    let len = chunk.len();
                    Ok((chunk, len))
                })
            })
            .await
        {
            Ok(video) => video,
            Err(error) => {
                record_line_kind_failure(
                    pool,
                    &line_key,
                    config.cookie_health_webhook.as_deref(),
                    &error,
                )
                .await;
                return Err(error_stack::Report::new(error).change_context(AppError::Unknown));
            }
        };
        upload_line_health::record_success(pool, &line_key).await?;
        let t = instant.elapsed().as_millis();
        info!(
            "Upload completed: {file_name} => cost {:.2}s, {:.2} MB/s.",
            t as f64 / 1000.,
            total_size as f64 / 1000. / t as f64
        );
        videos.push(video);
    }

    Ok((bilibili, videos))
}

#[derive(Debug, serde::Serialize)]
pub struct LocalSegmentRescanResult {
    pub live_streamer_id: i64,
    pub streamer_info_id: i64,
    pub upload_session_id: i64,
    pub scanned: usize,
    pub queued: usize,
    pub skipped_known: usize,
    pub skipped_invalid: usize,
    pub skipped_finalized: bool,
}

fn is_rescan_filename_candidate(
    file_name: &str,
    literal_prefix: &str,
    streamer_name: &str,
) -> bool {
    (!literal_prefix.is_empty() && file_name.starts_with(literal_prefix))
        || (!streamer_name.is_empty() && file_name.contains(streamer_name))
}

fn resolve_recorded_path(root: &Path, path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

/// Studio does not expose our local lifecycle id. A remote filename is the only safe identity:
/// titles intentionally collide for different source segments. Checking it before `edit_by_app`
/// makes a retried finalized legacy recovery idempotent.
fn studio_already_contains_video(studio: &Studio, video: &Video) -> bool {
    !video.filename.is_empty()
        && studio
            .videos
            .iter()
            .any(|existing| existing.filename == video.filename)
}

/// 按主播本场 StreamerInfo 补扫本地有效媒体，并绑定到该场未完成的 upload_session。
/// filelist 是首选来源；同时扫描工作目录，覆盖“消息尚未被旧 UActor 消费，因此连
/// filelist 都没有”的历史遗留。目录推断候选必须同时满足本场时间和文件名前缀。
pub async fn rescan_local_valid_segments(
    config: &Config,
    pool: &ConnectionPool,
    streamer_info_id: i64,
    working_directory: &Path,
) -> AppResult<LocalSegmentRescanResult> {
    let streamer_info = StreamerInfo::select()
        .where_("id = ?")
        .bind(streamer_info_id)
        .fetch_one(pool)
        .await
        .change_context(AppError::Unknown)?;
    let live_streamer = LiveStreamer::select()
        .where_("url = ?")
        .bind(&streamer_info.url)
        .fetch_one(pool)
        .await
        .change_context(AppError::Unknown)?;

    let session = sqlx::query_as::<_, UploadSession>(
        "SELECT * FROM upload_session \
         WHERE live_streamer_id = ? AND streamer_info_id = ? AND status != 'finalized' \
         ORDER BY updated_at DESC, id DESC LIMIT 1",
    )
    .bind(live_streamer.id)
    .bind(streamer_info_id)
    .fetch_optional(pool)
    .await
    .change_context(AppError::Unknown)?;
    let session = match session {
        Some(session) => session,
        None => {
            if let Some(session_id) =
                finalized_session_for_streamer_info(pool, live_streamer.id, streamer_info_id)
                    .await?
            {
                record_recovery_audit(
                    pool,
                    live_streamer.id,
                    streamer_info_id,
                    working_directory,
                    "rescan_skipped_finalized_session",
                    chrono::Utc::now(),
                )
                .await?;
                return Ok(LocalSegmentRescanResult {
                    live_streamer_id: live_streamer.id,
                    streamer_info_id,
                    upload_session_id: session_id,
                    scanned: 0,
                    queued: 0,
                    skipped_known: 0,
                    skipped_invalid: 0,
                    skipped_finalized: true,
                });
            }
            insert_uploading_session(pool, live_streamer.id, streamer_info_id, &[]).await?
        }
    };
    let videos = parse_videos(&session.videos_json);
    let uploaded_stems: HashSet<String> = videos
        .iter()
        .flat_map(|video| {
            video
                .title
                .iter()
                .map(|title| crate::server::common::upload_session::filename_stem(Path::new(title)))
                .chain(std::iter::once(
                    crate::server::common::upload_session::filename_stem(Path::new(
                        &video.filename,
                    )),
                ))
        })
        .filter(|stem| !stem.is_empty())
        .collect();

    let known_paths: HashSet<String> = sqlx::query_scalar::<_, String>(
        "SELECT file_path FROM upload_missing_segment WHERE live_streamer_id = ?",
    )
    .bind(live_streamer.id)
    .fetch_all(pool)
    .await
    .change_context(AppError::Unknown)?
    .into_iter()
    .map(|path| {
        resolve_recorded_path(working_directory, path)
            .display()
            .to_string()
    })
    .collect();

    let mut candidates = BTreeSet::new();
    for item in FileItem::select()
        .where_("streamer_info_id = ?")
        .bind(streamer_info_id)
        .fetch_all(pool)
        .await
        .change_context(AppError::Unknown)?
    {
        candidates.insert(resolve_recorded_path(working_directory, item.file));
    }

    let filename_prefix = live_streamer
        .filename_prefix
        .clone()
        .or_else(|| config.filename_prefix.clone());
    let filename_template =
        Recorder::new(filename_prefix, streamer_info.clone()).filename_template();
    let literal_prefix = filename_template.split('%').next().unwrap_or_default();
    let session_cutoff = streamer_info.date - chrono::Duration::minutes(5);
    if let Ok(entries) = std::fs::read_dir(working_directory) {
        for entry in entries.flatten() {
            let path = entry.path();
            let extension = path
                .extension()
                .and_then(|value| value.to_str())
                .map(str::to_ascii_lowercase);
            if !matches!(
                extension.as_deref(),
                Some("flv" | "ts" | "mp4" | "mkv" | "m3u8")
            ) {
                continue;
            }
            let file_name = entry.file_name().to_string_lossy().into_owned();
            if !is_rescan_filename_candidate(&file_name, literal_prefix, &streamer_info.name) {
                continue;
            }
            let modified = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .map(chrono::DateTime::<chrono::Utc>::from);
            if modified.is_some_and(|modified| modified < session_cutoff) {
                continue;
            }
            candidates.insert(path);
        }
    }

    let validator = FileValidator::new(config.filtering_threshold * 1_000_000, true);
    let mut result = LocalSegmentRescanResult {
        live_streamer_id: live_streamer.id,
        streamer_info_id,
        upload_session_id: session.id,
        scanned: 0,
        queued: 0,
        skipped_known: 0,
        skipped_invalid: 0,
        skipped_finalized: false,
    };
    for path in candidates {
        if !path.is_file() {
            continue;
        }
        result.scanned += 1;
        let path_key = path.display().to_string();
        let stem = crate::server::common::upload_session::filename_stem(&path);
        if known_paths.contains(&path_key) || uploaded_stems.contains(&stem) {
            result.skipped_known += 1;
            continue;
        }
        if !matches!(validator.validate(&path)?, MediaValidation::Valid) {
            result.skipped_invalid += 1;
            continue;
        }
        let danmaku_path = path.with_extension("xml");
        let request = EnrollmentRequest {
            live_streamer_id: live_streamer.id,
            streamer_info_id,
            file_path: path.clone(),
            normalized_file_path: normalize_segment_path(&path)?,
            danmaku_file_path: danmaku_path.is_file().then_some(danmaku_path),
            total_bytes: std::fs::metadata(&path)
                .change_context(AppError::Unknown)?
                .len(),
            now: chrono::Utc::now(),
            recovery_window_minutes: config.recovery_window_minutes.unwrap_or(30) as i64,
        };
        match enroll_validated_segment(&EnrollmentStore::production(pool.clone()), &request).await?
        {
            EnrollmentOutcome::Enrolled(enrollment) if enrollment.duplicate => {
                result.skipped_known += 1;
            }
            EnrollmentOutcome::Enrolled(enrollment) => {
                info!(
                    file = %path.display(), session = enrollment.upload_session_id,
                    segment_order = enrollment.segment_order,
                    "本地补扫：有效遗留分段已登记到缺失补传"
                );
                result.queued += 1;
            }
            EnrollmentOutcome::FinalizedRejected { .. } => result.skipped_finalized = true,
            EnrollmentOutcome::SourceMissing => {}
            EnrollmentOutcome::Outboxed(_) => {}
        }
    }
    Ok(result)
}

async fn ensure_missing_segment_session(
    pool: &ConnectionPool,
    row: &mut UploadMissingSegment,
) -> AppResult<()> {
    if row.upload_session_id.is_some() || row.aid.is_some() {
        return Ok(());
    }
    let existing_session_id = sqlx::query_scalar::<_, i64>(
        "SELECT id FROM upload_session \
         WHERE live_streamer_id = ?1 AND streamer_info_id = ?2 AND status != 'finalized' \
         ORDER BY updated_at DESC, id DESC LIMIT 1",
    )
    .bind(row.live_streamer_id)
    .bind(row.streamer_info_id)
    .fetch_optional(pool)
    .await
    .change_context(AppError::Unknown)?;
    let session_id = match existing_session_id {
        Some(session_id) => session_id,
        None => {
            insert_uploading_session(pool, row.live_streamer_id, row.streamer_info_id, &[])
                .await?
                .id
        }
    };
    row.upload_session_id = Some(session_id);
    // The caller atomically claimed this row in SQL before loading the destination session. Keep
    // that claim when writing the new session id; otherwise a stale in-memory `failed` status
    // would reopen the row to a concurrent manual recovery.
    row.status = "uploading".to_string();
    row.updated_at = chrono::Utc::now();
    *row = row
        .clone()
        .update_all_fields(pool)
        .await
        .change_context(AppError::Unknown)?;
    info!(
        missing_id = row.id,
        session = session_id,
        "manual_recover_bind_session：为未绑定缺失分段创建本地投稿会话"
    );
    Ok(())
}

pub async fn manual_recover_missing_segment(
    config: &Config,
    pool: &ConnectionPool,
    missing_id: i64,
) -> AppResult<RecoveryEligibility> {
    let mut row = UploadMissingSegment::select()
        .where_("id = ?")
        .bind(missing_id)
        .fetch_one(pool)
        .await
        .change_context(AppError::Unknown)?;

    let claim_now = chrono::Utc::now();
    // A source_missing row is only made claimable by an explicit recheck after its exact path
    // has reappeared. Silent recovery never performs this transition.
    if row.status == "source_missing" && Path::new(&row.file_path).is_file() {
        sqlx::query(
            "UPDATE upload_missing_segment SET status = 'failed', next_retry_at = ?1, \
             last_error = 'source file reappeared; manual recovery requested', updated_at = ?1 \
             WHERE id = ?2 AND status = 'source_missing'",
        )
        .bind(claim_now)
        .bind(missing_id)
        .execute(pool)
        .await
        .change_context(AppError::Unknown)?;
        row.status = "failed".to_string();
        row.next_retry_at = claim_now;
    }
    if row.lifecycle_version == 2 && matches!(row.status.as_str(), "pending" | "failed") {
        sqlx::query(
            "UPDATE upload_missing_segment SET next_retry_at = ?1, updated_at = ?1 \
             WHERE id = ?2 AND status IN ('pending', 'failed') AND attempt_token IS NULL",
        )
        .bind(claim_now)
        .bind(missing_id)
        .execute(pool)
        .await
        .change_context(AppError::Unknown)?;
        row.next_retry_at = claim_now;
    }
    let eligibility = check_recovery_eligibility(pool, &row, None, claim_now).await?;
    match eligibility {
        RecoveryEligibility::Eligible | RecoveryEligibility::LegacyFinalizedEdit => {}
        RecoveryEligibility::SourceMissing => {
            mark_source_missing(
                pool,
                row.id,
                "source file is no longer a regular file during manual recovery",
                claim_now,
            )
            .await?;
            return Ok(RecoveryEligibility::SourceMissing);
        }
        decision => return Ok(decision),
    }
    let v2_enrollment = if row.lifecycle_version == 2 {
        let Some(session_id) = row.upload_session_id else {
            return Ok(RecoveryEligibility::Conflict);
        };
        Some(SegmentEnrollment {
            missing_id: row.id,
            upload_session_id: session_id,
            segment_order: row.segment_order,
            normalized_file_path: PathBuf::from(
                row.normalized_file_path
                    .clone()
                    .unwrap_or_else(|| row.file_path.clone()),
            ),
            total_bytes: row
                .total_bytes
                .and_then(|value| u64::try_from(value).ok())
                .unwrap_or(0),
            duplicate: false,
        })
    } else {
        None
    };
    let selected_recovery = if v2_enrollment.is_some() {
        Some(
            select_recovery_line(
                pool,
                &StatelessClient::default().client,
                row.line_index,
                config.cookie_health_webhook.as_deref(),
            )
            .await?,
        )
    } else {
        None
    };
    let attempt_token = if let (Some(enrollment), Some(selected)) =
        (&v2_enrollment, &selected_recovery)
    {
        let token =
            match claim_enrolled_attempt(pool, enrollment, &selected.key, selected.recovery_index)
                .await?
            {
                AttemptClaim::Claimed(token) => token,
                AttemptClaim::AlreadyRunning => {
                    return Ok(RecoveryEligibility::AlreadyRunning);
                }
                AttemptClaim::AlreadyCompleted => {
                    return Ok(RecoveryEligibility::AlreadySucceeded);
                }
                AttemptClaim::NotDue => {
                    return Ok(RecoveryEligibility::Conflict);
                }
                AttemptClaim::NotClaimable(status) => {
                    warn!(
                        missing_id,
                        status, "manual recovery claim rejected after eligibility check"
                    );
                    return Ok(RecoveryEligibility::Conflict);
                }
            };
        Some(token)
    } else {
        let claimed = sqlx::query(
            "UPDATE upload_missing_segment SET status = 'uploading', updated_at = ?1 \
             WHERE id = ?2 AND status IN ('pending', 'failed')",
        )
        .bind(claim_now)
        .bind(missing_id)
        .execute(pool)
        .await
        .change_context(AppError::Unknown)?;
        if claimed.rows_affected() == 0 {
            return Ok(RecoveryEligibility::AlreadyRunning);
        }
        None
    };

    // Perform the upload + edit/insert inside a fallible scope so that any failure
    // resets the row to 'failed' and persists the error before returning.
    let span = tracing::info_span!("session", session = row.upload_session_id);
    let upload_result: AppResult<RecoveryEligibility> = async {
        // A database error while the live uploader was creating its local session may have left
        // this row unbound. Materialize a recoverable session before uploading so the manual
        // action has a durable destination instead of failing forever with neither session nor aid.
        ensure_missing_segment_session(pool, &mut row).await?;

        let _streamer_info = get_streamer_info(pool, row.streamer_info_id).await?;
        let upload_config = UploadStreamer::select()
            .where_("id = (SELECT upload_streamers_id FROM livestreamers WHERE id = ?)")
            .bind(row.live_streamer_id)
            .fetch_one(pool)
            .await
            .change_context(AppError::Unknown)?;
        // 载入主播配置，取其 postprocessor 用于补传成功后清理本地文件（对齐自动补传路径）。
        let live_streamer = LiveStreamer::select()
            .where_("id = ?")
            .bind(row.live_streamer_id)
            .fetch_one(pool)
            .await
            .change_context(AppError::Unknown)?;

        let mut effective_config = config.clone();
        if let Some(override_config) = live_streamer.override_cfg.clone() {
            effective_config.apply(override_config);
        }
        let mut upload_context = initialize_upload_context(
            &effective_config,
            &StatelessClient::default(),
            &upload_config,
            pool,
        )
        .await?;
        if let Some(selected) = &selected_recovery {
            upload_context.line = selected.line.clone();
            upload_context.line_key = selected.key.clone();
        }
        let path = PathBuf::from(&row.file_path);
        let repair_enabled = effective_config.timestamp_repair.unwrap_or(true);
        let (video, outcome, _normalization_artifact, _attempt_guard) =
            if let (Some(enrollment), Some(token)) = (&v2_enrollment, &attempt_token) {
                upload_enrolled_with_watchdog(
                    &path,
                    &upload_context,
                    repair_enabled,
                    effective_config.audio_normalization_enabled,
                    effective_config.effective_audio_target_lufs(),
                    pool,
                    enrollment.missing_id,
                    token,
                )
                .await?
            } else {
                upload_single_file_with_repair(
                    &path,
                    &upload_context,
                    repair_enabled,
                    effective_config.audio_normalization_enabled,
                    effective_config.effective_audio_target_lufs(),
                    None,
                )
                .await
                .map(|(video, outcome, artifact)| (video, outcome, artifact, None))?
            };
        match &outcome {
            RepairOutcome::Repaired(fixed) => {
                let _ = tokio::fs::remove_file(fixed).await;
            }
            RepairOutcome::Unfixable => {
                notify_alert(
                    effective_config.cookie_health_webhook.as_deref(),
                    "biliup 时间戳修复失败",
                    &format!(
                        "手动补投分段 {} 时间戳异常且无法自动修复，本地文件已保留，请手动处理。",
                        path.display()
                    ),
                );
            }
            RepairOutcome::Clean => {}
        }

        if let Some(session_id) = row.upload_session_id {
            let session = crate::server::infrastructure::models::UploadSession::select()
                .where_("id = ?")
                .bind(session_id)
                .fetch_one(pool)
                .await
                .change_context(AppError::Unknown)?;
            if let Some(aid) = session.aid.or(row.aid) {
                let bilibili = &upload_context.bilibili;
                let mut studio = bilibili
                    .studio_data(&Vid::Aid(aid as u64), None)
                    .await
                    .change_context(AppError::Unknown)?;
                if studio_already_contains_video(&studio, &video) {
                    info!(
                        aid,
                        "manual_recover_edit_archive：远端已存在相同视频，跳过重复编辑"
                    );
                } else {
                    patch_studio_videos(&mut studio, video.clone(), row.segment_order);
                    bilibili
                        .edit_by_app(&studio, None)
                        .await
                        .change_context(AppError::Unknown)?;
                    info!(
                        aid,
                        segment_order = row.segment_order,
                        "manual_recover_edit_archive：手动补传已追加到稿件"
                    );
                }
            } else {
                if v2_enrollment.is_none() {
                    insert_session_video_at_order(
                        pool,
                        session_id,
                        video.clone(),
                        row.segment_order,
                    )
                    .await?;
                }
                info!(
                    session = session_id,
                    segment_order = row.segment_order,
                    "manual_recover_to_session：手动补传已补进待提交会话，待下播投稿"
                );
            }
        } else if let Some(aid) = row.aid {
            let bilibili = &upload_context.bilibili;
            let mut studio = bilibili
                .studio_data(&Vid::Aid(aid as u64), None)
                .await
                .change_context(AppError::Unknown)?;
            if studio_already_contains_video(&studio, &video) {
                info!(
                    aid,
                    "manual_recover_edit_archive：远端已存在相同视频，跳过重复编辑"
                );
            } else {
                patch_studio_videos(&mut studio, video.clone(), row.segment_order);
                bilibili
                    .edit_by_app(&studio, None)
                    .await
                    .change_context(AppError::Unknown)?;
                info!(
                    aid,
                    segment_order = row.segment_order,
                    "manual_recover_edit_archive：手动补传已追加到稿件"
                );
            }
        } else {
            return Err(error_stack::Report::new(AppError::Custom(
                "missing segment has neither upload_session_id nor aid".to_string(),
            )));
        }

        if let (Some(enrollment), Some(token)) = (&v2_enrollment, &attempt_token) {
            let session = UploadSession::select()
                .where_("id = ?")
                .bind(enrollment.upload_session_id)
                .fetch_one(pool)
                .await
                .change_context(AppError::Unknown)?;
            let mut archive = LiveArchive {
                session_row_id: Some(session.id),
                aid: session.aid.map(|aid| aid as u64),
                bvid: session.bvid,
                videos: parse_videos(&session.videos_json),
            };
            persist_segment(pool, &mut archive, video, enrollment, token).await?;
        }

        // 补传成功并入稿/入会话后，按主播 postprocessor 清理本地文件，对齐自动补传路径
        // recover_due_missing_segments。Unfixable（时间戳无法修复）保留本地文件，留待手动处理。
        if !matches!(outcome, RepairOutcome::Unfixable)
            && let Some(processor) = &live_streamer.postprocessor
            && let Err(e) = process_video(&[path.as_path()], processor).await
        {
            error!(
                row_id = row.id,
                "postprocessor failed after manual missing segment recovery: {:?}", e
            );
        }

        Ok(eligibility)
    }
    .instrument(span)
    .await;

    match upload_result {
        Ok(decision) => {
            if v2_enrollment.is_none() {
                mark_retry_success(&mut row, chrono::Utc::now());
                row = row
                    .update_all_fields(pool)
                    .await
                    .change_context(AppError::Unknown)?;
                let _ = row;
            }
            Ok(decision)
        }
        Err(e) => {
            if let Some(token) = &attempt_token {
                fail_enrolled_attempt(pool, row.id, token, format!("{e:?}"), chrono::Utc::now())
                    .await?;
            } else {
                mark_retry_failure(&mut row, format!("{e:?}"), chrono::Utc::now());
                row.update_all_fields(pool)
                    .await
                    .change_context(AppError::Unknown)?;
            }
            Err(e)
        }
    }
}

pub async fn retry_missing_segment(
    config: &Config,
    pool: &ConnectionPool,
    missing_id: i64,
) -> AppResult<RecoveryEligibility> {
    let row = UploadMissingSegment::select()
        .where_("id = ?")
        .bind(missing_id)
        .fetch_one(pool)
        .await
        .change_context(AppError::Unknown)?;

    if row.status == "succeeded" {
        return Ok(RecoveryEligibility::AlreadySucceeded);
    }

    if row.status == "uploading" {
        let now = chrono::Utc::now();
        let token = row.attempt_token.as_deref().ok_or_else(|| {
            error_stack::Report::new(AppError::Custom(
                "uploading missing segment has no attempt token".to_string(),
            ))
        })?;
        let cancellation = cancel_registered_attempt(missing_id, token).await;
        if matches!(cancellation, CancelAttemptResult::TimedOut) {
            return Err(error_stack::Report::new(AppError::Custom(
                "previous upload attempt did not exit within cancellation wait limit".to_string(),
            )));
        }
        let _ = fail_enrolled_attempt(
            pool,
            missing_id,
            token,
            if matches!(cancellation, CancelAttemptResult::Exited) {
                "manual retry cancelled previous attempt".to_string()
            } else {
                "manual retry revoked stale attempt from a previous process".to_string()
            },
            now,
        )
        .await?;
        sqlx::query(
            "UPDATE upload_missing_segment SET next_retry_at = ?1, updated_at = ?1 \
             WHERE id = ?2 AND status = 'failed' AND attempt_token IS NULL",
        )
        .bind(now)
        .bind(missing_id)
        .execute(pool)
        .await
        .change_context(AppError::Unknown)?;
    }

    manual_recover_missing_segment(config, pool, missing_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::common::segment_enrollment::{
        EnrollmentOutcome, EnrollmentRequest, EnrollmentStore, enroll_validated_segment,
        normalize_segment_path,
    };
    use crate::server::infrastructure::connection_pool::ConnectionManager;
    use chrono::TimeZone;

    async fn deferred_test_pool() -> (tempfile::TempDir, ConnectionPool) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let pool = ConnectionManager::new_pool(db_path.to_str().unwrap())
            .await
            .unwrap();
        let now = chrono::Utc.with_ymd_and_hms(2026, 8, 23, 12, 0, 0).unwrap();
        sqlx::query(
            "INSERT INTO streamerinfo (id, name, url, title, date, live_cover_path) \
             VALUES (20, 'test', 'https://example.com/live', 'test stream', ?1, '')",
        )
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO upload_session \
             (id, live_streamer_id, streamer_info_id, aid, bvid, videos_json, status, created_at, updated_at) \
             VALUES (30, 10, 20, NULL, NULL, '[]', 'uploading', ?1, ?1)",
        )
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        (dir, pool)
    }

    #[test]
    fn segment_paths_keeps_video_only_without_danmaku() {
        let video = PathBuf::from("segment.ts");
        let event = SegmentInfo::new(video.clone(), None, None, 0);

        assert_eq!(segment_paths(&event), vec![video]);
    }

    #[test]
    fn segment_paths_keeps_video_then_danmaku_when_present() {
        let video = PathBuf::from("segment.ts");
        let danmaku = PathBuf::from("segment.xml");
        let event = SegmentInfo::new(video.clone(), Some(danmaku.clone()), None, 0);

        assert_eq!(segment_paths(&event), vec![video, danmaku]);
    }

    async fn v2_enrollment(
        pool: &ConnectionPool,
        directory: &Path,
        name: &str,
    ) -> SegmentEnrollment {
        let path = directory.join(name);
        std::fs::write(&path, b"synthetic upload bytes").unwrap();
        let request = EnrollmentRequest {
            live_streamer_id: 10,
            streamer_info_id: 20,
            file_path: path.clone(),
            normalized_file_path: normalize_segment_path(&path).unwrap(),
            danmaku_file_path: None,
            total_bytes: std::fs::metadata(&path).unwrap().len(),
            now: chrono::Utc.with_ymd_and_hms(2026, 8, 23, 12, 5, 0).unwrap(),
            recovery_window_minutes: 30,
        };
        let store = EnrollmentStore::new(pool.clone(), directory.join("outbox"));
        let EnrollmentOutcome::Enrolled(enrollment) =
            enroll_validated_segment(&store, &request).await.unwrap()
        else {
            panic!("healthy test database must enroll directly");
        };
        enrollment
    }

    fn uploaded_video(name: &str) -> Video {
        Video {
            title: Some(name.to_string()),
            filename: name.to_string(),
            desc: String::new(),
        }
    }

    #[tokio::test]
    async fn watchdog_waits_until_the_full_no_progress_deadline() {
        assert_eq!(NO_PROGRESS_TIMEOUT, Duration::from_secs(5 * 60));
        let upload = std::future::pending::<()>();
        pin!(upload);
        let cancellation = CancellationToken::new();
        let no_progress = tokio::time::sleep(Duration::from_millis(80));
        let total = tokio::time::sleep(Duration::from_secs(1));
        pin!(no_progress);
        pin!(total);
        let (_progress_tx, mut progress_rx) = mpsc::unbounded_channel();

        assert!(
            tokio::time::timeout(
                Duration::from_millis(60),
                next_attempt_event(
                    upload.as_mut(),
                    &cancellation,
                    no_progress.as_mut(),
                    total.as_mut(),
                    &mut progress_rx,
                    true,
                ),
            )
            .await
            .is_err(),
            "an attempt must still be alive immediately before its deadline"
        );
        assert!(matches!(
            next_attempt_event(
                upload.as_mut(),
                &cancellation,
                no_progress.as_mut(),
                total.as_mut(),
                &mut progress_rx,
                true,
            )
            .await,
            AttemptEvent::NoProgressTimeout
        ));
    }

    #[tokio::test]
    async fn progress_extends_idle_deadline_but_not_total_deadline() {
        assert_eq!(TOTAL_UPLOAD_TIMEOUT, Duration::from_secs(2 * 60 * 60));
        let upload = std::future::pending::<()>();
        pin!(upload);
        let cancellation = CancellationToken::new();
        let no_progress_duration = Duration::from_millis(100);
        let no_progress = tokio::time::sleep(no_progress_duration);
        let total = tokio::time::sleep(Duration::from_millis(260));
        pin!(no_progress);
        pin!(total);
        let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
        let sender = tokio::spawn(async move {
            for chunk_index in 0..20 {
                tokio::time::sleep(Duration::from_millis(20)).await;
                if progress_tx
                    .send(UploadProgress {
                        chunk_bytes: 1024,
                        uploaded_bytes: (chunk_index + 1) * 1024,
                        total_bytes: 100 * 1024,
                        chunk_index: chunk_index as usize,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        loop {
            match next_attempt_event(
                upload.as_mut(),
                &cancellation,
                no_progress.as_mut(),
                total.as_mut(),
                &mut progress_rx,
                true,
            )
            .await
            {
                AttemptEvent::Progress(_) => no_progress
                    .as_mut()
                    .reset(tokio::time::Instant::now() + no_progress_duration),
                AttemptEvent::TotalUploadTimeout => break,
                AttemptEvent::NoProgressTimeout => panic!("regular progress must extend idle time"),
                _ => panic!("unexpected watchdog event"),
            }
        }
        sender.abort();
    }

    #[tokio::test]
    async fn cancellation_drops_upload_future_and_releases_permit() {
        let semaphore = Arc::new(Semaphore::new(1));
        let acquired = Arc::new(Notify::new());
        let acquired_wait = acquired.notified();
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let task_semaphore = semaphore.clone();
        let task_acquired = acquired.clone();
        let task = tokio::spawn(async move {
            let upload = async move {
                let _permit = task_semaphore.acquire_owned().await.unwrap();
                task_acquired.notify_one();
                std::future::pending::<()>().await;
            };
            pin!(upload);
            let no_progress = tokio::time::sleep(Duration::from_secs(1));
            let total = tokio::time::sleep(Duration::from_secs(1));
            pin!(no_progress);
            pin!(total);
            let (_progress_tx, mut progress_rx) = mpsc::unbounded_channel();
            next_attempt_event(
                upload.as_mut(),
                &task_cancellation,
                no_progress.as_mut(),
                total.as_mut(),
                &mut progress_rx,
                true,
            )
            .await
        });

        acquired_wait.await;
        cancellation.cancel();
        assert!(matches!(task.await.unwrap(), AttemptEvent::Cancelled));
        assert!(semaphore.try_acquire_owned().is_ok());
    }

    #[tokio::test]
    async fn v2_upload_success_commits_video_lifecycle_and_session_atomically() {
        let (directory, pool) = deferred_test_pool().await;
        let enrollment = v2_enrollment(&pool, directory.path(), "atomic-success.flv").await;
        let token = claim_enrolled_attempt(&pool, &enrollment, "test", None)
            .await
            .unwrap()
            .unwrap();
        let mut archive = LiveArchive {
            session_row_id: Some(enrollment.upload_session_id),
            aid: None,
            bvid: None,
            videos: vec![],
        };
        let uploaded = uploaded_video("remote-atomic");

        persist_segment(&pool, &mut archive, uploaded.clone(), &enrollment, &token)
            .await
            .unwrap();

        let lifecycle = sqlx::query_as::<_, (String, Option<String>, i64, Option<String>)>(
            "SELECT status, video_json, uploaded_bytes, attempt_token \
             FROM upload_missing_segment WHERE id = ?",
        )
        .bind(enrollment.missing_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(lifecycle.0, "succeeded");
        assert_eq!(
            serde_json::from_str::<Video>(&lifecycle.1.unwrap())
                .unwrap()
                .filename,
            uploaded.filename
        );
        assert_eq!(lifecycle.2, enrollment.total_bytes as i64);
        assert_eq!(lifecycle.3, None);
        let session_json: String =
            sqlx::query_scalar("SELECT videos_json FROM upload_session WHERE id = ?")
                .bind(enrollment.upload_session_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let session_videos = serde_json::from_str::<Vec<Video>>(&session_json).unwrap();
        assert_eq!(session_videos.len(), 1);
        assert_eq!(session_videos[0].filename, uploaded.filename);
    }

    #[tokio::test]
    async fn concurrent_v2_claims_issue_exactly_one_uuid_lease() {
        let (directory, pool) = deferred_test_pool().await;
        let enrollment = v2_enrollment(&pool, directory.path(), "concurrent-claim.flv").await;
        let (left, right) = tokio::join!(
            claim_enrolled_attempt(&pool, &enrollment, "bda2", None),
            claim_enrolled_attempt(&pool, &enrollment, "tx", None),
        );
        let claims = [left.unwrap(), right.unwrap()];
        let tokens = claims
            .iter()
            .filter_map(|claim| match claim {
                AttemptClaim::Claimed(token) => Some(token),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(tokens.len(), 1);
        assert!(uuid::Uuid::parse_str(tokens[0]).is_ok());
        assert_eq!(
            claims
                .iter()
                .filter(|claim| matches!(claim, AttemptClaim::AlreadyRunning))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn configured_bldsa_cooldown_is_overridden_by_bda2() {
        let (_directory, pool) = deferred_test_pool().await;
        upload_line_health::record_failure(
            &pool,
            "bldsa",
            UploadFailureKind::CertificateExpired,
            "certificate expired",
            chrono::Utc::now(),
        )
        .await
        .unwrap();

        let selected = select_configured_line(&pool, &reqwest::Client::new(), "bldsa", None)
            .await
            .unwrap();
        assert_eq!(selected.key, "bda2");
        assert_eq!(selected.line.key(), "bda2");
    }

    #[tokio::test]
    async fn recovery_skips_cooling_bda2_and_selects_tx_without_bldsa() {
        let (_directory, pool) = deferred_test_pool().await;
        upload_line_health::record_failure(
            &pool,
            "bda2",
            UploadFailureKind::Transport,
            "connection reset",
            chrono::Utc::now(),
        )
        .await
        .unwrap();

        let selected = select_recovery_line(&pool, &reqwest::Client::new(), 0, None)
            .await
            .unwrap();
        assert_eq!(selected.key, "tx");
        assert_eq!(selected.recovery_index, Some(1));
        assert_ne!(selected.line.key(), "bldsa");
    }

    #[tokio::test]
    async fn revoked_attempt_cannot_publish_delayed_success_over_new_lease() {
        let (directory, pool) = deferred_test_pool().await;
        let enrollment = v2_enrollment(&pool, directory.path(), "delayed-success.flv").await;
        let old_token = claim_enrolled_attempt(&pool, &enrollment, "bda2", None)
            .await
            .unwrap()
            .unwrap();
        assert!(
            fail_enrolled_attempt(
                &pool,
                enrollment.missing_id,
                &old_token,
                "cancelled".to_string(),
                chrono::Utc::now(),
            )
            .await
            .unwrap()
        );
        sqlx::query("UPDATE upload_missing_segment SET next_retry_at = ? WHERE id = ?")
            .bind(chrono::Utc::now())
            .bind(enrollment.missing_id)
            .execute(&pool)
            .await
            .unwrap();
        let new_token = claim_enrolled_attempt(&pool, &enrollment, "tx", None)
            .await
            .unwrap()
            .unwrap();
        let mut archive = LiveArchive::default();

        assert!(
            persist_segment(
                &pool,
                &mut archive,
                uploaded_video("late-old"),
                &enrollment,
                &old_token,
            )
            .await
            .is_err()
        );
        persist_segment(
            &pool,
            &mut archive,
            uploaded_video("current-new"),
            &enrollment,
            &new_token,
        )
        .await
        .unwrap();
        assert_eq!(archive.videos.len(), 1);
        assert_eq!(archive.videos[0].filename, "current-new");
    }

    #[tokio::test]
    async fn v2_session_write_failure_rolls_back_lifecycle_success() {
        let (directory, pool) = deferred_test_pool().await;
        let enrollment = v2_enrollment(&pool, directory.path(), "atomic-rollback.flv").await;
        let token = claim_enrolled_attempt(&pool, &enrollment, "test", None)
            .await
            .unwrap()
            .unwrap();
        sqlx::query("UPDATE upload_session SET status = 'finalized' WHERE id = ?")
            .bind(enrollment.upload_session_id)
            .execute(&pool)
            .await
            .unwrap();
        let mut archive = LiveArchive::default();

        assert!(
            persist_segment(
                &pool,
                &mut archive,
                uploaded_video("must-rollback"),
                &enrollment,
                &token,
            )
            .await
            .is_err()
        );

        let lifecycle = sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
            "SELECT status, video_json, attempt_token FROM upload_missing_segment WHERE id = ?",
        )
        .bind(enrollment.missing_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(lifecycle.0, "uploading");
        assert_eq!(lifecycle.1, None);
        assert_eq!(lifecycle.2.as_deref(), Some(token.as_str()));
    }

    #[tokio::test]
    async fn v2_out_of_order_success_rebuilds_session_in_enrollment_order() {
        let (directory, pool) = deferred_test_pool().await;
        let first = v2_enrollment(&pool, directory.path(), "order-0.flv").await;
        let second = v2_enrollment(&pool, directory.path(), "order-1.flv").await;
        let first_token = claim_enrolled_attempt(&pool, &first, "test", None)
            .await
            .unwrap()
            .unwrap();
        let second_token = claim_enrolled_attempt(&pool, &second, "test", None)
            .await
            .unwrap()
            .unwrap();
        let mut archive = LiveArchive::default();

        persist_segment(
            &pool,
            &mut archive,
            uploaded_video("remote-order-1"),
            &second,
            &second_token,
        )
        .await
        .unwrap();
        assert_eq!(archive.videos.len(), 1);
        assert_eq!(archive.videos[0].filename, "remote-order-1");

        persist_segment(
            &pool,
            &mut archive,
            uploaded_video("remote-order-0"),
            &first,
            &first_token,
        )
        .await
        .unwrap();
        assert_eq!(
            archive
                .videos
                .iter()
                .map(|video| video.filename.as_str())
                .collect::<Vec<_>>(),
            ["remote-order-0", "remote-order-1"]
        );
    }

    #[test]
    fn rescan_filename_candidate_accepts_session_prefix_or_streamer_name() {
        assert!(is_rescan_filename_candidate(
            "帝骑哥 2026-08-25 22:29:32.flv",
            "帝骑哥 ",
            "帝骑哥"
        ));
        assert!(is_rescan_filename_candidate(
            "record-帝骑哥-2026-08-25.flv",
            "custom-prefix-",
            "帝骑哥"
        ));
        assert!(!is_rescan_filename_candidate(
            "懒懒椰椰 2026-08-25 22:29:32.flv",
            "帝骑哥 ",
            "帝骑哥"
        ));
    }

    #[test]
    fn resolve_recorded_path_keeps_absolute_and_anchors_relative_paths() {
        let root = Path::new("/opt");
        assert_eq!(
            resolve_recorded_path(root, "帝骑哥 2026-08-25.flv"),
            PathBuf::from("/opt/帝骑哥 2026-08-25.flv")
        );
        assert_eq!(
            resolve_recorded_path(root, "/archive/帝骑哥.flv"),
            PathBuf::from("/archive/帝骑哥.flv")
        );
    }

    #[tokio::test]
    async fn init_failure_drain_indexes_and_queues_every_segment() {
        let (_dir, pool) = deferred_test_pool().await;
        let (tx, rx) = async_channel::bounded(4);
        tx.send(SegmentInfo::new(
            PathBuf::from("/opt/segment-1.flv"),
            Some(PathBuf::from("/opt/segment-1.xml")),
            None,
            0,
        ))
        .await
        .unwrap();
        tx.send(SegmentInfo::new(
            PathBuf::from("/opt/segment-2.flv"),
            None,
            None,
            1,
        ))
        .await
        .unwrap();

        let summary = defer_segments_after_upload_init_failure(
            rx,
            &pool,
            10,
            20,
            Some(30),
            2,
            "login unavailable",
            &[],
            &AudioSampleStore::for_working_directory(_dir.path()),
        )
        .await;

        assert!(tx.is_closed());
        assert!(
            tx.send(SegmentInfo::new(
                PathBuf::from("/opt/segment-3.flv"),
                None,
                None,
                2,
            ))
            .await
            .is_err(),
            "failed pipeline must close so the producer rebuilds it on the next segment"
        );
        assert_eq!(
            summary,
            DeferredSegmentSummary {
                received: 2,
                queued: 2,
                queue_failures: 0,
            }
        );
        let indexed = sqlx::query_as::<_, (String, i64)>(
            "SELECT file, streamer_info_id FROM filelist ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            indexed,
            vec![
                ("/opt/segment-1.flv".to_string(), 20),
                ("/opt/segment-2.flv".to_string(), 20),
            ]
        );
        let queued = sqlx::query_as::<_, (String, Option<String>, i64, String, i64)>(
            "SELECT file_path, danmaku_file_path, segment_order, status, attempts \
             FROM upload_missing_segment ORDER BY segment_order",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            queued,
            vec![
                (
                    "/opt/segment-1.flv".to_string(),
                    Some("/opt/segment-1.xml".to_string()),
                    2,
                    "pending".to_string(),
                    0,
                ),
                (
                    "/opt/segment-2.flv".to_string(),
                    None,
                    3,
                    "pending".to_string(),
                    0,
                ),
            ]
        );
    }

    #[tokio::test]
    async fn manual_recovery_materializes_session_for_unbound_segment() {
        let (_dir, pool) = deferred_test_pool().await;
        let now = chrono::Utc.with_ymd_and_hms(2026, 8, 23, 12, 5, 0).unwrap();
        enqueue_pending_segment(
            &pool,
            10,
            20,
            None,
            None,
            Path::new("/opt/unbound.flv"),
            None,
            0,
            "session creation failed".to_string(),
            now,
        )
        .await
        .unwrap();
        let mut row = UploadMissingSegment::select()
            .where_("file_path = ?")
            .bind("/opt/unbound.flv")
            .fetch_one(&pool)
            .await
            .unwrap();

        ensure_missing_segment_session(&pool, &mut row)
            .await
            .unwrap();

        let session_id = row
            .upload_session_id
            .expect("manual recovery should bind a local session");
        let stored_session_id = sqlx::query_scalar::<_, Option<i64>>(
            "SELECT upload_session_id FROM upload_missing_segment WHERE id = ?",
        )
        .bind(row.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(stored_session_id, Some(session_id));
        assert_eq!(session_id, 30, "reuse the matching active local session");
        assert_eq!(row.status, "uploading");
    }

    #[tokio::test]
    async fn local_rescan_reuses_current_session_and_rejects_thirteen_byte_flv() {
        let (dir, pool) = deferred_test_pool().await;
        sqlx::query(
            "INSERT INTO livestreamers (id, url, remark) \
             VALUES (10, 'https://example.com/live', 'test')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let empty_chunk = dir.path().join("test 2026-08-23 12:30:00.flv");
        std::fs::write(&empty_chunk, b"thirteen-byte").unwrap();

        let result = rescan_local_valid_segments(&Config::default(), &pool, 20, dir.path())
            .await
            .unwrap();

        assert_eq!(result.upload_session_id, 30);
        assert_eq!(result.scanned, 1);
        assert_eq!(result.queued, 0);
        assert_eq!(result.skipped_invalid, 1);
        let missing_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM upload_missing_segment")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(missing_count, 0, "空片段不得出现在缺失补传");
    }

    #[tokio::test]
    async fn finalized_session_rescan_does_not_create_a_replacement_session() {
        let (dir, pool) = deferred_test_pool().await;
        sqlx::query("INSERT INTO livestreamers (id, url, remark) VALUES (10, 'https://example.com/live', 'test')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE upload_session SET status = 'finalized' WHERE id = 30")
            .execute(&pool)
            .await
            .unwrap();

        let result = rescan_local_valid_segments(&Config::default(), &pool, 20, dir.path())
            .await
            .unwrap();

        assert!(result.skipped_finalized);
        assert_eq!(result.upload_session_id, 30);
        let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM upload_session")
            .fetch_one(&pool)
            .await
            .unwrap();
        let missing: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM upload_missing_segment")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!((sessions, missing), (1, 0));
    }

    #[tokio::test]
    async fn vanished_v2_source_becomes_terminal_without_incrementing_attempts() {
        let (directory, pool) = deferred_test_pool().await;
        let enrollment = v2_enrollment(&pool, directory.path(), "vanished-source.flv").await;
        std::fs::remove_file(directory.path().join("vanished-source.flv")).unwrap();
        let row = UploadMissingSegment::select()
            .where_("id = ?")
            .bind(enrollment.missing_id)
            .fetch_one(&pool)
            .await
            .unwrap();

        assert_eq!(
            check_recovery_eligibility(&pool, &row, None, chrono::Utc::now())
                .await
                .unwrap(),
            RecoveryEligibility::SourceMissing
        );
        assert!(
            mark_source_missing(&pool, row.id, "test source removed", chrono::Utc::now())
                .await
                .unwrap()
        );
        assert!(
            !mark_source_missing(&pool, row.id, "must be idempotent", chrono::Utc::now())
                .await
                .unwrap()
        );
        let state = sqlx::query_as::<_, (String, i64, Option<String>)>(
            "SELECT status, attempts, attempt_token FROM upload_missing_segment WHERE id = ?",
        )
        .bind(row.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(state, ("source_missing".to_string(), 0, None));
    }

    #[tokio::test]
    async fn late_validated_segment_is_audited_without_reopening_finalized_session() {
        let (directory, pool) = deferred_test_pool().await;
        sqlx::query("UPDATE upload_session SET status = 'finalized' WHERE id = 30")
            .execute(&pool)
            .await
            .unwrap();
        let path = directory.path().join("late-after-finalize.flv");
        std::fs::write(&path, b"synthetic upload bytes").unwrap();
        let request = EnrollmentRequest {
            live_streamer_id: 10,
            streamer_info_id: 20,
            file_path: path.clone(),
            normalized_file_path: normalize_segment_path(&path).unwrap(),
            danmaku_file_path: None,
            total_bytes: std::fs::metadata(&path).unwrap().len(),
            now: chrono::Utc::now(),
            recovery_window_minutes: 30,
        };
        let outcome = enroll_validated_segment(
            &EnrollmentStore::new(pool.clone(), directory.path().join("outbox")),
            &request,
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome,
            EnrollmentOutcome::FinalizedRejected { session_id: 30 }
        ));
        let counts = sqlx::query_as::<_, (i64, i64, i64)>(
            "SELECT (SELECT COUNT(*) FROM upload_session), \
                    (SELECT COUNT(*) FROM upload_missing_segment), \
                    (SELECT COUNT(*) FROM upload_recovery_audit)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(counts, (1, 0, 1));
    }

    const LIVE_URL: &str = "https://live.douyin.com/123456";

    #[test]
    fn resolve_source_falls_back_when_none() {
        // 配置文件未提供 copyright_source
        assert_eq!(resolve_source(None, LIVE_URL), LIVE_URL);
    }

    #[test]
    fn resolve_source_falls_back_when_empty_string() {
        // 前端表单留空 -> Some("")，应回退到直播间地址（核心 bug 场景）
        assert_eq!(resolve_source(Some(""), LIVE_URL), LIVE_URL);
    }

    #[test]
    fn resolve_source_falls_back_when_whitespace_only() {
        // 仅空白同样视作未填写
        assert_eq!(resolve_source(Some("   "), LIVE_URL), LIVE_URL);
    }

    #[test]
    fn resolve_source_keeps_user_value_and_trims() {
        // 用户填写了真实来源则保留（并去除首尾空白）
        assert_eq!(
            resolve_source(Some("  https://b23.tv/abc  "), LIVE_URL),
            "https://b23.tv/abc"
        );
    }

    // 断言解析结果，失败时打印实际取到的变体便于定位
    fn assert_black(background: Background) {
        match background {
            Background::Black => {}
            other => panic!("应回退为纯黑背景，实际 {other:?}"),
        }
    }

    fn assert_image(background: Background, expected_file_name: &str) {
        match background {
            Background::Image(path) => {
                assert_eq!(path, Path::new(BACKGROUND_DIR).join(expected_file_name));
            }
            other => panic!("应解析为图片背景，实际 {other:?}"),
        }
    }

    #[test]
    fn resolve_background_falls_back_to_black_when_both_levels_empty() {
        // 两级都没配 —— 升级前的既有配置全都是这种状态，产出维持纯黑底
        assert_black(resolve_background(None, None));
    }

    #[test]
    fn resolve_background_uses_template_when_only_template_set() {
        // 存的是文件名，实际路径在运行时拼出来
        assert_image(resolve_background(None, Some("aurora.jpg")), "aurora.jpg");
    }

    #[test]
    fn resolve_background_uses_streamer_when_only_streamer_set() {
        assert_image(resolve_background(Some("nebula.jpg"), None), "nebula.jpg");
    }

    #[test]
    fn resolve_background_prefers_streamer_over_template() {
        // 主播级覆盖模板级——这是本级配置存在的全部意义
        assert_image(
            resolve_background(Some("nebula.jpg"), Some("aurora.jpg")),
            "nebula.jpg",
        );
    }

    #[test]
    fn resolve_background_trims_surrounding_whitespace() {
        assert_image(
            resolve_background(None, Some("  aurora.jpg  ")),
            "aurora.jpg",
        );
    }

    #[test]
    fn resolve_background_treats_blank_as_unset() {
        // 与 resolve_source 同一套空值语义：NULL（None）与空白字符串等价，都是「没填」
        assert_black(resolve_background(None, Some("")));
        assert_black(resolve_background(None, Some("   ")));
    }

    // 主播级填空白 = 没配，回退到模板。
    // 已知限制：因此主播这一级无法表达「我就是要纯黑，别用模板那张图」——
    // 见 spec 的 Further Notes，这是为与 resolve_source 语义一致而接受的代价。
    #[test]
    fn resolve_background_blank_streamer_falls_back_to_template() {
        assert_image(
            resolve_background(Some("   "), Some("aurora.jpg")),
            "aurora.jpg",
        );
        assert_image(
            resolve_background(Some(""), Some("aurora.jpg")),
            "aurora.jpg",
        );
    }

    // 库里存的必须是「一个文件名」。绝对路径尤其危险：Path::join 会把基路径整段丢掉，
    // 不拦的话库里一个值就能把渲染器指到背景图目录之外。
    #[test]
    fn resolve_background_rejects_anything_but_a_bare_file_name() {
        assert_black(resolve_background(None, Some("/etc/passwd")));
        assert_black(resolve_background(None, Some("../../etc/passwd")));
        assert_black(resolve_background(None, Some("sub/aurora.jpg")));
        assert_black(resolve_background(None, Some("..")));
        assert_black(resolve_background(None, Some(".")));
        assert_black(resolve_background(Some("/etc/passwd"), None));
    }

    // 不可用的值等同于没填，因此继续往下一级回退，而不是把整条链断在这里。
    // 主播那一级写错一个路径，不该把模板配好的背景也一起废掉。
    #[test]
    fn resolve_background_unusable_streamer_value_falls_back_to_template() {
        assert_image(
            resolve_background(Some("/etc/passwd"), Some("aurora.jpg")),
            "aurora.jpg",
        );
    }
}

/// 上传Actor
/// 负责处理上传相关的消息和任务
pub struct UActor {
    /// 上传消息接收器
    receiver: Receiver<UploaderMessage>,
}

impl UActor {
    /// 创建新的上传Actor实例
    pub fn new(receiver: Receiver<UploaderMessage>) -> Self {
        Self { receiver }
    }

    /// 运行Actor主循环，处理接收到的消息
    pub(crate) async fn run(&mut self) {
        while let Ok(msg) = self.receiver.recv().await {
            // SegmentEvent 的 receiver 会活到整场直播结束。若在 Actor 循环里直接 await，
            // 一个长直播会独占 Actor，使其他主播的已验证分段只停在内存队列，连本地
            // upload_session 都无法创建。每条直播管道独立运行；真正的 B 站网络上传仍由
            // GLOBAL_UPLOAD_SEMAPHORE 串行化，因此不会放大上传并发。
            tokio::spawn(async move {
                Self::handle_message(msg).await;
            });
        }
    }

    /// 处理上传消息
    ///
    /// # 参数
    /// * `msg` - 要处理的上传消息
    async fn handle_message(msg: UploaderMessage) {
        match msg {
            UploaderMessage::SegmentEvent(rx, ctx) => {
                ctx.change_status(Stage::Upload, WorkerStatus::Pending)
                    .await;
                let result = match ctx.upload_config() {
                    Some(config) => process_with_upload(rx, &ctx, config).await,
                    None => {
                        // 未绑定投稿模板：仅录制。消费分段事件，但【不执行会删文件的后处理】，
                        // 避免误配把没上传的录像静默删掉（footgun）。文件保留本地由用户处置。
                        pin!(rx);
                        let mut segments = 0u32;
                        let sample_store = AudioSampleStore::for_working_directory(
                            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                        );
                        while let Some(event) = rx.next().await {
                            segments += 1;
                            if let Err(error) =
                                index_recorded_segment(ctx.pool(), ctx.id(), &event).await
                            {
                                error!(
                                    ?error,
                                    file = %event.prev_file_path.display(),
                                    "仅录制模式写入 filelist 失败；本地文件保持不动"
                                );
                            }
                            // 仅录制模式同样会产出完整分段，样片截取不应依赖投稿模板。
                            maybe_capture_reference_sample(&event.prev_file_path, &sample_store)
                                .await;
                        }
                        warn!(
                            url = ctx.live_streamer().url,
                            segments,
                            "未绑定投稿模板，仅录制；已保留本地文件，跳过后处理（避免误删未上传录像）"
                        );
                        Ok(())
                    }
                };

                if let Err(e) = &result {
                    // 用 {:?} 打全 error_stack 错误链，便于定位底层原因（网络/cookie/转码…）
                    error!("Process segment event failed: {:?}", e);
                    // 可以添加错误通知机制
                }
                info!(url=ctx.live_streamer().url, result=?result, "后处理执行完毕：Finished processing segment event");
                ctx.change_status(Stage::Upload, WorkerStatus::Idle).await;
            }
            UploaderMessage::RecoveryBatchDeferred {
                ctx,
                batch_id,
                manifest_path,
            } => {
                if let Err(error) =
                    persist_recovery_batch_manifest(ctx.pool(), &ctx, &batch_id, &manifest_path)
                        .await
                {
                    error!(
                        recovery_batch_id = batch_id,
                        manifest = %manifest_path.display(),
                        ?error,
                        "failed to index deferred recovery manifest; manifest remains durable"
                    );
                }
            }
        }
    }
}

async fn persist_recovery_batch_manifest(
    pool: &ConnectionPool,
    ctx: &Context,
    batch_id: &str,
    manifest_path: &Path,
) -> AppResult<()> {
    let bytes = tokio::fs::read(manifest_path)
        .await
        .change_context(AppError::Unknown)?;
    let manifest: serde_json::Value =
        serde_json::from_slice(&bytes).change_context(AppError::Unknown)?;
    let files_json = serde_json::to_string(&manifest["files"]).change_context(AppError::Unknown)?;
    let last_error = manifest["last_error"].as_str();
    let next_retry_ms = manifest["next_retry_at_ms"]
        .as_u64()
        .and_then(|value| i64::try_from(value).ok())
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
    let next_retry_at = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(next_retry_ms)
        .unwrap_or_else(chrono::Utc::now);
    let now = chrono::Utc::now();
    sqlx::query(
        "INSERT INTO recoverable_short_batch \
         (recovery_batch_id, live_streamer_id, streamer_info_id, state, files_json, manifest_path, attempts, next_retry_at, last_error, created_at, updated_at) \
         VALUES (?1, ?2, ?3, 'Deferred', ?4, ?5, 0, ?6, ?7, ?8, ?8) \
         ON CONFLICT(recovery_batch_id) DO UPDATE SET state = excluded.state, files_json = excluded.files_json, \
         manifest_path = excluded.manifest_path, next_retry_at = excluded.next_retry_at, last_error = excluded.last_error, updated_at = excluded.updated_at",
    )
    .bind(batch_id)
    .bind(ctx.live_streamer().id)
    .bind(ctx.id())
    .bind(files_json)
    .bind(manifest_path.display().to_string())
    .bind(next_retry_at)
    .bind(last_error)
    .bind(now)
    .execute(pool)
    .await
    .change_context(AppError::Unknown)?;
    info!(
        recovery_batch_id = batch_id,
        manifest = %manifest_path.display(),
        "deferred recovery batch indexed in database"
    );
    Ok(())
}

/// 上传消息枚举
/// 定义上传Actor可以处理的消息类型
#[derive(Debug)]
pub enum UploaderMessage {
    /// 分段事件消息，包含事件、接收器和工作器
    SegmentEvent(Receiver<SegmentInfo>, Context),
    RecoveryBatchDeferred {
        ctx: Context,
        batch_id: String,
        manifest_path: PathBuf,
    },
}
