use crate::UploadLine;
use crate::observe::{self, standalone::UploadTask};
use crate::server::errors::{AppError, AppResult};
use crate::upload_lock::UploadLock;
use biliup::client::StatelessClient;
use biliup::error::Kind;
use biliup::uploader::bilibili::{BiliBili, Studio, Vid, Video};
use biliup::uploader::credential::{Credential, LoginInfo};
use biliup::uploader::line::Probe;
use biliup::uploader::util::SubmitOption;
use biliup::uploader::{VideoFile, credential, line, load_config};
use bytes::{Buf, Bytes};
use clap::ValueEnum;
use dialoguer::Input;
use dialoguer::Select;
use dialoguer::theme::ColorfulTheme;
use error_stack::ResultExt;
use futures::{Stream, StreamExt};
use image::Luma;
use indicatif::{ProgressBar, ProgressStyle};
use qrcode::QrCode;
use qrcode::render::unicode;
use reqwest::Body;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::io::Seek;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Instant;
use tracing::{info, warn};

// 断点续传的数据结构
#[derive(Serialize, Deserialize, Debug)]
struct UploadCheckpoint {
    videos: Vec<Video>,
    uploaded_files: Vec<String>,
}

impl UploadCheckpoint {
    fn new() -> Self {
        Self {
            videos: Vec::new(),
            uploaded_files: Vec::new(),
        }
    }

    fn load(path: &Path) -> Option<Self> {
        if !path.exists() {
            return None;
        }
        match std::fs::read_to_string(path) {
            Ok(content) => serde_json::from_str(&content).ok(),
            Err(_) => None,
        }
    }

    fn save(&self, path: &Path) -> std::io::Result<()> {
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(path, content)
    }

    fn is_uploaded(&self, file_path: &Path) -> bool {
        let file_name = file_path.to_string_lossy().to_string();
        self.uploaded_files.contains(&file_name)
    }

    fn add_video(&mut self, file_path: &Path, video: Video) {
        self.videos.push(video);
        self.uploaded_files
            .push(file_path.to_string_lossy().to_string());
    }
}

pub async fn login(user_cookie: PathBuf, proxy: Option<&str>) -> AppResult<()> {
    let client = Credential::new(proxy);
    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("选择一种登录方式")
        .default(1)
        .item("账号密码")
        .item("短信登录")
        .item("扫码登录")
        .item("浏览器登录")
        .item("网页Cookie登录1")
        .item("网页Cookie登录2")
        .interact()
        .change_context_lazy(|| AppError::Unknown)?;
    let info = observe::auth::observe(
        "bilibili",
        "login",
        match selection {
            0 => login_by_password(client).await,
            1 => login_by_sms(client).await,
            2 => login_by_qrcode(client).await,
            3 => login_by_browser(client).await,
            4 => login_by_web_cookies(client).await,
            5 => login_by_webqr_cookies(client).await,
            _ => panic!(),
        },
    )?;
    let file = std::fs::File::create(user_cookie).change_context_lazy(|| AppError::Unknown)?;
    serde_json::to_writer_pretty(&file, &info).change_context_lazy(|| AppError::Unknown)?;
    info!("登录成功，数据保存在{:?}", file);
    Ok(())
}

pub async fn renew(user_cookie: PathBuf, proxy: Option<&str>) -> AppResult<()> {
    let client = Credential::new(proxy);
    let mut file = fopen_rw(user_cookie)?;
    let login_info: LoginInfo =
        serde_json::from_reader(&file).change_context_lazy(|| AppError::Unknown)?;
    let new_info = observe::auth::observe(
        "bilibili",
        "renew",
        client
            .renew_tokens(login_info)
            .await
            .change_context_lazy(|| AppError::Unknown),
    )?;
    file.rewind().change_context_lazy(|| AppError::Unknown)?;
    file.set_len(0).change_context_lazy(|| AppError::Unknown)?;
    serde_json::to_writer_pretty(std::io::BufWriter::new(&file), &new_info)
        .change_context_lazy(|| AppError::Unknown)?;
    info!("{new_info:?}");
    Ok(())
}

