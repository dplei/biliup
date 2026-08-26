use crate::server::common::cookie_health;
use crate::server::common::route_health::{HealthUpdate, RouteHealthState, RouteSelection};
use crate::server::common::segment_enrollment::{
    EnrollmentOutcome, EnrollmentRequest, EnrollmentStore, enroll_validated_segment,
    normalize_segment_path,
};
use crate::server::common::upload::UploaderMessage;
use crate::server::common::util::{FileValidator, MediaValidation};
use crate::server::core::downloader::cover_downloader;
use crate::server::core::downloader::{
    DanmakuClient, DownloadStatus, DownloaderRuntime, SegmentEvent, SegmentInfo,
};
use crate::server::core::live::{danmaku_client, downloader_runtime, live_request};
use crate::server::core::monitor::Monitor;
use crate::server::errors::{AppError, AppResult};
use crate::server::infrastructure::context::{Context, Stage, WorkerStatus};
use crate::server::infrastructure::models::hook_step::process;
use async_channel::Sender;
use biliup::downloader::live::{LivePlugin, LiveStatus, LiveStream};
use error_stack::{ResultExt, bail};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

const RETRY_BASE_DELAY: Duration = Duration::from_secs(2);
const RETRY_MAX_DELAY: Duration = Duration::from_secs(30);
const UPLOAD_SEGMENT_QUEUE_CAPACITY: usize = 64;

#[derive(Debug, Default)]
struct OfflineRetryState {
    offline_since: Option<Instant>,
    offline_retry_count: u32,
}

impl OfflineRetryState {
    fn record_live(&mut self) {
        self.offline_since = None;
        self.offline_retry_count = 0;
    }

    fn record_unavailable(&mut self, now: Instant, grace: Duration) -> bool {
        let since = *self.offline_since.get_or_insert(now);
        self.offline_retry_count = self.offline_retry_count.saturating_add(1);
        now.saturating_duration_since(since) >= grace
    }

    /// 保持阶段 2 之前的下播宽限复查间隔：首次不可用后等待 4 秒。
    fn retry_delay(&self) -> Duration {
        RETRY_BASE_DELAY
            .saturating_mul(2_u32.saturating_pow(self.offline_retry_count.min(5)))
            .min(RETRY_MAX_DELAY)
    }
}

fn exponential_backoff(failure_count: u32) -> Duration {
    RETRY_BASE_DELAY
        .saturating_mul(2_u32.saturating_pow(failure_count.saturating_sub(1).min(5)))
        .min(RETRY_MAX_DELAY)
}

struct DownloadAttempt {
    result: AppResult<DownloadStatus>,
    connected_for: Duration,
    completed_configured_segment: bool,
}

/// 分段事件处理器
pub struct SegmentEventProcessor {
    channel: Option<Sender<SegmentInfo>>,
    uploader: Sender<UploaderMessage>,
    ctx: Context,
    file_validator: FileValidator,
    preserve_recoverable_short_segments: bool,
    recovery_batch_max_files: usize,
    recovery_retry_interval: Duration,
    pending_short_segments: Vec<SegmentInfo>,
    stats: SegmentProcessingStats,
}

#[derive(Debug, Default, Clone, Copy)]
struct SegmentProcessingStats {
    valid_segments: u64,
    recoverable_short_segments: u64,
    recoverable_short_bytes: u64,
    merged_recovery_outputs: u64,
    deferred_recovery_batches: u64,
    invalid_segments: u64,
    segments_queued_for_upload: u64,
    upload_queue_peak_depth: usize,
}

impl SegmentEventProcessor {
    /// 创建处理器
    pub fn new(uploader: Sender<UploaderMessage>, ctx: Context) -> Self {
        let config = ctx.config();
        Self {
            channel: None,
            uploader,
            file_validator: FileValidator::new(config.filtering_threshold * 1000 * 1000, true),
            preserve_recoverable_short_segments: config.preserve_recoverable_short_segments,
            recovery_batch_max_files: config.recoverable_short_batch_max_files.max(1),
            recovery_retry_interval: Duration::from_secs(
                config.recoverable_short_retry_interval_secs,
            ),
            pending_short_segments: Vec::new(),
            stats: SegmentProcessingStats::default(),
            ctx,
        }
    }

