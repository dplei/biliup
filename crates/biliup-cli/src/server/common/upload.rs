use crate::UploadLine;
use crate::server::common::cookie_health::notify_alert;
use crate::server::common::cover_generator::{
    CoverOptions, render_to_tempfile, split_template_lines,
};
use crate::server::common::missing_segment::{
    due_missing_segments_for_session, enqueue_missing_segment, mark_retry_failure,
    mark_retry_success, next_segment_order, patch_studio_videos, upload_line_for_recovery,
};
use crate::server::common::timestamp_repair::{RepairOutcome, SystemFfmpeg, normalize_timestamps};
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
use futures::stream::Inspect;
use ormlite::Insert;
use ormlite::Model;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::OnceLock;
use std::time::Instant;
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

pub async fn process_with_upload<F>(
    rx: Inspect<Receiver<SegmentInfo>, F>,
    ctx: &Context,
    upload_config: &UploadStreamer,
) -> AppResult<()>
where
    F: FnMut(&SegmentInfo),
{
    info!(upload_config=?upload_config, "Starting process with upload");
    let upload_context =
        initialize_upload_context(&ctx.config(), &ctx.stateless_client(), upload_config).await?;

    let segment_processors: Vec<HookStep> = ctx
        .live_streamer()
        .segment_processor
        .clone()
        .unwrap_or_default();

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

async fn initialize_upload_context(
    config: &Config,
    client: &StatelessClient,
    upload_config: &UploadStreamer,
) -> AppResult<UploadContext> {
    // 登录处理
    let cookie_file = upload_config
        .user_cookie
        .clone()
        .unwrap_or("cookies.json".to_string());
    let bilibili = login_by_cookies(&cookie_file, None)
        .await
        .change_context(AppError::Unknown)?;

    // 获取上传线路
    let line = get_upload_line(&client.client, &config.lines).await?;

    Ok(UploadContext {
        bilibili,
        line,
        threads: config.threads as usize,
        client: client.clone(),
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
        _ => Probe::probe(client).await.unwrap_or_default(),
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

/// 一次性提交一个会话累积的全部分段：构建 studio → submit_by_app → 写回 aid 并 finalize → 加合集。
/// 仅在成功拿到 aid 时才 finalize；失败保持 uploading，留待下次开播补提交。
/// `streamer_info` 决定标题/时间，补提交废弃会话时传该会话当时的 streamer_info。
async fn submit_session(
    upload_context: &UploadContext,
    pool: &ConnectionPool,
    upload_config: &UploadStreamer,
    season_section_id: Option<i64>,
    submit_api: Option<&str>,
    streamer_info: &StreamerInfo,
    session_row_id: i64,
    videos: &[Video],
) -> AppResult<()> {
    let bilibili = &upload_context.bilibili;
    let recorder = Recorder::new(upload_config.title.clone(), streamer_info.clone());
    let studio = build_studio(upload_config, bilibili, videos.to_vec(), &recorder).await?;
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
    match archive.session_row_id {
        Some(row_id) => update_session_videos(ctx.pool(), row_id, &archive.videos).await,
        None => {
            let row =
                insert_uploading_session(ctx.pool(), ctx.worker_id(), ctx.id(), &archive.videos)
                    .await?;
            archive.session_row_id = Some(row.id);
            Ok(())
        }
    }
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
        let videos = parse_videos(&stale.videos_json);
        if videos.is_empty() {
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
            n = videos.len(),
            "补提交上一场未提交的废弃会话"
        );
        if let Err(e) = submit_session(
            upload_context,
            ctx.pool(),
            upload_config,
            config.season_section_id,
            config.submit_api.as_deref(),
            &streamer_info,
            stale.id,
            &videos,
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

async fn pipeline_upload_videos<F>(
    rx: Inspect<Receiver<SegmentInfo>, F>,
    upload_context: &UploadContext,
    upload_config: &UploadStreamer,
    segment_processors: &[HookStep],
    ctx: &Context,
) -> AppResult<Option<LiveArchive>>
where
    F: FnMut(&SegmentInfo),
{
    let mut archive = prepare_archive(upload_context, upload_config, ctx).await?;
    let mut missing_count = 0usize;
    pin!(rx);
    while let Some(event) = rx.next().await {
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

        // 上传前时间戳检测与修复（全局开关，默认开），helper 内部选路并上传。
        let repair_enabled = ctx.config().timestamp_repair.unwrap_or(true);
        let _permit = acquire_global_upload_permit().await;
        match upload_single_file_with_repair(&original_path, upload_context, repair_enabled).await {
            Ok((video, outcome)) => {
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
                        if let Err(e) = execute_postprocessor(paths, ctx).await {
                            error!(file = ?original_path, "per-segment postprocessor failed: {:?}", e);
                        }
                    }
                    RepairOutcome::Clean => {
                        if let Err(e) = execute_postprocessor(paths, ctx).await {
                            error!(file = ?original_path, "per-segment postprocessor failed: {:?}", e);
                        }
                    }
                }
            }
            Err(e) => {
                let err = format!("{e:?}");
                let segment_order = next_segment_order(archive.videos.len(), missing_count);
                missing_count += 1;
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
) -> AppResult<(Video, RepairOutcome)> {
    let outcome = if repair_enabled {
        normalize_timestamps(original_path, &SystemFfmpeg).await
    } else {
        RepairOutcome::Clean
    };
    let upload_path = match &outcome {
        RepairOutcome::Repaired(fixed) => fixed.clone(),
        _ => original_path.to_path_buf(),
    };
    match upload_single_file(&upload_path, context).await {
        Ok(video) => Ok((video, outcome)),
        Err(e) => {
            if let RepairOutcome::Repaired(fixed) = &outcome {
                let _ = tokio::fs::remove_file(fixed).await;
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
    let uploader = line
        .pre_upload(bilibili, video_file)
        .await
        .change_context(AppError::Unknown)?;

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
            }
        } else {
            UploadContext {
                bilibili: upload_context.bilibili.clone(),
                line: Probe::probe(&upload_context.client.client)
                    .await
                    .unwrap_or_default(),
                threads: upload_context.threads,
                client: upload_context.client.clone(),
            }
        };

        let path = PathBuf::from(&file_path);
        let repair_enabled = ctx.config().timestamp_repair.unwrap_or(true);
        let result = {
            let _permit = acquire_global_upload_permit().await;
            upload_single_file_with_repair(&path, &recovery_context, repair_enabled).await
        };

        match result {
            Ok((video, outcome)) => {
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

pub(crate) async fn build_studio(
    upload_config: &UploadStreamer,
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
        match render_to_tempfile(&lines, &CoverOptions::default()) {
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
        _ => Probe::probe(&client.client).await.unwrap_or_default(),
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
        let uploader = line
            .pre_upload(&bilibili, video_file)
            .await
            .change_context_lazy(|| AppError::Unknown)?;

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
        let _streamer_info = get_streamer_info(pool, row.streamer_info_id).await?;
        let upload_config = UploadStreamer::select()
            .where_("id = (SELECT upload_streamers_id FROM livestreamers WHERE id = ?)")
            .bind(row.live_streamer_id)
            .fetch_one(pool)
            .await
            .change_context(AppError::Unknown)?;

        let upload_context =
            initialize_upload_context(config, &StatelessClient::default(), &upload_config).await?;
        let path = PathBuf::from(&row.file_path);
        let repair_enabled = config.timestamp_repair.unwrap_or(true);
        let (video, outcome) = {
            let _permit = acquire_global_upload_permit().await;
            upload_single_file_with_repair(&path, &upload_context, repair_enabled).await?
        };
        match &outcome {
            RepairOutcome::Repaired(fixed) => {
                let _ = tokio::fs::remove_file(fixed).await;
            }
            RepairOutcome::Unfixable => {
                notify_alert(
                    config.cookie_health_webhook.as_deref(),
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

#[cfg(test)]
mod tests {
    use super::*;

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
                let inspect = rx.inspect(|f| {
                    let pool = ctx.pool().clone();
                    let streamer_info_id = ctx.id();
                    let file = f.prev_file_path.display().to_string();
                    tokio::spawn(async move {
                        let result = InsertFileItem {
                            file,
                            streamer_info_id,
                        }
                        .insert(&pool)
                        .await;
                        info!(result=?result, "Insert file");
                    });
                });
                let result = match ctx.upload_config() {
                    Some(config) => process_with_upload(inspect, &ctx, config).await,
                    None => {
                        // 未绑定投稿模板：仅录制。消费分段事件，但【不执行会删文件的后处理】，
                        // 避免误配把没上传的录像静默删掉（footgun）。文件保留本地由用户处置。
                        pin!(inspect);
                        let mut segments = 0u32;
                        while inspect.next().await.is_some() {
                            segments += 1;
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
                    error!("Process segment event failed: {}", e);
                    // 可以添加错误通知机制
                }
                info!(url=ctx.live_streamer().url, result=?result, "后处理执行完毕：Finished processing segment event");
                ctx.change_status(Stage::Upload, WorkerStatus::Idle).await;
            }
        }
    }
}

/// 上传消息枚举
/// 定义上传Actor可以处理的消息类型
#[derive(Debug)]
pub enum UploaderMessage {
    /// 分段事件消息，包含事件、接收器和工作器
    SegmentEvent(Receiver<SegmentInfo>, Context),
}