pub async fn upload_by_command(
    mut studio: Studio,
    user_cookie: PathBuf,
    video_path: Vec<PathBuf>,
    line: Option<UploadLine>,
    limit: usize,
    submit: SubmitOption,
    proxy: Option<&str>,
) -> AppResult<()> {
    let task = UploadTask::default();
    if video_path.is_empty() {
        observe::submission_decided(&task.submission, "failed", "no_input", 0);
        return Err(AppError::Custom(
            "No video files specified. Please provide at least one video file path.".to_string(),
        )
        .into());
    }
    let bili = task.check(
        login_by_cookies(user_cookie, proxy).await,
        "authentication_failed",
    )?;
    if studio.title.is_empty() {
        studio.title = video_path[0]
            .file_stem()
            .and_then(OsStr::to_str)
            .map(|s| s.to_string())
            .unwrap();
    }
    task.check(cover_up(&mut studio, &bili).await, "cover_failed")?;
    studio.videos = task.check(
        upload_with_task(&video_path, &bili, line, limit, &task).await,
        "upload_failed",
    )?;

    task.submit(async {
        Ok(match submit {
            SubmitOption::BCutAndroid => bili
                .submit_by_bcut_android(&studio, proxy)
                .await
                .change_context_lazy(|| AppError::Unknown)?,
            SubmitOption::Web => bili
                .submit_by_web(&studio, proxy)
                .await
                .change_context_lazy(|| AppError::Unknown)?,
            _ => bili
                .submit_by_app(&studio, proxy)
                .await
                .change_context_lazy(|| AppError::Unknown)?,
        })
    })
    .await?;

    Ok(())
}

pub async fn upload_by_config(
    config: PathBuf,
    user_cookie: PathBuf,
    submit_override: Option<SubmitOption>,
    proxy: Option<&str>,
) -> AppResult<()> {
    // println!("number of concurrent futures: {limit}");
    let setup = UploadTask::default();
    let bilibili = setup.check(
        login_by_cookies(user_cookie, proxy).await,
        "authentication_failed",
    )?;
    let config = setup.check(
        load_config(&config).change_context_lazy(|| AppError::Unknown),
        "config_failed",
    )?;
    observe::submission_decided(&setup.submission, "skipped", "config_dispatch", 0);
    for (filename_patterns, mut studio) in config.streamers {
        let task = UploadTask::default();
        let mut paths = Vec::new();
        for entry in task
            .check(
                glob::glob(&filename_patterns).change_context_lazy(|| AppError::Unknown),
                "config_failed",
            )?
            .filter_map(Result::ok)
        {
            paths.push(entry);
        }
        if paths.is_empty() {
            warn!("未搜索到匹配的视频文件：{filename_patterns}");
            observe::submission_decided(&task.submission, "skipped", "no_input", 0);
            continue;
        }
        task.check(cover_up(&mut studio, &bilibili).await, "cover_failed")?;

        studio.videos = task.check(
            upload_with_task(
                &paths,
                &bilibili,
                config
                    .line
                    .as_ref()
                    .and_then(|l| UploadLine::from_str(l, true).ok()),
                config.limit,
                &task,
            )
            .await,
            "upload_failed",
        )?;
        // 命令行参数优先，如果没有提供则使用配置文件中的设置
        let submit_option = submit_override.clone().unwrap_or(config.submit.clone());
        task.submit(async {
            Ok(match submit_option {
                SubmitOption::BCutAndroid => bilibili
                    .submit_by_bcut_android(&studio, proxy)
                    .await
                    .change_context_lazy(|| AppError::Unknown)?,
                SubmitOption::Web => bilibili
                    .submit_by_web(&studio, proxy)
                    .await
                    .change_context_lazy(|| AppError::Unknown)?,
                _ => bilibili
                    .submit_by_app(&studio, proxy)
                    .await
                    .change_context_lazy(|| AppError::Unknown)?,
            })
        })
        .await?;
    }
    Ok(())
}

/// 把 1 起的分P序号换算成 `studio.videos` 的下标，越界就说清楚稿件到底有几个分P。
fn replace_index(part: usize, parts: usize) -> Result<usize, String> {
    match part.checked_sub(1) {
        Some(index) if index < parts => Ok(index),
        _ => Err(format!(
            "分P序号从 1 开始，这个稿件只有 {parts} 个分P，替换不了第 {part} 个"
        )),
    }
}