    /// 处理分段事件
    pub async fn process(&mut self, mut event: SegmentInfo) -> AppResult<()> {
        let file_bytes = std::fs::metadata(&event.prev_file_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        // 删除决定只能发生在媒体内容探测之后；体积本身不再代表文件无效。
        match self.file_validator.validate(&event.prev_file_path)? {
            MediaValidation::Valid => {
                self.stats.valid_segments += 1;
                self.flush_pending_short_segments().await?;
                self.enqueue_validated(&mut event, file_bytes).await
            }
            MediaValidation::RecoverableShort {
                duration,
                first_media_timestamp_ms,
                last_media_timestamp_ms,
            } => {
                if self.preserve_recoverable_short_segments {
                    self.stats.recoverable_short_segments += 1;
                    self.stats.recoverable_short_bytes = self
                        .stats
                        .recoverable_short_bytes
                        .saturating_add(file_bytes);
                    warn!(
                        file = %event.prev_file_path.display(),
                        file_bytes,
                        media_duration_ms = duration.map(|duration| duration.as_millis() as u64),
                        first_media_timestamp_ms,
                        last_media_timestamp_ms,
                        close_reason = ?event.close_reason,
                        attempt_id = event.attempt_id.as_deref().unwrap_or("untracked"),
                        "queueing recoverable short media segment"
                    );
                    self.pending_short_segments.push(event);
                    Ok(())
                } else {
                    self.remove_invalid_segment(
                        &event.prev_file_path,
                        "recoverable short segment rejected by rollback configuration",
                    );
                    Ok(())
                }
            }
            MediaValidation::Invalid { reason } => {
                self.stats.invalid_segments += 1;
                warn!(
                    file = %event.prev_file_path.display(),
                    file_bytes,
                    reason = ?reason,
                    close_reason = ?event.close_reason,
                    attempt_id = event.attempt_id.as_deref().unwrap_or("untracked"),
                    "discarding invalid media segment"
                );
                self.remove_invalid_segment(&event.prev_file_path, &format!("{reason:?}"));
                Ok(())
            }
        }
    }

    pub async fn finish(&mut self) -> AppResult<()> {
        self.flush_pending_short_segments().await
    }

    async fn flush_pending_short_segments(&mut self) -> AppResult<()> {
        if self.pending_short_segments.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.pending_short_segments);
        for compatible_group in compatible_segment_groups(pending) {
            for chunk in compatible_group.chunks(self.recovery_batch_max_files) {
                let group = chunk.to_vec();
                if group.len() > 1 {
                    match merge_compatible_segments(&group, &self.file_validator) {
                        Ok(merged) => {
                            self.stats.merged_recovery_outputs += 1;
                            let original_files: Vec<_> = group
                                .iter()
                                .map(|event| event.prev_file_path.display().to_string())
                                .collect();
                            info!(
                                output = %merged.prev_file_path.display(),
                                originals = ?original_files,
                                "merged compatible recoverable short segments; originals retained"
                            );
                            let mut merged = merged;
                            let bytes = std::fs::metadata(&merged.prev_file_path)
                                .map(|metadata| metadata.len())
                                .unwrap_or(0);
                            self.enqueue_validated(&mut merged, bytes).await?;
                            continue;
                        }
                        Err(error) => {
                            let (batch_id, manifest) = defer_recovery_batch(
                                &group,
                                &format!("{error:?}"),
                                self.recovery_retry_interval,
                            )?;
                            self.stats.deferred_recovery_batches += 1;
                            self.queue_deferred_batch_record(&batch_id, &manifest);
                            warn!(
                                recovery_batch_id = batch_id,
                                manifest = %manifest.display(),
                                error = ?error,
                                file_count = group.len(),
                                "failed to merge recoverable segments; deferred batch without uploading originals"
                            );
                            continue;
                        }
                    }
                }
                let (batch_id, manifest) = defer_recovery_batch(
                    &group,
                    "media parameters are not compatible with an adjacent recovery group",
                    self.recovery_retry_interval,
                )?;
                self.stats.deferred_recovery_batches += 1;
                self.queue_deferred_batch_record(&batch_id, &manifest);
                warn!(
                    recovery_batch_id = batch_id,
                    manifest = %manifest.display(),
                    file_count = group.len(),
                    "recoverable segment deferred without immediate upload"
                );
            }
        }
        Ok(())
    }

    async fn enqueue_validated(
        &mut self,
        event: &mut SegmentInfo,
        total_bytes: u64,
    ) -> AppResult<()> {
        if self.ctx.upload_config().is_some() {
            let normalized_file_path = normalize_segment_path(&event.prev_file_path)?;
            let request = EnrollmentRequest {
                live_streamer_id: self.ctx.worker_id(),
                streamer_info_id: self.ctx.id(),
                file_path: event.prev_file_path.clone(),
                normalized_file_path,
                danmaku_file_path: event.danmaku_file_path.clone(),
                total_bytes,
                now: chrono::Utc::now(),
                recovery_window_minutes: self.ctx.config().recovery_window_minutes.unwrap_or(30)
                    as i64,
            };
            let store = EnrollmentStore::production(self.ctx.pool().clone());
            match enroll_validated_segment(&store, &request).await? {
                EnrollmentOutcome::Enrolled(enrollment) => {
                    info!(
                        file = %event.prev_file_path.display(),
                        missing_id = enrollment.missing_id,
                        session = enrollment.upload_session_id,
                        segment_order = enrollment.segment_order,
                        duplicate = enrollment.duplicate,
                        close_reason = ?event.close_reason,
                        attempt_id = event.attempt_id.as_deref().unwrap_or("untracked"),
                        "validated and enrolled media segment"
                    );
                    if enrollment.duplicate {
                        return Ok(());
                    }
                    event.enrollment = Some(enrollment);
                }
                EnrollmentOutcome::Outboxed(manifest) => {
                    warn!(
                        file = %event.prev_file_path.display(),
                        manifest = %manifest.display(),
                        "validated media segment durably outboxed; upload deferred until database import"
                    );
                    return Ok(());
                }
            }
        } else {
            sqlx::query(
                "INSERT INTO filelist (file, streamer_info_id) \
                 SELECT ?1, ?2 WHERE NOT EXISTS \
                 (SELECT 1 FROM filelist WHERE file = ?1 AND streamer_info_id = ?2)",
            )
            .bind(event.prev_file_path.display().to_string())
            .bind(self.ctx.id())
            .execute(self.ctx.pool())
            .await
            .change_context(AppError::Unknown)?;
            info!(
                file = %event.prev_file_path.display(),
                "validated and indexed record-only media segment"
            );
        }
        self.enqueue(event.clone())
    }

    fn enqueue(&mut self, event: SegmentInfo) -> AppResult<()> {
        // 上一轮 process_with_upload 可能因上传失败提前返回，UActor 已 drop rx，
        // 这里挂着的 tx 是死的；丢弃后下面会重建一条新的管道。
        if let Some(tx) = &self.channel
            && tx.is_closed()
        {
            warn!(
                url = self.ctx.live_streamer().url,
                "upload channel closed by uploader, reopening"
            );
            self.channel = None;
        }

        match &self.channel {
            None => {
                let (tx, rx) = async_channel::bounded(UPLOAD_SEGMENT_QUEUE_CAPACITY);

                // 发送到上传器
                if let Err(error) = self
                    .uploader
                    .try_send(UploaderMessage::SegmentEvent(rx.clone(), self.ctx.clone()))
                {
                    warn!(?error, "upload actor queue unavailable; deferring segment");
                    return self.defer_for_backpressure(event, "upload actor queue unavailable");
                }

                // 发送到缓冲区
                if let Err(error) = tx.try_send(event) {
                    warn!(
                        ?error,
                        "new upload segment queue unexpectedly unavailable; deferring"
                    );
                    let event = error.into_inner();
                    return self
                        .defer_for_backpressure(event, "new upload segment queue unavailable");
                }
                self.channel = Some(tx);
            }
            Some(tx) => {
                if let Err(error) = tx.try_send(event) {
                    warn!(
                        capacity = UPLOAD_SEGMENT_QUEUE_CAPACITY,
                        ?error,
                        "upload segment queue reached its bound; deferring without data loss"
                    );
                    let event = error.into_inner();
                    return self.defer_for_backpressure(
                        event,
                        "upload segment queue full during rate-limit/backlog window",
                    );
                }
            }
        }

        self.stats.segments_queued_for_upload += 1;
        if let Some(tx) = &self.channel {
            self.stats.upload_queue_peak_depth = self.stats.upload_queue_peak_depth.max(tx.len());
        }
        Ok(())
    }

