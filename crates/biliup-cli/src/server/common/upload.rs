use crate::UploadLine;
use crate::server::common::cover_generator::{CoverOptions, render_to_tempfile};
use crate::server::common::upload_session::{
    LiveArchive, active_sessions_for_room, finalize_session, insert_session, parse_videos,
    reattach_session, select_recovery_candidate, update_session_after_submit,
    update_session_videos,
};
use crate::server::common::util::Recorder;
use crate::server::config::Config;
use crate::server::core::downloader::SegmentInfo;
use crate::server::errors::{AppError, AppResult};
use crate::server::infrastructure::context::{Context, Stage, WorkerStatus};
use crate::server::infrastructure::models::InsertFileItem;
use crate::server::infrastructure::models::hook_step::{
    HookStep, process_video, process_video_paths,
};
use crate::server::infrastructure::models::upload_streamer::UploadStreamer;
use async_channel::Receiver;
use biliup::bilibili::{BiliBili, ResponseData, Studio, Video};
use biliup::client::StatelessClient;
use biliup::credential::login_by_cookies;
use biliup::error::Kind;
use biliup::uploader::line::{Line, Probe};
use biliup::uploader::util::SubmitOption;
use biliup::uploader::{VideoFile, line};
use error_stack::{ResultExt, bail};
use futures::StreamExt;
use futures::stream::Inspect;
use ormlite::Insert;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Instant;
use tokio::pin;
use tracing::{error, info, warn};

// 辅助结构体
struct UploadContext {
    bilibili: BiliBili,
    line: Line,
    threads: usize,
    client: StatelessClient,
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

    let archive =
        pipeline_upload_videos(rx, &upload_context, upload_config, &segment_processors, ctx)
            .await?;

    if let Some(archive) = archive
        && let Some(row_id) = archive.session_row_id
    {
        if let Err(e) = finalize_session(ctx.pool(), row_id).await {
            warn!(?e, "finalize upload_session 失败");
        }
    }

    Ok(())
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

/// 每段上传成功后：首段建稿，后续 edit 追加。就地更新 archive 与缓存 studio，并落库。
async fn submit_or_append_segment(
    ctx: &Context,
    upload_context: &UploadContext,
    upload_config: &UploadStreamer,
    archive: &mut LiveArchive,
    cached_studio: &mut Option<Studio>,
    video: Video,
) -> AppResult<()> {
    let bilibili = &upload_context.bilibili;
    let room_id = ctx.worker_id();
    let streamer_info_id = ctx.id();

    if archive.aid.is_none() {
        // 首段：构建 studio（封面上传/自动封面等一次性开销）并建稿。
        let mut recorder = ctx.recorder(ctx.streamer_info().clone());
        recorder.filename_prefix = upload_config.title.clone();
        let mut studio =
            build_studio(upload_config, bilibili, vec![video.clone()], &recorder).await?;

        let submit_api = ctx.config().submit_api.clone();
        let resp = submit_to_bilibili(bilibili, &studio, submit_api.as_deref()).await?;
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

        archive.videos = vec![video];
        archive.aid = aid;
        archive.bvid = bvid.clone();

        if let (Some(aid_val), Some(row_id)) = (aid, archive.session_row_id) {
            update_session_after_submit(ctx.pool(), row_id, aid_val, bvid.clone(), &archive.videos).await?;
        } else if let Some(aid_val) = aid {
            let row =
                insert_session(ctx.pool(), room_id, streamer_info_id, aid_val, bvid.clone(), &archive.videos)
                    .await?;
            archive.session_row_id = Some(row.id);
        } else {
            warn!(?resp, "建稿响应缺少 aid，无法落库 upload_session");
        }

        studio.aid = archive.aid;
        *cached_studio = Some(studio);

        if let (Some(section_id), Some(aid_val)) = (ctx.config().season_section_id, archive.aid) {
            add_archive_to_season_with_retry(bilibili, section_id, aid_val).await;
        }
    } else {
        // 后续段：追加到已有 aid。
        if cached_studio.is_none() {
            // 重启续接：进程内无缓存 studio，用 B 站现有稿件数据兜底重建。
            if let Some(aid_val) = archive.aid {
                let vid = biliup::bilibili::Vid::Aid(aid_val);
                match bilibili.studio_data(&vid, None).await {
                    Ok(mut s) => {
                        s.aid = archive.aid;
                        *cached_studio = Some(s);
                    }
                    Err(e) => {
                        warn!(?e, "studio_data 兜底失败，改为重建最小 studio");
                        let mut recorder = ctx.recorder(ctx.streamer_info().clone());
                        recorder.filename_prefix = upload_config.title.clone();
                        let mut s = build_studio(
                            upload_config,
                            bilibili,
                            archive.videos.clone(),
                            &recorder,
                        )
                        .await?;
                        s.aid = archive.aid;
                        *cached_studio = Some(s);
                    }
                }
            }
        }
        archive.videos.push(video);
        let Some(studio) = cached_studio.as_mut() else {
            bail!(AppError::Custom("cached studio missing for edit".into()));
        };
        studio.aid = archive.aid;
        studio.videos = archive.videos.clone();
        bilibili
            .edit_by_app(studio, None)
            .await
            .change_context(AppError::Unknown)?;
        if let Some(row_id) = archive.session_row_id {
            update_session_videos(ctx.pool(), row_id, &archive.videos).await?;
        }
    }
    Ok(())
}

/// 开播时准备本场稿件状态：命中窗口内未 finalize 的同 room 会话则续接，否则返回空 archive。
async fn prepare_archive(ctx: &Context) -> AppResult<LiveArchive> {
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
        let aid = candidate.aid.map(|a| a as u64);
        let bvid = candidate.bvid.clone();
        info!(aid=?aid, room_id, "重启续接已有稿件，将 edit 追加后续分段");
        let row = reattach_session(ctx.pool(), candidate, ctx.id()).await?;
        Ok(LiveArchive {
            session_row_id: Some(row.id),
            aid,
            bvid,
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
    let mut archive = prepare_archive(ctx).await?;
    let mut cached_studio: Option<Studio> = None;
    pin!(rx);
    while let Some(event) = rx.next().await {
        let mut paths = segment_paths(&event);
        if !segment_processors.is_empty()
            && let Err(e) = process_video_paths(&mut paths, segment_processors).await
        {
            error!(file = ?event.prev_file_path, "segment_processor failed, skipping segment: {:?}", e);
            continue;
        }
        let upload_path = paths
            .first()
            .cloned()
            .unwrap_or_else(|| event.prev_file_path.clone());

        match upload_single_file(&upload_path, upload_context).await {
            Ok(video) => {
                if let Err(e) = submit_or_append_segment(
                    ctx,
                    upload_context,
                    upload_config,
                    &mut archive,
                    &mut cached_studio,
                    video,
                )
                .await
                {
                    error!(file = ?upload_path, "submit/append failed, keeping local file: {:?}", e);
                    continue;
                }
                if let Err(e) = execute_postprocessor(paths, ctx).await {
                    error!(file = ?upload_path, "per-segment postprocessor failed: {:?}", e);
                }
            }
            Err(e) => {
                error!(file = ?upload_path, "upload_single_file failed, skipping segment: {:?}", e);
            }
        }
    }
    Ok(if archive.aid.is_some() { Some(archive) } else { None })
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
        let lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
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
                        let mut paths = Vec::new();
                        pin!(inspect);
                        while let Some(event) = inspect.next().await {
                            paths.extend(segment_paths(&event));
                        }
                        // 无上传配置时，直接执行后处理
                        execute_postprocessor(paths, &ctx).await
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