pub async fn append(
    user_cookie: PathBuf,
    vid: Vid,
    video_path: Vec<PathBuf>,
    line: Option<UploadLine>,
    limit: usize,
    submit: SubmitOption,
    replace: Option<usize>,
    execute: bool,
    proxy: Option<&str>,
) -> AppResult<()> {
    let task = UploadTask::default();
    if video_path.is_empty() {
        observe::submission_decided(&task.submission, "failed", "no_input", 0);
        return Err(AppError::Custom(
            "No video files specified. Please provide at least one video file path.".to_string(),
        )
        .into());
    }
    let bilibili = task.check(
        login_by_cookies(user_cookie, proxy).await,
        "authentication_failed",
    )?;
    // 稿件先取回来：替换要在**上传之前**校验分P序号并让人确认，序号写错时不该已经白传一遍。
    let mut studio = task.check(
        bilibili
            .studio_data(&vid, proxy)
            .await
            .change_context_lazy(|| AppError::Unknown),
        "target_lookup_failed",
    )?;
    if let Some(part) = replace {
        let index = replace_index(part, studio.videos.len())
            .map_err(|message| AppError::Custom(message))?;
        if video_path.len() != 1 {
            return Err(AppError::Custom(format!(
                "替换一个分P只能给一个文件，收到 {} 个",
                video_path.len()
            ))
            .into());
        }
        let old = &studio.videos[index];
        println!("稿件 {vid} 第 {part} 个分P：");
        println!("  现在  title={:?} filename={}", old.title, old.filename);
        println!("  换成  {}", video_path[0].display());
        if !execute {
            println!();
            println!("这是预演：没有上传任何文件，也没有改动稿件。确认无误后加 --execute 再跑一次。");
            return Ok(());
        }
    }
    let mut uploaded_videos = task.check(
        upload_with_task(&video_path, &bilibili, line, limit, &task).await,
        "upload_failed",
    )?;
    match replace {
        Some(part) => {
            // 序号在上传前已经校验过，这里重算一次只是为了拿下标。
            let index = replace_index(part, studio.videos.len())
                .map_err(|message| AppError::Custom(message))?;
            let replacement = uploaded_videos.remove(0);
            println!(
                "替换第 {part} 个分P：{} -> {}",
                studio.videos[index].filename, replacement.filename
            );
            // 标题跟着老分P走：换的是坏掉的那个文件，不是这一P的身份。
            let title = studio.videos[index].title.clone();
            studio.videos[index] = replacement;
            studio.videos[index].title = title;
        }
        None => studio.videos.append(&mut uploaded_videos),
    }
    observe::submission_started(&task.submission, "append_ready");
    let result = async {
        Ok::<_, error_stack::Report<AppError>>(match submit {
            SubmitOption::App => bilibili
                .edit_by_app(&studio, proxy)
                .await
                .change_context_lazy(|| AppError::Unknown)?,
            _ => bilibili
                .edit_by_web(&studio)
                .await
                .change_context_lazy(|| AppError::Unknown)?,
        })
    }
    .await;
    observe::submission_completed(
        &task.submission,
        if result.is_ok() {
            "succeeded"
        } else {
            "unknown"
        },
        if result.is_ok() {
            if replace.is_some() { "part_replaced" } else { "appended" }
        } else {
            "request_failed"
        },
    );
    result?;
    // studio.edit(&login_info).await?;
    Ok(())
}

pub async fn show(user_cookie: PathBuf, vid: Vid, proxy: Option<&str>) -> AppResult<()> {
    let bilibili = login_by_cookies(user_cookie, proxy).await?;
    let video_info = bilibili
        .video_data(&vid, proxy)
        .await
        .change_context_lazy(|| AppError::Unknown)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&video_info).change_context_lazy(|| AppError::Unknown)?
    );
    Ok(())
}

pub async fn comments(
    user_cookie: PathBuf,
    vid: Vid,
    sort: u8,
    pn: u32,
    ps: u32,
    proxy: Option<&str>,
) -> AppResult<()> {
    let bilibili = login_by_cookies(user_cookie, proxy).await?;
    let reply_list = bilibili
        .comments(&vid, sort, pn, ps, proxy)
        .await
        .change_context_lazy(|| AppError::Unknown)?;

    for reply in reply_list.replies.unwrap_or_default() {
        println!("rpid={}  uname={}", reply.rpid, reply.member.uname);
        println!("{}", reply.content.message);
        println!();
    }

    Ok(())
}