    fn defer_for_backpressure(&mut self, event: SegmentInfo, reason: &str) -> AppResult<()> {
        self.stats.upload_queue_peak_depth = self
            .stats
            .upload_queue_peak_depth
            .max(UPLOAD_SEGMENT_QUEUE_CAPACITY);
        if let Some(enrollment) = &event.enrollment {
            warn!(
                missing_id = enrollment.missing_id,
                session = enrollment.upload_session_id,
                file = %event.prev_file_path.display(),
                reason,
                "upload queue unavailable; durable lifecycle row remains pending"
            );
            return Ok(());
        }
        let (batch_id, manifest) = defer_recovery_batch(
            std::slice::from_ref(&event),
            reason,
            self.recovery_retry_interval,
        )?;
        self.stats.deferred_recovery_batches += 1;
        self.queue_deferred_batch_record(&batch_id, &manifest);
        warn!(
            recovery_batch_id = batch_id,
            manifest = %manifest.display(),
            file = %event.prev_file_path.display(),
            reason,
            "segment deferred because bounded upload queue was unavailable"
        );
        Ok(())
    }

    fn queue_deferred_batch_record(&self, batch_id: &str, manifest_path: &std::path::Path) {
        if let Err(error) = self
            .uploader
            .try_send(UploaderMessage::RecoveryBatchDeferred {
                ctx: self.ctx.clone(),
                batch_id: batch_id.to_string(),
                manifest_path: manifest_path.to_path_buf(),
            })
        {
            // The fsynced manifest is the durability boundary. A full actor queue may delay
            // database indexing, but must never fall back to uploading the originals.
            warn!(
                recovery_batch_id = batch_id,
                manifest = %manifest_path.display(),
                ?error,
                "deferred recovery manifest persisted but database indexing was not queued"
            );
        }
    }

    fn remove_invalid_segment(&self, path: &std::path::Path, reason: &str) {
        let path = path.to_owned();
        let reason = reason.to_string();
        tokio::spawn(async move {
            match crate::server::infrastructure::models::hook_step::HookStep::remove_file(&[&path])
                .await
            {
                Ok(()) => info!(file = %path.display(), reason, "removed invalid media segment"),
                Err(error) => error!(
                    file = %path.display(),
                    reason,
                    error = ?error,
                    "failed to remove invalid media segment; original preserved"
                ),
            }
        });
    }
}

fn compatible_segment_groups(events: Vec<SegmentInfo>) -> Vec<Vec<SegmentInfo>> {
    let mut groups: Vec<(Option<u64>, Vec<SegmentInfo>)> = Vec::new();
    for event in events {
        let key = media_compatibility_key(&event);
        let joins_previous = key.is_some()
            && groups
                .last()
                .is_some_and(|(previous_key, _)| previous_key == &key);
        if joins_previous {
            groups.last_mut().expect("group exists").1.push(event);
        } else {
            groups.push((key, vec![event]));
        }
    }
    groups.into_iter().map(|(_, group)| group).collect()
}

fn defer_recovery_batch(
    events: &[SegmentInfo],
    error: &str,
    retry_interval: Duration,
) -> AppResult<(String, std::path::PathBuf)> {
    let first = events
        .first()
        .ok_or_else(|| AppError::Custom("cannot defer an empty recovery batch".into()))?;
    let created_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let batch_id = format!(
        "{}-{}-{}",
        std::process::id(),
        first.segment_index,
        created_ms
    );
    let parent = first
        .prev_file_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let manifest_path = parent.join(format!(".biliup-recovery-{batch_id}.json"));
    let temp_path = parent.join(format!(".biliup-recovery-{batch_id}.json.tmp"));
    let files: Vec<_> = events
        .iter()
        .flat_map(|event| {
            std::iter::once(&event.prev_file_path).chain(event.recovery_source_paths.iter())
        })
        .map(|path| path.display().to_string())
        .collect();
    let manifest = serde_json::json!({
        "version": 1,
        "recovery_batch_id": batch_id,
        "state": "Deferred",
        "created_at_ms": created_ms,
        "next_retry_at_ms": created_ms.saturating_add(retry_interval.as_millis()),
        "files": files,
        "last_error": bounded_diagnostic(error, 4096),
    });
    let bytes = serde_json::to_vec_pretty(&manifest).change_context(AppError::Unknown)?;
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp_path)
        .change_context(AppError::Unknown)?;
    use std::io::Write;
    file.write_all(&bytes).change_context(AppError::Unknown)?;
    file.sync_all().change_context(AppError::Unknown)?;
    std::fs::rename(&temp_path, &manifest_path).change_context(AppError::Unknown)?;
    Ok((batch_id, manifest_path))
}

fn bounded_diagnostic(value: &str, max_chars: usize) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}

fn media_compatibility_key(event: &SegmentInfo) -> Option<u64> {
    if event.danmaku_file_path.is_some()
        || !event
            .prev_file_path
            .extension()?
            .to_str()?
            .eq_ignore_ascii_case("flv")
    {
        return None;
    }
    let bytes = std::fs::read(&event.prev_file_path).ok()?;
    if bytes.len() <= 13 || bytes.get(..3) != Some(b"FLV") {
        return None;
    }
    let mut offset = u32::from_be_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]) as usize + 4;
    let mut headers: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut has_media = false;
    let mut has_video_sequence = false;
    let mut has_video_keyframe = false;
    while offset < bytes.len() {
        if bytes.len().saturating_sub(offset) < 15 {
            return None;
        }
        let tag_type = bytes[offset];
        let size = ((bytes[offset + 1] as usize) << 16)
            | ((bytes[offset + 2] as usize) << 8)
            | bytes[offset + 3] as usize;
        let body_start = offset + 11;
        let body_end = body_start.checked_add(size)?;
        let next = body_end.checked_add(4)?;
        if next > bytes.len() {
            return None;
        }
        let body = &bytes[body_start..body_end];
        let is_sequence_header = match tag_type {
            8 => body.len() > 2 && (body[0] >> 4) == 10 && body[1] == 0,
            9 => body.len() > 5 && (body[0] & 0x0f) == 7 && body[1] == 0,
            _ => false,
        };
        if is_sequence_header {
            has_video_sequence |= tag_type == 9;
            headers.push((tag_type, body.to_vec()));
        }
        has_media |= match tag_type {
            8 if !body.is_empty() => (body[0] >> 4) != 10 || body.get(1) == Some(&1),
            9 if !body.is_empty() => (body[0] & 0x0f) != 7 || body.get(1) == Some(&1),
            _ => false,
        };
        has_video_keyframe |= tag_type == 9
            && body.len() > 1
            && (body[0] >> 4) == 1
            && ((body[0] & 0x0f) != 7 || body[1] == 1);
        offset = next;
    }
    if headers.is_empty() || !has_media || (has_video_sequence && !has_video_keyframe) {
        return None;
    }
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    headers.hash(&mut hasher);
    Some(hasher.finish())
}

