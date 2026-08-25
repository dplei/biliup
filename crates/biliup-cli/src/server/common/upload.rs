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
    due_missing_segments_for_session, enqueue_missing_segment, enqueue_pending_segment,
    mark_retry_failure, mark_retry_success, next_missing_segment_order, patch_studio_videos,
    reset_for_manual_retry, upload_line_for_recovery,
};
use crate::server::common::path_safety::single_segment_name;
use crate::server::common::timestamp_repair::{RepairOutcome, SystemFfmpeg, normalize_timestamps};
use crate::server::common::upload_rate_gate::{self, UploadRateGateSettings};
use crate::server::common::upload_session::{
    LiveArchive, active_sessions_for_room, get_streamer_info, insert_session_video_at_order,
    insert_uploading_session, mark_submit_anomaly, mark_submitted, parse_videos, reattach_session,
    select_recovery_candidate, select_stale_session_indices, submit_state_label,
    update_session_videos,
};
use crate::server::common::util::Recorder;
use crate::server::config::Config;
use crate::server::core::downloader::SegmentInfo;
use crate::server::errors::{AppError, AppResult};
use crate::server::infrastructure::connection_pool::ConnectionPool;
use crate::server::infrastructure::context::{Context, Stage, WorkerStatus};
use crate::server::infrastructure::models::hook_step::{
    HookStep, process_video, process_video_paths,
};
use crate::server::infrastructure::models::live_streamer::LiveStreamer;
use crate::server::infrastructure::models::upload_streamer::UploadStreamer;
use crate::server::infrastructure::models::{InsertFileItem, StreamerInfo, UploadMissingSegment};
use async_channel::Receiver;
use biliup::bilibili::Vid;
use biliup::bilibili::{BiliBili, ResponseData, Studio, Video};
use biliup::client::StatelessClient;
use biliup::credential::login_by_cookies;
use biliup::error::Kind;
use biliup::uploader::line::{Line, Probe};
use biliup::uploader::util::SubmitOption;
use biliup::uploader::{VideoFile, line};
use error_stack::ResultExt;
use futures::StreamExt;
use ormlite::Insert;
use ormlite::Model;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::Instant;
use struct_patch::Patch;
use tokio::pin;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
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
}

static GLOBAL_UPLOAD_SEMAPHORE: OnceLock<std::sync::Arc<Semaphore>> = OnceLock::new();

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
            let deferred_archive = prepare_deferred_archive(ctx).await;
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
            && !archive.videos.is_empty()
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
                &archive.videos,
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

    // 获取上传线路
    let line = get_upload_line(&client.client, &config.lines).await?;

    Ok(UploadContext {
        bilibili,
        line,
        threads: config.threads as usize,
        client: client.clone(),
        rate_gate: UploadRateGateSettings::from(config),
        pool: pool.clone(),
    })
}