pub async fn reply(
    user_cookie: PathBuf,
    vid: Vid,
    rpid: u64,
    message: String,
    execute: bool,
    proxy: Option<&str>,
) -> AppResult<()> {
    if !execute {
        println!("dry-run: reply to {vid} rpid={rpid}");
        println!("{message}");
        println!("use --execute to send");
        return Ok(());
    }

    let bilibili = login_by_cookies(user_cookie, proxy).await?;
    let ret = bilibili
        .reply_comment(&vid, rpid, &message, proxy)
        .await
        .change_context_lazy(|| AppError::Unknown)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&ret).change_context_lazy(|| AppError::Unknown)?
    );
    Ok(())
}

pub async fn list(
    user_cookie: PathBuf,
    is_pubing: bool,
    pubed: bool,
    not_pubed: bool,
    proxy: Option<&str>,
    from_page: u32,
    max_pages: Option<u32>,
) -> AppResult<()> {
    let status = match (is_pubing, pubed, not_pubed) {
        (true, false, false) => "is_pubing",
        (false, true, false) => "pubed",
        (false, false, true) => "not_pubed",
        (false, false, false) => "is_pubing,pubed,not_pubed",
        _ => {
            tracing::warn!("选项互斥，默认列出所有状态的稿件");
            "is_pubing,pubed,not_pubed"
        }
    };

    let bilibili = login_by_cookies(user_cookie, proxy).await?;
    bilibili
        .recent_archives(status, from_page, max_pages)
        .await
        .change_context_lazy(|| AppError::Unknown)?
        .iter()
        .for_each(|arc| println!("{}", arc.to_string_pretty()));
    Ok(())
}

async fn login_by_cookies(user_cookie: PathBuf, proxy: Option<&str>) -> AppResult<BiliBili> {
    let result = credential::login_by_cookies(&user_cookie, proxy).await;
    Ok(match result {
        Err(Kind::IO(_)) => result.change_context_lazy(|| {
            AppError::Custom(String::from("open cookies file: ") + &user_cookie.to_string_lossy())
        })?,
        _ => {
            let bili = result.change_context_lazy(|| AppError::Unknown)?;
            let info = bili
                .my_info()
                .await
                .change_context_lazy(|| AppError::Unknown)?;
            info!(
                "user: {}",
                info["data"]["name"]
                    .as_str()
                    .ok_or_else(|| AppError::Custom(format!("{info}no name")))?
            );
            bili
        }
    })
}

pub async fn cover_up(studio: &mut Studio, bili: &BiliBili) -> AppResult<()> {
    if !studio.cover.is_empty() {
        // 扩展路径中的 ~ 为用户主目录
        let expanded = shellexpand::tilde(&studio.cover);
        let cover_path = PathBuf::from(expanded.as_ref());

        let url = bili
            .cover_up(&std::fs::read(&cover_path).change_context_lazy(|| {
                AppError::Custom(format!("cover: {}", cover_path.display()))
            })?)
            .await
            .change_context_lazy(|| AppError::Unknown)?;
        info!("{url}");
        studio.cover = url;
    }
    Ok(())
}

pub async fn upload(
    video_path: &[PathBuf],
    bili: &BiliBili,
    line: Option<UploadLine>,
    limit: usize,
) -> AppResult<Vec<Video>> {
    let task = UploadTask::default();
    task.check(
        upload_with_task(video_path, bili, line, limit, &task).await,
        "upload_failed",
    )
}