fn merge_compatible_segments(
    events: &[SegmentInfo],
    validator: &FileValidator,
) -> AppResult<SegmentInfo> {
    let first = events
        .first()
        .ok_or_else(|| AppError::Custom("cannot merge an empty segment group".into()))?;
    let last = events.last().expect("non-empty segment group");
    let parent = first
        .prev_file_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let stem = first
        .prev_file_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("segment");
    let suffix = format!("{}-{}", std::process::id(), first.segment_index);
    let list_path = parent.join(format!(".{stem}.{suffix}.concat.txt"));
    let copy_output_path = parent.join(format!("{stem}.{suffix}.recovered.flv"));
    write_concat_list(
        &list_path,
        events.iter().map(|event| event.prev_file_path.as_path()),
    )?;
    let started_at = Instant::now();
    let copy_output = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "warning",
            "-y",
            "-fflags",
            "+genpts+igndts",
            "-f",
            "concat",
            "-safe",
            "0",
            "-i",
        ])
        .arg(&list_path)
        .args(["-c", "copy", "-avoid_negative_ts", "make_zero"])
        .arg(&copy_output_path)
        .output()
        .change_context(AppError::Custom(
            "failed to spawn ffmpeg for short segment merge".into(),
        ))?;
    let copy_diagnostic = bounded_diagnostic(&String::from_utf8_lossy(&copy_output.stderr), 4096);
    let output_path = if copy_output.status.success() {
        let _ = std::fs::remove_file(&list_path);
        info!(
            phase = "concat_copy",
            elapsed_ms = started_at.elapsed().as_millis(),
            stderr = copy_diagnostic,
            "short segment merge phase succeeded"
        );
        copy_output_path
    } else {
        let _ = std::fs::remove_file(&copy_output_path);
        warn!(
            phase = "concat_copy",
            status = %copy_output.status,
            elapsed_ms = started_at.elapsed().as_millis(),
            stderr = copy_diagnostic,
            "short segment concat copy failed; trying per-file remux"
        );
        remux_then_concat(events, parent, stem, &suffix, &list_path)?
    };
    if matches!(
        validator.validate(&output_path)?,
        MediaValidation::Invalid { .. }
    ) {
        let _ = std::fs::remove_file(&output_path);
        bail!(AppError::Custom(
            "merged short segment failed media validation".into()
        ));
    }
    Ok(SegmentInfo {
        prev_file_path: output_path,
        danmaku_file_path: None,
        next_file_path: None,
        segment_index: first.segment_index,
        close_reason: last.close_reason,
        attempt_id: first.attempt_id.clone(),
        recovery_source_paths: events
            .iter()
            .map(|event| event.prev_file_path.clone())
            .collect(),
        enrollment: None,
    })
}

fn write_concat_list<'a>(
    list_path: &std::path::Path,
    paths: impl IntoIterator<Item = &'a std::path::Path>,
) -> AppResult<()> {
    let mut concat_list = String::new();
    for path in paths {
        let absolute = path.canonicalize().change_context(AppError::Unknown)?;
        let escaped = absolute.to_string_lossy().replace('\'', "'\\''");
        concat_list.push_str(&format!("file '{escaped}'\n"));
    }
    std::fs::write(list_path, concat_list).change_context(AppError::Unknown)
}

fn remux_then_concat(
    events: &[SegmentInfo],
    parent: &std::path::Path,
    stem: &str,
    suffix: &str,
    list_path: &std::path::Path,
) -> AppResult<std::path::PathBuf> {
    let started_at = Instant::now();
    let mut normalized = Vec::with_capacity(events.len());
    let mut diagnostics = Vec::new();
    for (index, event) in events.iter().enumerate() {
        let path = parent.join(format!(".{stem}.{suffix}.{index}.normalized.mkv"));
        let output = std::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "warning",
                "-y",
                "-fflags",
                "+genpts+igndts",
                "-i",
            ])
            .arg(&event.prev_file_path)
            .args([
                "-map",
                "0:v?",
                "-map",
                "0:a?",
                "-c",
                "copy",
                "-avoid_negative_ts",
                "make_zero",
            ])
            .arg(&path)
            .output()
            .change_context(AppError::Custom(
                "failed to spawn ffmpeg for recovery remux".into(),
            ))?;
        diagnostics.push(bounded_diagnostic(
            &String::from_utf8_lossy(&output.stderr),
            1024,
        ));
        if !output.status.success() {
            for temporary in normalized.iter().chain(std::iter::once(&path)) {
                let _ = std::fs::remove_file(temporary);
            }
            let _ = std::fs::remove_file(list_path);
            bail!(AppError::Custom(format!(
                "ffmpeg short segment merge failed at remux_input_{index} with {}; elapsed_ms={}; stderr={}",
                output.status,
                started_at.elapsed().as_millis(),
                diagnostics.last().cloned().unwrap_or_default(),
            )));
        }
        normalized.push(path);
    }

    write_concat_list(
        list_path,
        normalized.iter().map(std::path::PathBuf::as_path),
    )?;
    let output_path = parent.join(format!("{stem}.{suffix}.recovered.mkv"));
    let output = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "warning",
            "-y",
            "-f",
            "concat",
            "-safe",
            "0",
            "-i",
        ])
        .arg(list_path)
        .args(["-c", "copy"])
        .arg(&output_path)
        .output()
        .change_context(AppError::Custom(
            "failed to spawn ffmpeg for normalized recovery concat".into(),
        ))?;
    let stderr = bounded_diagnostic(&String::from_utf8_lossy(&output.stderr), 4096);
    for temporary in &normalized {
        let _ = std::fs::remove_file(temporary);
    }
    let _ = std::fs::remove_file(list_path);
    if !output.status.success() {
        let _ = std::fs::remove_file(&output_path);
        bail!(AppError::Custom(format!(
            "ffmpeg short segment merge failed at remux_concat with {}; elapsed_ms={}; stderr={stderr}",
            output.status,
            started_at.elapsed().as_millis(),
        )));
    }
    info!(
        phase = "remux_concat",
        elapsed_ms = started_at.elapsed().as_millis(),
        stderr,
        "short segment merge phase succeeded"
    );
    Ok(output_path)
}