async fn get_upload_line(client: &reqwest::Client, line: &str) -> AppResult<Line> {
    let line = match line {
        "bda2" => line::bda2(),
        "bda" => line::bda(),
        "tx" => line::tx(),
        "txa" => line::txa(),
        "bldsa" => line::bldsa(),
        "alia" => line::alia(),
        _ => Probe::probe(client)
            .await
            .change_context(AppError::Unknown)?,
    };
    Ok(line)
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
    InsertFileItem {
        file: event.prev_file_path.display().to_string(),
        streamer_info_id,
    }
    .insert(pool)
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

        match enqueue_pending_segment(
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
        {
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
    videos: &[Video],
) -> AppResult<()> {
    let bilibili = &upload_context.bilibili;
    let recorder = Recorder::new(upload_config.title.clone(), streamer_info.clone());
    let studio = build_studio(
        upload_config,
        streamer_background,
        bilibili,
        videos.to_vec(),
        &recorder,
    )
    .await?;
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
            if let Err(db) = mark_submit_anomaly(pool, session_row_id, state, msg).await {
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
            if let Err(e) = mark_submitted(pool, session_row_id, aid_val, bvid.clone()).await {
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
            if let Err(db) = mark_submit_anomaly(pool, session_row_id, state, msg).await {
                error!(?db, "写回 submit_state=ok_no_aid 失败");
            }
        }
    }
    Ok(())
}

/// 每段上传成功后：把 Video 累积进 archive 并落库（uploading 态），供下播一次性提交 & 重启恢复。
/// 首段创建会话行，之后更新 videos_json。落库失败则返回 Err（调用方据此保留本地文件不删）。
async fn persist_segment(ctx: &Context, archive: &mut LiveArchive, video: Video) -> AppResult<()> {
    archive.videos.push(video);
    let had_session = archive.session_row_id.is_some();
    let row_id = ensure_archive_session(ctx, archive).await?;
    if had_session {
        update_session_videos(ctx.pool(), row_id, &archive.videos).await
    } else {
        Ok(())
    }
}

/// Ensure both successful uploads and failed first segments have a durable local session.
async fn ensure_archive_session(ctx: &Context, archive: &mut LiveArchive) -> AppResult<i64> {
    if let Some(row_id) = archive.session_row_id {
        return Ok(row_id);
    }
    let row =
        insert_uploading_session(ctx.pool(), ctx.worker_id(), ctx.id(), &archive.videos).await?;
    archive.session_row_id = Some(row.id);
    Ok(row.id)
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
        if archive.videos.is_empty() {
            continue;
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
            &archive.videos,
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
        Ok(LiveArchive::default())
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
        let row = insert_uploading_session(ctx.pool(), room_id, ctx.id(), &[]).await?;
        info!(
            row_id = row.id,
            room_id, "上传初始化失败：已创建本地投稿会话用于登记待补传"
        );
        Ok(LiveArchive {
            session_row_id: Some(row.id),
            aid: None,
            bvid: None,
            videos: Vec::new(),
        })
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
    let mut next_order = if let Some(row_id) = archive.session_row_id {
        match next_missing_segment_order(ctx.pool(), row_id, archive.videos.len()).await {
            Ok(order) => order,
            Err(error) => {
                error!(?error, row_id, "读取上传管道分段顺序失败，从已上传段数继续");
                i64::try_from(archive.videos.len()).unwrap_or(i64::MAX)
            }
        }
    } else {
        i64::try_from(archive.videos.len()).unwrap_or(i64::MAX)
    };
    pin!(rx);
    while let Some(event) = rx.next().await {
        if let Err(error) = index_recorded_segment(ctx.pool(), ctx.id(), &event).await {
            error!(
                ?error,
                file = %event.prev_file_path.display(),
                "写入 filelist 失败；继续上传，缺失补传仍以持久队列为准"
            );
        }
        let recovery_source_paths = event.recovery_source_paths.clone();
        let mut paths = segment_paths(&event);
        if !segment_processors.is_empty()
            && let Err(e) = process_video_paths(&mut paths, segment_processors).await
        {
            error!(file = ?event.prev_file_path, "segment_processor failed, skipping segment: {:?}", e);
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

        let effective_config = ctx.config();
        let repair_enabled = effective_config.timestamp_repair.unwrap_or(true);
        match upload_single_file_with_repair(
            &original_path,
            upload_context,
            repair_enabled,
            effective_config.audio_normalization_enabled,
            effective_config.effective_audio_target_lufs(),
        )
        .await
        {
            Ok((video, outcome, _normalization_artifact)) => {
                // The remote upload consumed this logical position even if local session
                // persistence fails; never reuse the order for a later segment.
                next_order = next_order.saturating_add(1);
                // 上传成功后落库累积。落库失败则保留本地文件（不删），保证「未 durable 不删」。
                if let Err(e) = persist_segment(ctx, &mut archive, video).await {
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
                let segment_order = next_order;
                next_order = next_order.saturating_add(1);
                if let Err(session_error) = ensure_archive_session(ctx, &mut archive).await {
                    error!(
                        ?session_error,
                        file = ?original_path,
                        "上传失败后创建本地投稿会话失败，将以未绑定状态登记分段"
                    );
                }
                error!(file = ?original_path, segment_order, "upload_single_file failed, queueing missing segment: {:?}", e);
                if let Err(queue_err) = enqueue_missing_segment(
                    ctx.pool(),
                    ctx.worker_id(),
                    ctx.id(),
                    archive.session_row_id,
                    archive.aid.map(|aid| aid as i64),
                    &original_path,
                    event.danmaku_file_path.as_deref(),
                    segment_order,
                    err,
                    chrono::Utc::now(),
                )
                .await
                {
                    error!(file = ?original_path, "failed to enqueue missing segment: {:?}", queue_err);
                }
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
        upload_single_file(&upload_path, context).await
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

async fn upload_single_file(file_path: &Path, context: &UploadContext) -> AppResult<Video> {
    let video_path = file_path;
    let UploadContext {
        bilibili,
        line,
        threads: limit,
        client,
        rate_gate,
        pool,
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
            return Err(error_stack::Report::new(error).change_context(AppError::Unknown));
        }
    };

    let instant = Instant::now();

    let video = uploader
        .upload(client.clone(), *limit, |vs| {
            vs.map(|vs| {
                let chunk = vs?;
                let len = chunk.len();
                Ok((chunk, len))
            })
        })
        .await
        .change_context(AppError::Unknown)?;
    let t = instant.elapsed().as_millis();
    info!(
        "Upload completed: {file_name} => cost {:.2}s, {:.2} MB/s.",
        t as f64 / 1000.,
        total_size as f64 / 1000. / t as f64
    );
    Ok(video)
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
        row.status = "uploading".to_string();
        row.updated_at = chrono::Utc::now();
        let line_index = row.line_index;
        let file_path = row.file_path.clone();
        let segment_order = row.segment_order;
        let row_id = row.id;
        row = row
            .update_all_fields(ctx.pool())
            .await
            .change_context(AppError::Unknown)?;

        let selected_line = upload_line_for_recovery(line_index);
        let recovery_context = if let Some(line) = selected_line {
            let line_str = match line {
                UploadLine::Bda2 => "bda2",
                UploadLine::Tx => "tx",
                UploadLine::Bldsa => "bldsa",
                // Unknown future variant: fall through to probe instead of silently misrouting to bda2
                _ => "auto",
            };
            UploadContext {
                bilibili: upload_context.bilibili.clone(),
                line: get_upload_line(&upload_context.client.client, line_str).await?,
                threads: upload_context.threads,
                client: upload_context.client.clone(),
                rate_gate: upload_context.rate_gate,
                pool: upload_context.pool.clone(),
            }
        } else {
            UploadContext {
                bilibili: upload_context.bilibili.clone(),
                line: Probe::probe(&upload_context.client.client)
                    .await
                    .change_context(AppError::Unknown)?,
                threads: upload_context.threads,
                client: upload_context.client.clone(),
                rate_gate: upload_context.rate_gate,
                pool: upload_context.pool.clone(),
            }
        };

        let path = PathBuf::from(&file_path);
        let effective_config = ctx.config();
        let repair_enabled = effective_config.timestamp_repair.unwrap_or(true);
        let result = upload_single_file_with_repair(
            &path,
            &recovery_context,
            repair_enabled,
            effective_config.audio_normalization_enabled,
            effective_config.effective_audio_target_lufs(),
        )
        .await;

        match result {
            Ok((video, outcome, _normalization_artifact)) => {
                let updated =
                    insert_session_video_at_order(ctx.pool(), session_row_id, video, segment_order)
                        .await?;
                archive.videos = updated;
                mark_retry_success(&mut row, chrono::Utc::now());
                row.update_all_fields(ctx.pool())
                    .await
                    .change_context(AppError::Unknown)?;
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
                mark_retry_failure(&mut row, format!("{e:?}"), chrono::Utc::now());
                row.update_all_fields(ctx.pool())
                    .await
                    .change_context(AppError::Unknown)?;
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
    let line = match line {
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
        _ => Probe::probe(&client.client)
            .await
            .change_context(AppError::Unknown)?,
    };
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
                return Err(error_stack::Report::new(error).change_context(AppError::Unknown));
            }
        };

        let instant = Instant::now();

        let video = uploader
            .upload(client.clone(), limit, |vs| {
                vs.map(|vs| {
                    let chunk = vs?;
                    let len = chunk.len();
                    Ok((chunk, len))
                })
            })
            .await
            .change_context_lazy(|| AppError::Unknown)?;
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
) -> AppResult<()> {
    let mut row = UploadMissingSegment::select()
        .where_("id = ?")
        .bind(missing_id)
        .fetch_one(pool)
        .await
        .change_context(AppError::Unknown)?;

    // Atomic claim: flip to 'uploading' only if the row is still actionable.
    // Rows already 'succeeded', 'uploading' (another recovery in flight), or otherwise
    // finalized are intentionally skipped — idempotent no-op.
    let claim_now = chrono::Utc::now();
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
        // Already succeeded, or another recovery is in flight — nothing to do.
        return Ok(());
    }

    // Perform the upload + edit/insert inside a fallible scope so that any failure
    // resets the row to 'failed' and persists the error before returning.
    let span = tracing::info_span!("session", session = row.upload_session_id);
    let upload_result: AppResult<()> = async {
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
        let upload_context = initialize_upload_context(
            &effective_config,
            &StatelessClient::default(),
            &upload_config,
            pool,
        )
        .await?;
        let path = PathBuf::from(&row.file_path);
        let repair_enabled = effective_config.timestamp_repair.unwrap_or(true);
        let (video, outcome, _normalization_artifact) = upload_single_file_with_repair(
            &path,
            &upload_context,
            repair_enabled,
            effective_config.audio_normalization_enabled,
            effective_config.effective_audio_target_lufs(),
        )
        .await?;
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
                patch_studio_videos(&mut studio, video, row.segment_order);
                bilibili
                    .edit_by_app(&studio, None)
                    .await
                    .change_context(AppError::Unknown)?;
                info!(
                    aid,
                    segment_order = row.segment_order,
                    "manual_recover_edit_archive：手动补传已追加到稿件"
                );
            } else {
                insert_session_video_at_order(pool, session_id, video, row.segment_order).await?;
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
            patch_studio_videos(&mut studio, video, row.segment_order);
            bilibili
                .edit_by_app(&studio, None)
                .await
                .change_context(AppError::Unknown)?;
            info!(
                aid,
                segment_order = row.segment_order,
                "manual_recover_edit_archive：手动补传已追加到稿件"
            );
        } else {
            return Err(error_stack::Report::new(AppError::Custom(
                "missing segment has neither upload_session_id nor aid".to_string(),
            )));
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

        Ok(())
    }
    .instrument(span)
    .await;

    match upload_result {
        Ok(()) => {
            mark_retry_success(&mut row, chrono::Utc::now());
            row = row
                .update_all_fields(pool)
                .await
                .change_context(AppError::Unknown)?;
            let _ = row;
            Ok(())
        }
        Err(e) => {
            mark_retry_failure(&mut row, format!("{e:?}"), chrono::Utc::now());
            row.update_all_fields(pool)
                .await
                .change_context(AppError::Unknown)?;
            Err(e)
        }
    }
}

pub async fn retry_missing_segment(
    config: &Config,
    pool: &ConnectionPool,
    missing_id: i64,
) -> AppResult<()> {
    let mut row = UploadMissingSegment::select()
        .where_("id = ?")
        .bind(missing_id)
        .fetch_one(pool)
        .await
        .change_context(AppError::Unknown)?;

    if row.status == "succeeded" {
        return Ok(());
    }

    if row.status == "uploading" {
        let now = chrono::Utc::now();
        reset_for_manual_retry(&mut row, now);
        row.update_all_fields(pool)
            .await
            .change_context(AppError::Unknown)?;
    }

    manual_recover_missing_segment(config, pool, missing_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
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
            self.handle_message(msg).await;
        }
    }

    /// 处理上传消息
    ///
    /// # 参数
    /// * `msg` - 要处理的上传消息
    async fn handle_message(&mut self, msg: UploaderMessage) {
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