async fn upload_with_task(
    video_path: &[PathBuf],
    bili: &BiliBili,
    line: Option<UploadLine>,
    limit: usize,
    task: &UploadTask,
) -> AppResult<Vec<Video>> {
    info!("number of concurrent futures: {limit}");

    // 生成断点续传文件路径（基于视频列表的哈希）
    let checkpoint_filename = format!(
        "biliup_checkpoint_{}.json",
        video_path
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("_")
            .chars()
            .fold(0u64, |acc, c| acc.wrapping_mul(31).wrapping_add(c as u64))
    );

    // 使用平台相关的本地数据目录，Windows 下是 %LOCALAPPDATA%，Linux/macOS 下是 /tmp
    let checkpoint_path = if let Some(data_dir) = dirs::data_local_dir() {
        data_dir.join(checkpoint_filename)
    } else {
        // 如果无法获取数据目录，回退到临时目录
        std::env::temp_dir().join(checkpoint_filename)
    };

    // 尝试加载已有的断点续传数据
    let mut checkpoint = UploadCheckpoint::load(&checkpoint_path).unwrap_or_else(|| {
        info!("No checkpoint found, starting fresh upload");
        UploadCheckpoint::new()
    });

    if !checkpoint.uploaded_files.is_empty() {
        info!(
            "Found checkpoint with {} uploaded files, resuming...",
            checkpoint.uploaded_files.len()
        );
    }

    let mut videos = checkpoint.videos.clone();
    let client = StatelessClient::default();
    let line_reason = if line.is_some() {
        "configured"
    } else {
        "automatic"
    };
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
    // let line = line::kodo();
    for (index, video_path) in video_path.iter().enumerate() {
        let identity = task.file(video_path, index + 1);
        // 检查文件是否已经上传
        if checkpoint.is_uploaded(video_path) {
            info!("Skipping already uploaded file: {}", video_path.display());
            observe::recovery_decided(&identity, "skipped", "checkpoint_reused");
            continue;
        }

        observe::upload_queued(&identity, "awaiting_pre_upload");
        let identity = identity.with_attempt(&uuid::Uuid::new_v4().to_string());
        info!("{line:?}");
        let video_file = VideoFile::new(video_path)
            .inspect_err(|_| observe::upload_failed(&identity, "source_io", "无法读取上传文件"))
            .change_context_lazy(|| {
                AppError::Custom(format!("file {}", video_path.to_string_lossy()))
            })?;
        let total_size = video_file.total_size;
        let file_name = video_file.file_name.clone();

        // 使用通用的 retry 函数处理限流错误（code: 601）
        // 配合账号级互斥锁防止多进程同时重试
        let credential_id = format!("{}", bili.login_info.token_info.mid);
        let upload_lock = Arc::new(Mutex::new(
            UploadLock::new(&credential_id)
                .inspect_err(|_| observe::upload_failed(&identity, "lock_failed", "无法创建上传锁"))
                .map_err(|e| AppError::Custom(format!("Failed to create upload lock: {}", e)))?,
        ));

        // 在开始上传前检查是否有其他进程正在等待限流恢复
        {
            let lock = upload_lock.lock().unwrap();
            if lock.is_locked() {
                observe::upload_failed(&identity, "cooldown", "其他上传任务正在等待限流恢复");
                return Err(AppError::Custom(format!(
                    "另一个使用该账号 ({}) 的上传进程正在等待限流恢复，请稍后重试",
                    credential_id
                ))
                .into());
            }
        }

        // 用于追踪是否已经尝试获取锁
        let lock_acquired = Arc::new(Mutex::new(false));

        // 执行上传，遇到限流错误时自动重试
        let (uploader, identity) = {
            let upload_lock_clone = Arc::clone(&upload_lock);
            let lock_acquired_clone = Arc::clone(&lock_acquired);

            biliup::retry_with_config(
                || {
                    let identity = identity.with_attempt(&uuid::Uuid::new_v4().to_string());
                    let line = &line;
                    async move {
                        observe::upload_line_decided(
                            &identity,
                            line.key(),
                            "executed",
                            line_reason,
                        );
                        let video_file_clone = VideoFile::new(video_path)
                            .inspect_err(|_| {
                                observe::upload_failed(&identity, "source_io", "无法读取上传文件")
                            })
                            .map_err(|e| {
                                Kind::Custom(format!(
                                    "file {}: {}",
                                    video_path.to_string_lossy(),
                                    e
                                ))
                            })?;
                        let result = line.pre_upload(bili, video_file_clone).await;
                        result
                            .inspect_err(|error| observe::standalone::failed(&identity, error))
                            .map(|uploader| (uploader, identity))
                    }
                },
                5,
                Some(move |e: &Kind| {
                    if matches!(e, Kind::RateLimit { .. }) {
                        let mut acquired = lock_acquired_clone.lock().unwrap();
                        if !*acquired {
                            // 第一次遇到限流错误，尝试获取锁
                            let mut lock = upload_lock_clone.lock().unwrap();
                            match lock.try_acquire() {
                                Ok(true) => {
                                    info!("检测到限流，成功获取上传锁，将进行重试");
                                    *acquired = true;
                                    true
                                }
                                Ok(false) => {
                                    warn!("检测到其他进程正在处理限流，本进程退出");
                                    false
                                }
                                Err(e) => {
                                    warn!("尝试获取锁时出错: {}", e);
                                    true // 出错时仍然尝试重试
                                }
                            }
                        } else {
                            // 已经获取锁，继续重试
                            true
                        }
                    } else {
                        false
                    }
                }),
            )
            .await
            .change_context_lazy(|| AppError::Custom("after retries".to_owned()))?
        };

        // 上传成功后释放锁
        if *lock_acquired.lock().unwrap() {
            let mut lock = upload_lock.lock().unwrap();
            let _ = lock.release();
        }
        //Progress bar
        let pb = ProgressBar::new(total_size);
        pb.set_style(ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})").change_context_lazy(|| AppError::Unknown)?);
        // pb.enable_steady_tick(Duration::from_secs(1));
        // pb.tick()

        let instant = Instant::now();

        observe::upload_started(&identity, line.key(), total_size);
        let video = uploader
            .upload(client.clone(), limit, |vs| {
                vs.map(|chunk| {
                    let pb = pb.clone();
                    let chunk = chunk?;
                    let len = chunk.len();
                    Ok((Progressbar::new(chunk, pb), len))
                })
            })
            .await
            .inspect_err(|error| observe::standalone::failed(&identity, error))
            .change_context_lazy(|| AppError::Unknown)?;
        pb.finish_and_clear();
        let t = instant.elapsed().as_millis();
        observe::upload_completed(&identity, "transferred", t as u64);
        info!(
            "Upload completed: {file_name} => cost {:.2}s, {:.2} MB/s.",
            t as f64 / 1000.,
            total_size as f64 / 1000. / t as f64
        );

        // 保存断点续传信息
        checkpoint.add_video(video_path, video.clone());
        if let Err(e) = checkpoint.save(&checkpoint_path) {
            warn!("Failed to save checkpoint: {}", e);
        } else {
            info!(
                "Checkpoint saved: {} files uploaded",
                checkpoint.uploaded_files.len()
            );
        }

        videos.push(video);
    }

    // 上传完成后删除断点续传文件
    if checkpoint_path.exists() {
        let _ = std::fs::remove_file(&checkpoint_path);
        info!("All files uploaded successfully, checkpoint removed");
    }

    Ok(videos)
}