/// 下载任务
pub struct DownloadTask {
    token: CancellationToken,
    done_notify: Notify,
    downloader: DownloaderRuntime,
}

impl DownloadTask {
    pub fn new(downloader: DownloaderRuntime) -> Self {
        Self {
            token: CancellationToken::new(),
            done_notify: Notify::new(),
            downloader,
        }
    }

    pub(self) async fn execute(
        &self,
        ctx: &Context,
        sender: Sender<UploaderMessage>,
        plugin: Arc<dyn LivePlugin + Send + Sync>,
        rooms_handle: Arc<Monitor>,
    ) -> AppResult<()> {
        // 下播确认 / 重连配置
        // grace = 下播宽限期（config.delay 秒）：流中断后持续复查重连，直到「确实连续离线
        // 超过 grace」才判定真下播 → 投稿。避免抖音等 flv 短暂中断（CDN/签名轮换）被当成
        // 下播，结果一场直播被切成多个稿件。grace=0（默认）→ 一离线立即结束，保持老行为。
        let grace = Duration::from_secs(ctx.config().delay);
        let route_health_enabled = ctx.config().route_health_enabled;
        let mut route_health = RouteHealthState::new(route_health_enabled);
        let mut offline_retry = OfflineRetryState::default();
        let url = ctx.live_streamer().url.clone();
        // cookie 健康监测：录制中断流复查时同样喂给健康统计（录制时检测）
        let platform = plugin.name();
        let cookie_webhook = ctx.config().cookie_health_webhook.clone();
        let mut stream = ctx.live_stream().clone();
        let filename_prefix = ctx
            .live_streamer()
            .filename_prefix
            .clone()
            .or_else(|| ctx.config().filename_prefix.clone());
        let danmaku_client = danmaku_client(
            stream.danmaku.as_ref(),
            filename_prefix.as_deref(),
            &stream.name,
        );
        // 启动弹幕客户端
        if let Some(ref client) = danmaku_client {
            // 启动弹幕下载逻辑
            info!("Starting danmaku client for stream: {}", url);
            client.download().await?;
        }

        // 初始化组件
        let mut processor = SegmentEventProcessor::new(sender, ctx.clone());
        let failover_enabled = platform == "douyin"
            && ctx.config().douyin_route_failover.unwrap_or(false)
            && route_health_enabled;
        let mut can_download = true;
        let mut route_failure_count = 0_u32;
        let mut estimated_missing = Duration::ZERO;
        let mut result = Ok(DownloadStatus::StreamEnded);
        loop {
            let attempt = if can_download {
                route_health.begin_attempt(&stream);
                let attempt = self
                    .download(&mut processor, ctx.clone(), danmaku_client.clone(), &stream)
                    .await;
                result = attempt.result;
                info!("initialize_components completed: {url}");
                Some((attempt.connected_for, attempt.completed_configured_segment))
            } else {
                None
            };
            let interrupted = attempt.is_some()
                && result
                    .as_ref()
                    .map_or(true, DownloadStatus::is_transport_failure);

            if self.token.is_cancelled() {
                info!(url = url, "task is cancelled");
                break;
            }
            // 检查流状态
            let check_started = std::time::Instant::now();
            let check_result = plugin.check_stream(live_request(ctx.worker())).await;
            let check_elapsed = check_started.elapsed();
            let mut confirmed_live = false;
            let backoff = match check_result {
                Ok(LiveStatus::Live {
                    stream: next_stream,
                }) => {
                    confirmed_live = true;
                    cookie_health::record_success(platform, cookie_webhook.as_deref());
                    offline_retry.record_live();
                    if let Some((connected_for, completed_configured_segment)) = attempt {
                        let health_update = route_health.observe_live_attempt(
                            result.as_ref().ok(),
                            connected_for,
                            completed_configured_segment,
                            Instant::now(),
                        );
                        match health_update {
                            HealthUpdate::Failure {
                                ref key,
                                failures,
                                circuit_opened,
                                alert,
                            } => {
                                route_failure_count = failures;
                                warn!(
                                    url = url,
                                    failures,
                                    circuit_opened,
                                    host = key.host.as_deref().unwrap_or("unknown"),
                                    protocol = key.protocol,
                                    quality = key.quality.as_deref().unwrap_or("unknown"),
                                    codec = key.codec.as_deref().unwrap_or("unknown"),
                                    "stream is live but route transport failed"
                                );
                                if alert && failover_enabled {
                                    cookie_health::notify_alert(
                                        cookie_webhook.as_deref(),
                                        "⚠️ 直播拉流线路故障，正在自动切换",
                                        &format!(
                                            "{}：当前 {} / {} / {} 线路连续失败，已熔断并尝试备用线路。后续同一轮故障不再重复告警。",
                                            ctx.live_streamer().remark,
                                            key.host.as_deref().unwrap_or("unknown"),
                                            key.protocol,
                                            key.quality.as_deref().unwrap_or("unknown"),
                                        ),
                                    );
                                }
                            }
                            HealthUpdate::AuthRefresh { ref key } => info!(
                                url = url,
                                host = key.host.as_deref().unwrap_or("unknown"),
                                protocol = key.protocol,
                                "stream authorization failed; refreshed signed candidates before counting route failure"
                            ),
                            HealthUpdate::Recovered { ref key } => {
                                route_failure_count = 0;
                                info!(
                                    url = url,
                                    connected_for = ?connected_for,
                                    host = key.host.as_deref().unwrap_or("unknown"),
                                    protocol = key.protocol,
                                    "stream route recovered after stable download"
                                );
                            }
                            HealthUpdate::Unchanged => {}
                        }
                    }
                    let previous_quality = stream.recording_quality.clone();
                    stream = *next_stream;
                    // 上一次 download future 已返回并关闭 LifecycleFile；这里才改写候选，
                    // 下一轮必然重新创建媒体文件，不会在同一分段内混接 Host/协议/Codec。
                    let selection =
                        route_health.select_route(&mut stream, Instant::now(), failover_enabled);
                    let (route_changed, selection_backoff) = match selection {
                        RouteSelection::Selected { ref key, changed } => {
                            can_download = true;
                            if changed {
                                info!(
                                    url = url,
                                    host = key.host.as_deref().unwrap_or("unknown"),
                                    protocol = key.protocol,
                                    quality = key.quality.as_deref().unwrap_or("unknown"),
                                    codec = key.codec.as_deref().unwrap_or("unknown"),
                                    "selected a different healthy stream route"
                                );
                            }
                            (changed, None)
                        }
                        RouteSelection::Unavailable { retry_after } => {
                            can_download = false;
                            warn!(
                                url = url,
                                retry_after = ?retry_after,
                                "all refreshed stream routes are cooling down; checking again after backoff"
                            );
                            (false, Some(retry_after.min(RETRY_MAX_DELAY)))
                        }
                    };
                    ctx.worker()
                        .set_recording_quality(stream.recording_quality.clone());
                    info!(url = url, check_elapsed = ?check_elapsed, "Stream is still live, continuing same session");
                    if previous_quality != stream.recording_quality
                        && stream.platform == "douyin"
                        && stream.recording_quality.is_some()
                    {
                        notify_douyin_quality_fallback(ctx, stream.recording_quality.as_deref());
                    }
                    if let Some(delay) = selection_backoff {
                        delay
                    } else if route_changed {
                        Duration::ZERO
                    } else if route_health_enabled {
                        exponential_backoff(route_failure_count.max(1))
                    } else {
                        // 回滚开关：保留旧流程中 Live 后固定 2 秒重试的行为。
                        RETRY_BASE_DELAY
                    }
                }
                Ok(LiveStatus::Offline) => {
                    // 下播是一次成功的检查（cookie 正常）
                    cookie_health::record_success(platform, cookie_webhook.as_deref());
                    let now = Instant::now();
                    if offline_retry.record_unavailable(now, grace) {
                        info!(
                            url = url,
                            check_elapsed = ?check_elapsed,
                            "连续离线超过宽限期 {:?}，确认下播，结束本场", grace
                        );
                        break;
                    }
                    let since = offline_retry.offline_since.expect("offline timestamp set");
                    info!(
                        url = url,
                        check_elapsed = ?check_elapsed,
                        "Stream went offline，宽限期内继续复查 ({:?}/{:?})",
                        now.saturating_duration_since(since),
                        grace
                    );
                    offline_retry.retry_delay()
                }
                Err(e) => {
                    let sanitized_error = cookie_health::redact_sensitive(&format!("{e:?}"));
                    // 健康模块会区分鉴权、网络、服务端和响应结构错误。
                    cookie_health::record_error(
                        platform,
                        &sanitized_error,
                        cookie_webhook.as_deref(),
                    );
                    let now = Instant::now();
                    if offline_retry.record_unavailable(now, grace) {
                        warn!(
                            url = url,
                            check_elapsed = ?check_elapsed,
                            "检查直播间持续失败超过宽限期 {:?}，结束本场: {}", grace, sanitized_error
                        );
                        break;
                    }
                    let since = offline_retry.offline_since.expect("offline timestamp set");
                    warn!(
                        url = url,
                        check_elapsed = ?check_elapsed,
                        "Failed to check stream status: {}，宽限期内继续复查 ({:?}/{:?})",
                        sanitized_error,
                        now.saturating_duration_since(since),
                        grace
                    );
                    offline_retry.retry_delay()
                }
            };

            if confirmed_live && (interrupted || !can_download) {
                estimated_missing = estimated_missing
                    .saturating_add(check_elapsed)
                    .saturating_add(backoff);
            }

            info!("Retrying download in {:?}...", backoff);
            if !backoff.is_zero() {
                tokio::select! {
                    _ = self.token.cancelled() => {
                        info!(url = url, "task was cancelled during retry backoff");
                        break;
                    }
                    _ = tokio::time::sleep(backoff) => {}
                }
            }
        }
        if let Err(error) = processor.finish().await {
            error!(
                error = ?error,
                "failed to flush queued recoverable segments; original files preserved"
            );
        }
        let health_metrics = route_health.metrics_snapshot();
        info!(
            event = "download_resilience_session_summary",
            url = url,
            platform,
            route_failover_enabled = failover_enabled,
            connection_failures = health_metrics.connection_failures,
            route_switches = health_metrics.route_switches,
            successful_route_switches = health_metrics.successful_switches,
            flv_to_hls_switches = health_metrics.flv_to_hls_switches,
            successful_flv_to_hls_switches = health_metrics.successful_flv_to_hls_switches,
            flv_to_hls_connected_ms = health_metrics.flv_to_hls_connected_for.as_millis(),
            all_routes_backoffs = health_metrics.all_routes_backoffs,
            estimated_missing_ms = estimated_missing.as_millis(),
            valid_segments = processor.stats.valid_segments,
            recoverable_short_segments = processor.stats.recoverable_short_segments,
            recoverable_short_bytes = processor.stats.recoverable_short_bytes,
            merged_recovery_outputs = processor.stats.merged_recovery_outputs,
            deferred_recovery_batches = processor.stats.deferred_recovery_batches,
            invalid_segments = processor.stats.invalid_segments,
            segments_queued_for_upload = processor.stats.segments_queued_for_upload,
            upload_queue_peak_depth = processor.stats.upload_queue_peak_depth,
            "download resilience session summary"
        );
        for route in health_metrics.routes {
            let failure_rate = if route.attempts == 0 {
                0.0
            } else {
                route.failures as f64 / route.attempts as f64
            };
            let average_connected_ms = if route.attempts == 0 {
                0
            } else {
                route.connected_for.as_millis() / u128::from(route.attempts)
            };
            info!(
                event = "download_resilience_route_summary",
                url = url,
                host = route.key.host.as_deref().unwrap_or("unknown"),
                protocol = route.key.protocol,
                quality = route.key.quality.as_deref().unwrap_or("unknown"),
                codec = route.key.codec.as_deref().unwrap_or("unknown"),
                attempts = route.attempts,
                failures = route.failures,
                failure_rate,
                stable_attempts = route.stable_attempts,
                average_connected_ms,
                "download resilience route summary"
            );
        }
        // 异步清理任务
        if let Some(client) = danmaku_client.clone()
            && let Err(e) = client.stop().await
        {
            error!("Error stopping danmaku client: {}", e);
        }
        // 清理资源
        // 确保状态更新和资源清理
        rooms_handle.wake_waker(ctx.worker_id()).await;
        info!("Download task completed: {:?}", result);
        self.done_notify.notify_one();
        Ok(())
    }

    async fn download(
        &self,
        processor: &mut SegmentEventProcessor,
        ctx: Context,
        danmaku_client: Option<Arc<dyn DanmakuClient + Send + Sync>>,
        stream: &LiveStream,
    ) -> DownloadAttempt {
        // 获取配置和主播信息
        let streamer = ctx.live_streamer();

        // 执行下载
        // let hook = processor.create_hook(danmaku_client.clone());
        let completed_configured_segment = Arc::new(AtomicBool::new(false));
        let completed_configured_segment_for_hook = completed_configured_segment.clone();
        let (segment_tx, segment_rx) = async_channel::unbounded::<SegmentInfo>();
        let hook = move |event| {
            match event {
                SegmentEvent::Start { .. } => {
                    warn!("Ignoring unexpected segment start event");
                }
                SegmentEvent::Segment(mut event) => {
                    if matches!(
                        event.close_reason,
                        biliup::downloader::util::SegmentCloseReason::TimedSplit
                            | biliup::downloader::util::SegmentCloseReason::SizeSplit
                    ) {
                        completed_configured_segment_for_hook.store(true, Ordering::Relaxed);
                    }
                    // 分段时，获取到的是已下载的文件名
                    // 触发弹幕滚动保存
                    if let Some(ref client) = danmaku_client {
                        let danmaku_file_path = event.prev_file_path.with_extension("xml");
                        match client.rolling(&danmaku_file_path.display().to_string()) {
                            Ok(true) => event.danmaku_file_path = Some(danmaku_file_path),
                            Ok(false) => {}
                            Err(e) => error!("Danmaku rolling error: {}", e),
                        }
                    }
                    if let Err(error) = segment_tx.try_send(event) {
                        error!(
                            ?error,
                            "failed to transfer completed segment to durable enrollment loop"
                        );
                    }
                }
            }
        };

        let started_at = Instant::now();
        let download = self
            .downloader
            .download(Box::new(hook), ctx.download_config(stream));
        tokio::pin!(download);
        let mut receive_segments = true;
        let result = loop {
            tokio::select! {
                result = &mut download => {
                    break result.change_context(AppError::Custom("Failed to download segment".into()));
                }
                event = segment_rx.recv(), if receive_segments => {
                    match event {
                        Ok(event) => {
                            if let Err(error) = processor.process(event).await {
                                error!(?error, "failed to durably process completed segment");
                            }
                        }
                        Err(_) => receive_segments = false,
                    }
                }
            }
        };
        while let Ok(event) = segment_rx.try_recv() {
            if let Err(error) = processor.process(event).await {
                error!(
                    ?error,
                    "failed to durably process trailing completed segment"
                );
            }
        }
        let connected_for = started_at.elapsed();
        let completed_configured_segment = completed_configured_segment.load(Ordering::Relaxed)
            || matches!(result.as_ref().ok(), Some(DownloadStatus::SegmentCompleted));

        // 处理结果
        info!(url=streamer.url,result=?result, "finished downloading");
        DownloadAttempt {
            result,
            connected_for,
            completed_configured_segment,
        }
    }