pub async fn login_by_password(credential: Credential) -> AppResult<LoginInfo> {
    let username: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("请输入账号")
        .interact()
        .change_context_lazy(|| AppError::Unknown)?;
    let password: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("请输入密码")
        .interact()
        .change_context_lazy(|| AppError::Unknown)?;
    credential
        .login_by_password(&username, &password)
        .await
        .change_context_lazy(|| AppError::Unknown)
}

pub async fn login_by_sms(credential: Credential) -> AppResult<LoginInfo> {
    let country_code: u32 = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("请输入手机国家代码")
        .default(86)
        .interact_text()
        .change_context_lazy(|| AppError::Unknown)?;
    let phone: u64 = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("请输入手机号")
        .interact_text()
        .change_context_lazy(|| AppError::Unknown)?;
    let res = credential
        .send_sms_handle_recaptcha(phone, country_code, |url| async move {
            println!("{url}");
            println!("请复制此链接至浏览器打开并启动开发者工具，完成滑动验证后查看网络请求");

            let challenge: String = Input::with_theme(&ColorfulTheme::default())
                .with_prompt("请输入get.php响应中的challenge值")
                .interact_text()
                .map_err(|e| e.to_string())?;

            let valiate: String = Input::with_theme(&ColorfulTheme::default())
                .with_prompt("请输入ajax.php响应中的validate值")
                .interact_text()
                .map_err(|e| e.to_string())?;

            Ok((challenge, valiate))
        })
        .await
        .change_context_lazy(|| AppError::Unknown)?;
    let input: u32 = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("请输入验证码")
        .interact_text()
        .change_context_lazy(|| AppError::Unknown)?;
    // println!("{}", payload);
    credential
        .login_by_sms(input, res)
        .await
        .change_context_lazy(|| AppError::Unknown)
}