    pub(crate) async fn stop(&self) -> AppResult<()> {
        // 仅发出取消信号并更新状态
        // 如果底层下载函数不支持取消，这里不能真正中断正在进行的下载
        self.token.cancel();
        self.downloader.stop().await?;
        self.done_notify.notified().await;
        Ok(())
    }
}

/// 启动完整下载流程。
///
/// 只能由 `Monitor` 在取得下载池许可后调用；调用方必须把许可移动到同一个任务中，
/// 并持有到本函数返回，保证 `pool1_size` 是下载并发的唯一限制。
fn notify_douyin_quality_fallback(ctx: &Context, actual: Option<&str>) {
    let Some(actual) = actual else {
        return;
    };
    let cfg = ctx.config();
    if !cookie_health::quality_below_alert(actual, cfg.douyin_quality_alert.as_deref()) {
        return;
    }
    let threshold = cookie_health::effective_quality_alert(cfg.douyin_quality_alert.as_deref());
    let actual_disp = cookie_health::quality_display(actual);
    let threshold_disp = cookie_health::quality_display(threshold);
    cookie_health::notify_alert(
        cfg.cookie_health_webhook.as_deref(),
        "⚠️ 抖音录制画质已降级",
        &format!(
            "{}：当前录制画质为 {}({})，低于告警阈值 {}({})。可能由候选线路熔断或 cookie（sessionid）失效触发，建议检查。",
            ctx.live_streamer().remark,
            actual_disp,
            actual,
            threshold_disp,
            threshold,
        ),
    );
}