pub async fn login_by_qrcode(credential: Credential) -> AppResult<LoginInfo> {
    let value = credential
        .get_qrcode()
        .await
        .change_context_lazy(|| AppError::Unknown)?;
    let code = QrCode::new(
        value["data"]["url"]
            .as_str()
            .unwrap()
            .replace("https", "http"),
    )
    .unwrap();
    let image = code
        .render::<unicode::Dense1x2>()
        .dark_color(unicode::Dense1x2::Light)
        .light_color(unicode::Dense1x2::Dark)
        .build();
    println!("{}", image);
    // Render the bits into an image.
    let image = code.render::<Luma<u8>>().build();
    println!(
        "在Windows下建议使用Windows Terminal(支持utf8，可完整显示二维码)\n否则可能无法正常显示，此时请打开./qrcode.png扫码"
    );
    // Save the image.
    image.save("qrcode.png").unwrap();
    credential
        .login_by_qrcode(value)
        .await
        .change_context_lazy(|| AppError::Unknown)
}

pub async fn login_by_browser(credential: Credential) -> AppResult<LoginInfo> {
    let value = credential
        .get_qrcode()
        .await
        .change_context_lazy(|| AppError::Unknown)?;
    println!(
        "{}",
        value["data"]["url"]
            .as_str()
            .ok_or_else(|| AppError::Custom(value.to_string()))?
    );
    println!("请复制此链接至浏览器中完成登录");
    credential
        .login_by_qrcode(value)
        .await
        .change_context_lazy(|| AppError::Unknown)
}

pub async fn login_by_web_cookies(credential: Credential) -> AppResult<LoginInfo> {
    let sess_data: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("请输入SESSDATA")
        .interact_text()
        .change_context_lazy(|| AppError::Unknown)?;
    let bili_jct: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("请输入bili_jct")
        .interact_text()
        .change_context_lazy(|| AppError::Unknown)?;
    credential
        .login_by_web_cookies(&sess_data, &bili_jct)
        .await
        .change_context_lazy(|| AppError::Unknown)
}

pub async fn login_by_webqr_cookies(credential: Credential) -> AppResult<LoginInfo> {
    let sess_data: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("请输入SESSDATA")
        .interact_text()
        .change_context_lazy(|| AppError::Unknown)?;
    let dede_user_id: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("请输入DedeUserID")
        .interact_text()
        .change_context_lazy(|| AppError::Unknown)?;
    credential
        .login_by_web_qrcode(&sess_data, &dede_user_id)
        .await
        .change_context_lazy(|| AppError::Unknown)
}

impl From<Progressbar> for Body {
    fn from(async_stream: Progressbar) -> Self {
        Body::wrap_stream(async_stream)
    }
}

#[inline]
pub fn fopen_rw<P: AsRef<Path>>(path: P) -> AppResult<std::fs::File> {
    let path = path.as_ref();
    std::fs::File::options()
        .read(true)
        .write(true)
        .open(path)
        .change_context_lazy(|| {
            AppError::Custom(String::from("open cookies file: ") + &path.to_string_lossy())
        })
}

#[derive(Clone)]
struct Progressbar {
    bytes: Bytes,
    pb: ProgressBar,
}

impl Progressbar {
    pub fn new(bytes: Bytes, pb: ProgressBar) -> Self {
        Self { bytes, pb }
    }

    pub fn progress(&mut self) -> AppResult<Option<Bytes>> {
        let pb = &self.pb;

        let content_bytes = &mut self.bytes;

        let n = content_bytes.remaining();

        let pc = 4096;
        if n == 0 {
            Ok(None)
        } else if n < pc {
            pb.inc(n as u64);
            Ok(Some(content_bytes.copy_to_bytes(n)))
        } else {
            pb.inc(pc as u64);

            Ok(Some(content_bytes.copy_to_bytes(pc)))
        }
    }
}

impl Stream for Progressbar {
    type Item = AppResult<Bytes>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        match self.progress()? {
            None => Poll::Ready(None),
            Some(s) => Poll::Ready(Some(Ok(s))),
        }
    }
}

#[cfg(test)]
mod append_tests {
    use super::replace_index;

    #[test]
    fn part_numbers_are_one_based() {
        assert_eq!(replace_index(1, 3), Ok(0));
        assert_eq!(replace_index(3, 3), Ok(2));
    }

    /// 越界要说清楚稿件到底有几个分P——写错序号是这条流程最容易犯的错，
    /// 而它作用在一个真实稿件上。
    #[test]
    fn out_of_range_says_how_many_parts_there_are() {
        let error = replace_index(4, 3).unwrap_err();
        assert!(error.contains("只有 3 个分P"), "{error}");
        assert!(replace_index(0, 3).is_err(), "0 不是合法分P序号");
    }
}