pub async fn start_download_workflow(
    downloader: Arc<dyn LivePlugin + Send + Sync>,
    ctx: Context,
    sender: Sender<UploaderMessage>,
    rooms_handle: Arc<Monitor>,
) {
    let task = Arc::new(DownloadTask::new(downloader_runtime(
        ctx.config().downloader,
        ctx.live_stream(),
    )));
    ctx.change_status(Stage::Download, WorkerStatus::Working(task.clone()))
        .await;

    // 记录实际画质供前端 tag 显示
    let recording_quality = ctx.live_stream().recording_quality.clone();
    ctx.worker()
        .set_recording_quality(recording_quality.clone());

    // 抖音画质降级告警：实际画质低于阈值则推送（初始选流与自动降档共用）。
    if ctx.live_stream().platform == "douyin" {
        notify_douyin_quality_fallback(&ctx, recording_quality.as_deref());
    }

    tokio::spawn({
        let streamer_info = ctx.streamer_info();
        let live_cover_url = streamer_info.live_cover_path.clone();
        let format_filename = ctx.recorder(streamer_info.clone()).format_filename();
        let client = ctx.stateless_client().client.clone();
        let enabled = ctx
            .config()
            .use_live_cover
            .map(|u| u && !live_cover_url.is_empty())
            .unwrap_or(false);
        async move {
            cover_downloader::download_cover_with(
                &live_cover_url,
                enabled,
                &format_filename,
                client,
            )
            .await
        }
    });

    process(&[], &ctx.live_streamer().preprocessor).await;

    let _ = task.execute(&ctx, sender, downloader, rooms_handle).await;

    ctx.worker().set_recording_quality(None);

    process(&[], &ctx.live_streamer().downloaded_processor).await;

    info!(
        "Download workflow completed {} => {:?}",
        ctx.live_streamer().url,
        ctx.status(Stage::Download)
    );
}

#[cfg(test)]
mod retry_state_tests {
    use super::{OfflineRetryState, exponential_backoff};
    use std::time::{Duration, Instant};

    #[test]
    fn backoff_is_two_four_eight_sixteen_then_capped_at_thirty() {
        let delays: Vec<_> = (1..=8).map(exponential_backoff).collect();
        assert_eq!(
            delays,
            vec![
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(16),
                Duration::from_secs(30),
                Duration::from_secs(30),
                Duration::from_secs(30),
                Duration::from_secs(30),
            ]
        );
    }

    #[test]
    fn offline_grace_state_keeps_existing_semantics() {
        let start = Instant::now();
        let grace = Duration::from_secs(10);
        let mut offline = OfflineRetryState::default();

        assert!(!offline.record_unavailable(start, grace));
        assert_eq!(offline.retry_delay(), Duration::from_secs(4));
        assert!(!offline.record_unavailable(start + Duration::from_secs(9), grace));
        assert!(offline.record_unavailable(start + Duration::from_secs(10), grace));

        offline.record_live();
        assert!(offline.offline_since.is_none());
        assert_eq!(offline.offline_retry_count, 0);
        assert!(!offline.record_unavailable(start + Duration::from_secs(30), grace));
    }
}

#[cfg(test)]
mod short_segment_group_tests {
    use super::{compatible_segment_groups, defer_recovery_batch};
    use crate::server::core::downloader::SegmentInfo;
    use biliup::downloader::util::SegmentCloseReason;
    use std::fs;
    use std::time::Duration;
    use tempfile::tempdir;

    fn append_tag(bytes: &mut Vec<u8>, tag_type: u8, body: &[u8], timestamp: u32) {
        bytes.push(tag_type);
        bytes.extend_from_slice(&[
            ((body.len() >> 16) & 0xff) as u8,
            ((body.len() >> 8) & 0xff) as u8,
            (body.len() & 0xff) as u8,
            ((timestamp >> 16) & 0xff) as u8,
            ((timestamp >> 8) & 0xff) as u8,
            (timestamp & 0xff) as u8,
            ((timestamp >> 24) & 0xff) as u8,
            0,
            0,
            0,
        ]);
        bytes.extend_from_slice(body);
        bytes.extend_from_slice(&((11 + body.len()) as u32).to_be_bytes());
    }

    fn flv_fixture(codec_marker: u8) -> Vec<u8> {
        let mut bytes = vec![b'F', b'L', b'V', 1, 5, 0, 0, 0, 9, 0, 0, 0, 0];
        append_tag(&mut bytes, 9, &[0x17, 0, 0, 0, 0, 1, 0x64, codec_marker], 0);
        append_tag(&mut bytes, 8, &[0xaf, 0, 0x12, 0x10], 0);
        append_tag(&mut bytes, 9, &[0x17, 1, 0, 0, 0, 0x65], 1000);
        bytes
    }

    fn event(path: std::path::PathBuf, index: usize) -> SegmentInfo {
        SegmentInfo {
            prev_file_path: path,
            danmaku_file_path: None,
            next_file_path: None,
            segment_index: index,
            close_reason: SegmentCloseReason::TransportError,
            attempt_id: Some(format!("attempt-{index}")),
            recovery_source_paths: Vec::new(),
            enrollment: None,
        }
    }

    #[test]
    fn thirty_seven_compatible_segments_form_one_merge_group() {
        let dir = tempdir().unwrap();
        let mut events = Vec::new();
        for index in 0..37 {
            let path = dir.path().join(format!("segment-{index}.flv"));
            fs::write(&path, flv_fixture(1)).unwrap();
            events.push(event(path, index));
        }
        let groups = compatible_segment_groups(events);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 37);
    }

    #[test]
    fn incompatible_codec_parameters_remain_independent() {
        let dir = tempdir().unwrap();
        let first = dir.path().join("first.flv");
        let second = dir.path().join("second.flv");
        fs::write(&first, flv_fixture(1)).unwrap();
        fs::write(&second, flv_fixture(2)).unwrap();
        let groups = compatible_segment_groups(vec![event(first, 0), event(second, 1)]);
        assert_eq!(groups.len(), 2);
        assert!(groups.iter().all(|group| group.len() == 1));
    }

    #[test]
    fn deferred_batch_manifest_is_durable_and_lists_all_originals() {
        let dir = tempdir().unwrap();
        let first = dir.path().join("first.flv");
        let second = dir.path().join("second.flv");
        fs::write(&first, flv_fixture(1)).unwrap();
        fs::write(&second, flv_fixture(1)).unwrap();
        let events = vec![event(first.clone(), 0), event(second.clone(), 1)];

        let (batch_id, manifest) =
            defer_recovery_batch(&events, "ffmpeg exited 254", Duration::from_secs(900)).unwrap();

        assert!(manifest.exists());
        assert!(first.exists());
        assert!(second.exists());
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(manifest).unwrap()).unwrap();
        assert_eq!(value["recovery_batch_id"], batch_id);
        assert_eq!(value["state"], "Deferred");
        assert_eq!(value["files"].as_array().unwrap().len(), 2);
        assert_eq!(value["last_error"], "ffmpeg exited 254");
    }
}
