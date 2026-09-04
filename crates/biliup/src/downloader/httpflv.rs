use crate::downloader::flv_parser::{
    AACPacketType, AVCPacketType, CodecId, FrameType, SoundFormat, TagData, TagHeader,
    aac_audio_packet_header, avc_video_packet_header, script_data, tag_data, tag_header,
};
use crate::downloader::flv_writer::{FlvFile, FlvTag, TagDataHeader};
use crate::downloader::util::{EVENT_TARGET, LifecycleFile, SegmentCloseReason, Segmentable};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use nom::{Err, IResult};
use reqwest::Response;

use std::time::{Duration, Instant};
use tokio::time::timeout;
use tracing::{info, warn};

pub async fn download(
    mut connection: Connection,
    file: LifecycleFile<'_>,
    segment: Segmentable,
) -> crate::downloader::error::Result<()> {
    download_inner(&mut connection, file, segment, None).await
}

#[derive(Debug, Clone)]
pub struct HttpFlvLogContext {
    pub attempt_id: String,
    pub stream_host: String,
    pub protocol: String,
    pub quality: Option<String>,
}

/// 服务端录制走这条路径：调用方保留 `Connection` 的所有权，
/// 以便在本函数返回后读取 `diagnostics()`（静默时长要传回重连循环做缺口记账）。
pub async fn download_with_context(
    connection: &mut Connection,
    file: LifecycleFile<'_>,
    segment: Segmentable,
    log_context: HttpFlvLogContext,
) -> crate::downloader::error::Result<()> {
    download_inner(connection, file, segment, Some(&log_context)).await
}

/// 一条连接内的分段与媒体时间戳进度。
///
/// 用途是把「连接死亡时距上一次分段过了多久」写进日志：抖音那批断连是否由分段动作触发，
/// 只能靠这个字段在真实录制里归因（见 `.scratch/douyin-reconnect-gap/issues/05`）。
/// 时间戳字段用的是 FLV 的流级绝对基准，跨连接可直接相减得到真实缺口。
#[derive(Debug, Clone, Default)]
pub struct FlvProgress {
    /// 本连接内发生的分段次数
    pub splits: u32,
    /// 最后一次分段的本地时刻
    pub last_split_at: Option<Instant>,
    /// 本连接写入文件的第一个 tag 的媒体时间戳
    pub first_timestamp_ms: Option<u64>,
    /// 本连接写入文件的最后一个 tag 的媒体时间戳
    pub last_timestamp_ms: Option<u64>,
}

impl FlvProgress {
    fn since_last_split_ms(&self) -> i64 {
        self.last_split_at
            .map(|at| at.elapsed().as_millis() as i64)
            .unwrap_or(-1)
    }
}

fn optional_ms(value: Option<u64>) -> i64 {
    value.map(|v| v as i64).unwrap_or(-1)
}

/// Source-side rollup for the DTS warning: the first backward jump of a segment is reported one
/// to one, the rest are counted and flushed as one summary. A summary never replaces the old
/// per-tag warning line, and a segment change always starts a new record.
#[derive(Default)]
struct DtsBackwardRollup {
    segment_id: String,
    original_file: String,
    count: u64,
    first_ms: u64,
    last_ms: u64,
    max_backward_ms: u64,
}

impl DtsBackwardRollup {
    fn record(&mut self, file: &LifecycleFile<'_>, previous_ms: u64, current_ms: u64) {
        let segment_id = file
            .identity()
            .map(|identity| identity.segment_id.clone())
            .unwrap_or_default();
        if segment_id != self.segment_id {
            self.flush(file);
            self.segment_id = segment_id;
            self.original_file = original_file(file).to_owned();
        }
        let backward = previous_ms.saturating_sub(current_ms);
        self.count += 1;
        if self.count == 1 {
            self.first_ms = current_ms;
            self.max_backward_ms = backward;
            emit_dts_first(file, previous_ms, current_ms);
        } else {
            self.max_backward_ms = self.max_backward_ms.max(backward);
        }
        self.last_ms = current_ms;
    }

    fn flush(&mut self, file: &LifecycleFile<'_>) {
        if self.count > 1 {
            emit_dts_summary(file, self);
        }
        self.count = 0;
        self.first_ms = 0;
        self.last_ms = 0;
        self.max_backward_ms = 0;
    }
}

fn emit_dts_first(file: &LifecycleFile<'_>, previous_ms: u64, current_ms: u64) {
    let owner = file.owner();
    warn!(
        target: EVENT_TARGET,
        event_name = "recording.dts_backward",
        outcome = "executed",
        reason_code = "timestamp_backward",
        segment_id = segment_id(file),
        original_file = original_file(file),
        previous_ms,
        current_ms,
        live_streamer_id = owner.live_streamer_id(),
        streamer_info_id = owner.streamer_info_id(),
        task_id = owner.task_id(),
        download_attempt_id = owner.download_attempt_id(),
        "检测到时间戳倒退，继续录制并标记待检查"
    );
}

fn emit_dts_summary(file: &LifecycleFile<'_>, rollup: &DtsBackwardRollup) {
    let owner = file.owner();
    warn!(
        target: EVENT_TARGET,
        event_name = "recording.dts_backward",
        outcome = "executed",
        reason_code = "timestamp_backward",
        segment_id = rollup.segment_id,
        original_file = rollup.original_file,
        count = rollup.count,
        first_ms = rollup.first_ms,
        last_ms = rollup.last_ms,
        max_backward_ms = rollup.max_backward_ms,
        live_streamer_id = owner.live_streamer_id(),
        streamer_info_id = owner.streamer_info_id(),
        task_id = owner.task_id(),
        download_attempt_id = owner.download_attempt_id(),
        "本分段时间戳倒退汇总"
    );
}

/// Connection outcome for the recording chain. The gap that follows is measured by the reconnect
/// loop, so this event only reports what this connection itself observed.
fn emit_disconnected(
    owner: &crate::downloader::util::RecordingOwner,
    outcome: &'static str,
    reason_code: &'static str,
    diagnostics: &ConnectionDiagnostics,
    error: Option<String>,
) {
    let silent_ms = diagnostics.silent_for.as_millis().min(u64::MAX as u128) as u64;
    let duration_ms = diagnostics.connected_for.as_millis().min(u64::MAX as u128) as u64;
    let error = error.unwrap_or_default();
    if outcome == "failed" {
        warn!(
            target: EVENT_TARGET,
            event_name = "recording.disconnected",
            outcome,
            reason_code,
            silent_ms,
            duration_ms,
            error,
            live_streamer_id = owner.live_streamer_id(),
            streamer_info_id = owner.streamer_info_id(),
            task_id = owner.task_id(),
            download_attempt_id = owner.download_attempt_id(),
            "拉流连接异常结束"
        );
    } else {
        info!(
            target: EVENT_TARGET,
            event_name = "recording.disconnected",
            outcome,
            reason_code,
            silent_ms,
            duration_ms,
            live_streamer_id = owner.live_streamer_id(),
            streamer_info_id = owner.streamer_info_id(),
            task_id = owner.task_id(),
            download_attempt_id = owner.download_attempt_id(),
            "拉流连接正常结束"
        );
    }
}

fn segment_id<'b>(file: &'b LifecycleFile<'_>) -> &'b str {
    file.identity()
        .map(|identity| identity.segment_id.as_str())
        .unwrap_or("")
}

fn original_file<'b>(file: &'b LifecycleFile<'_>) -> &'b str {
    file.identity()
        .map(|identity| identity.original_file.as_str())
        .unwrap_or("")
}

async fn download_inner(
    connection: &mut Connection,
    file: LifecycleFile<'_>,
    segment: Segmentable,
    log_context: Option<&HttpFlvLogContext>,
) -> crate::downloader::error::Result<()> {
    let file_name = file.file_name.clone();
    // Identity travels with the file; copy it out before the writer takes ownership so the
    // connection events can still say who was recording.
    let owner = file.owner().clone();
    let mut progress = FlvProgress::default();
    let result = parse_flv(connection, file, segment, &mut progress).await;
    let diagnostics = connection.diagnostics();
    let attempt_id = log_context
        .map(|context| context.attempt_id.as_str())
        .unwrap_or("untracked");
    let stream_host = log_context
        .map(|context| context.stream_host.as_str())
        .unwrap_or("unknown");
    let protocol = log_context
        .map(|context| context.protocol.as_str())
        .unwrap_or("flv");
    let quality = log_context
        .and_then(|context| context.quality.as_deref())
        .unwrap_or("unknown");
    match result {
        Ok(_) => {
            info!(
                event = "httpflv_connection_closed",
                outcome = "stream_ended",
                received_bytes = diagnostics.received_bytes,
                connected_ms = diagnostics.connected_for.as_millis() as u64,
                silent_ms = diagnostics.silent_for.as_millis() as u64,
                stall_timeout_secs = diagnostics.stall_timeout.as_secs(),
                splits = progress.splits,
                since_last_split_ms = progress.since_last_split_ms(),
                first_timestamp_ms = optional_ms(progress.first_timestamp_ms),
                last_timestamp_ms = optional_ms(progress.last_timestamp_ms),
                attempt_id,
                stream_host,
                protocol,
                quality,
                "httpflv connection closed"
            );
            info!("Done... {}", file_name);
            emit_disconnected(&owner, "succeeded", "stream_end", &diagnostics, None);
            Ok(())
        }
        Err(e) => {
            warn!(
                event = "httpflv_connection_closed",
                outcome = "transport_error",
                error = ?e,
                http_status = diagnostics.http_status,
                content_encoding = diagnostics.content_encoding.as_deref().unwrap_or("none"),
                transfer_encoding = diagnostics.transfer_encoding.as_deref().unwrap_or("none"),
                received_bytes = diagnostics.received_bytes,
                connected_ms = diagnostics.connected_for.as_millis() as u64,
                silent_ms = diagnostics.silent_for.as_millis() as u64,
                stall_timeout_secs = diagnostics.stall_timeout.as_secs(),
                splits = progress.splits,
                since_last_split_ms = progress.since_last_split_ms(),
                first_timestamp_ms = optional_ms(progress.first_timestamp_ms),
                last_timestamp_ms = optional_ms(progress.last_timestamp_ms),
                buffered = diagnostics.buffered,
                attempt_id,
                stream_host,
                protocol,
                quality,
                "httpflv download failed"
            );
            let reason = if matches!(
                e,
                crate::downloader::error::Error::HttpFlvReadTimeout { .. }
            ) {
                "read_timeout"
            } else {
                "transport_error"
            };
            emit_disconnected(&owner, "failed", reason, &diagnostics, Some(format!("{e}")));
            Err(e)
        }
    }
}

pub(crate) async fn parse_flv(
    connection: &mut Connection,
    file: LifecycleFile<'_>,
    mut segment: Segmentable,
    progress: &mut FlvProgress,
) -> crate::downloader::error::Result<()> {
    let mut flv_tags_cache: Vec<(TagHeader, Bytes, Bytes)> = Vec::new();
    // println!("parse_flv Segment: {:?}", segment);
    let _previous_tag_size = connection.read_frame(4).await?;

    let mut out = FlvFile::new(file)?;
    let mut dts_rollup = DtsBackwardRollup::default();
    let result: crate::downloader::error::Result<()> = async {
    segment.set_size_position(9 + 4);
    // let mut downloaded_size = 9 + 4;
    let mut on_meta_data = None;
    let mut aac_sequence_header = None;
    let mut h264_sequence_header: Option<(TagHeader, Bytes, Bytes)> = None;
    let mut prev_timestamp = 0;
    // 本段起点是否已锚定。曾经用 `prev_timestamp == 0` 代替这个状态，但 `prev_timestamp`
    // 是「上一批写出 tag 的最后一个时间戳」，抖音重发的 timestamp=0 Script tag 会让下一个
    // 关键帧误判成「流刚初始化」，把 start 推到当前，定时分段从此失效（issue #32）。
    let mut start_anchored = false;
    let mut create_new = false;
    loop {
        let tag_header_bytes = connection.read_frame(11).await?;
        if tag_header_bytes.is_empty() {
            // let mut rdr = Cursor::new(tag_header_bytes);
            // println!("{}", rdr.read_u32::<BigEndian>().unwrap());
            break;
        }

        let (_, tag_header) = map_parse_err(tag_header(&tag_header_bytes), "tag header")?;
        // write_tag_header(&mut out, &tag_header)?;

        let bytes = connection.read_frame(tag_header.data_size as usize).await?;
        let previous_tag_size = connection.read_frame(4).await?;
        // out.write(&bytes)?;
        let (i, flv_tag_data) = map_parse_err(
            tag_data(tag_header.tag_type, tag_header.data_size as usize)(&bytes),
            "tag data",
        )?;
        let flv_tag = match flv_tag_data {
            TagData::Audio(audio_data) => {
                let packet_type = if audio_data.sound_format == SoundFormat::AAC {
                    let (_, packet_header) = aac_audio_packet_header(audio_data.sound_data)
                        .expect("Error in parsing aac audio packet header.");
                    if packet_header.packet_type == AACPacketType::SequenceHeader {
                        if aac_sequence_header.is_some() {
                            warn!("Unexpected aac sequence header tag. {tag_header:?}");
                            // panic!("Unexpected aac_sequence_header tag.");
                            // create_new = true;
                        }
                        aac_sequence_header =
                            Some((tag_header, bytes.clone(), previous_tag_size.clone()))
                    }
                    Some(packet_header.packet_type)
                } else {
                    None
                };

                FlvTag {
                    header: tag_header,
                    data: TagDataHeader::Audio {
                        sound_format: audio_data.sound_format,
                        sound_rate: audio_data.sound_rate,
                        sound_size: audio_data.sound_size,
                        sound_type: audio_data.sound_type,
                        packet_type,
                    },
                }
            }
            TagData::Video(video_data) => {
                let (packet_type, composition_time) = if CodecId::H264 == video_data.codec_id {
                    let (_, avc_video_header) = avc_video_packet_header(video_data.video_data)
                        .expect("Error in parsing avc video packet header.");
                    if avc_video_header.packet_type == AVCPacketType::SequenceHeader {
                        if let Some((_, binary_data, _)) = &h264_sequence_header {
                            warn!("Unexpected h264 sequence header tag. {tag_header:?}");
                            if bytes != binary_data {
                                create_new = true;
                                warn!("Different h264 sequence header tag. {tag_header:?}");
                            }
                        }
                        h264_sequence_header =
                            Some((tag_header, bytes.clone(), previous_tag_size.clone()))
                    }
                    (
                        Some(avc_video_header.packet_type),
                        Some(avc_video_header.composition_time),
                    )
                } else {
                    (None, None)
                };

                FlvTag {
                    header: tag_header,
                    data: TagDataHeader::Video {
                        frame_type: video_data.frame_type,
                        codec_id: video_data.codec_id,
                        packet_type,
                        composition_time,
                    },
                }
            }
            TagData::Script => {
                let (_, tag_data) = script_data(i).expect("Error in parsing script tag.");
                if on_meta_data.is_some() {
                    warn!("Unexpected script tag. {tag_header:?}");
                }
                on_meta_data = Some((tag_header, bytes.clone(), previous_tag_size.clone()));

                FlvTag {
                    header: tag_header,
                    data: TagDataHeader::Script(tag_data),
                }
            }
        };
        match &flv_tag {
            FlvTag {
                data:
                    TagDataHeader::Video {
                        frame_type: FrameType::Key,
                        ..
                    },
                ..
            } => {
                let timestamp = flv_tag.header.timestamp as u64;
                if !start_anchored {
                    start_anchored = true;
                    // 重连后 CDN 可能给延续的非零时间基准，首个关键帧必须成为起点，
                    // 否则它会立刻满足时间条件。
                    segment.set_start_time(Duration::from_millis(timestamp));
                }
                segment.set_time_position(Duration::from_millis(timestamp));
                for (tag_header, flv_tag_data, previous_tag_size_bytes) in &flv_tags_cache {
                    if tag_header.timestamp < prev_timestamp {
                        warn!(
                            "Non-monotonous DTS in output stream; previous: {prev_timestamp}, current: {};",
                            tag_header.timestamp
                        );
                        dts_rollup.record(
                            &out.file,
                            prev_timestamp as u64,
                            tag_header.timestamp as u64,
                        );
                    }
                    out.write_tag(tag_header, flv_tag_data, previous_tag_size_bytes)?;
                    segment.increase_size((11 + tag_header.data_size + 4) as u64);
                    progress
                        .first_timestamp_ms
                        .get_or_insert(tag_header.timestamp as u64);
                    progress.last_timestamp_ms = Some(tag_header.timestamp as u64);
                    // downloaded_size += (11 + tag_header.data_size + 4) as u64;
                    prev_timestamp = tag_header.timestamp
                    // println!("{downloaded_size}");
                }
                flv_tags_cache.clear();

                if segment.needed() || create_new {
                    // The reason must be read before the counters are reset: reading it after
                    // `set_size_position`/`set_start_time` always saw a fresh segment and
                    // reported every configured split as `Unknown`.
                    let reason = if segment.size_needed() {
                        SegmentCloseReason::SizeSplit
                    } else if segment.time_needed() {
                        SegmentCloseReason::TimedSplit
                    } else {
                        SegmentCloseReason::Unknown
                    };
                    segment.set_start_time(Duration::from_millis(timestamp));
                    segment.set_size_position(9 + 4);

                    let (meta_header, meta_bytes, previous_meta_tag_size) =
                        on_meta_data.as_ref().expect("on_meta_data does not exist");
                    // onMetaData
                    flv_tags_cache.push((
                        *meta_header,
                        meta_bytes.clone(),
                        previous_meta_tag_size.clone(),
                    ));
                    // AACSequenceHeader
                    let aac_sequence_header = aac_sequence_header
                        .as_ref()
                        .expect("aac_sequence_header does not exist");
                    flv_tags_cache.push((
                        aac_sequence_header.0,
                        aac_sequence_header.1.clone(),
                        aac_sequence_header.2.clone(),
                    ));
                    if !create_new {
                        // H264SequenceHeader
                        flv_tags_cache.push(
                            h264_sequence_header
                                .as_ref()
                                .expect("h264_sequence_header does not exist")
                                .clone(),
                        );
                    }
                    info!("{} splitting.{segment:?}", out.file.file_name);
                    // Flush before the split so the summary still names the segment it counted.
                    dts_rollup.flush(&out.file);
                    out.create_new(reason)?;
                    progress.splits = progress.splits.saturating_add(1);
                    progress.last_split_at = Some(Instant::now());
                    create_new = false;
                }
                flv_tags_cache.push((tag_header, bytes.clone(), previous_tag_size.clone()));
            }
            _ => {
                flv_tags_cache.push((tag_header, bytes.clone(), previous_tag_size.clone()));
            }
        }
    }
    Ok(())
    }
    .await;
    dts_rollup.flush(&out.file);
    let close_reason = if result.is_ok() {
        SegmentCloseReason::StreamEnded
    } else {
        SegmentCloseReason::TransportError
    };
    let finalize_result = out.finish(close_reason);
    match (result, finalize_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(finalize_error)) => Err(finalize_error.into()),
        (Err(download_error), Ok(())) => Err(download_error),
        (Err(download_error), Err(finalize_error)) => Err(crate::downloader::error::Error::Custom(
            format!("{download_error}; additionally failed to finalize segment: {finalize_error}"),
        )),
    }
}

pub fn map_parse_err<'a, T>(
    i_result: IResult<&'a [u8], T>,
    msg: &str,
) -> core::result::Result<(&'a [u8], T), crate::downloader::error::Error> {
    match i_result {
        Ok((i, res)) => Ok((i, res)),
        Err(nom::Err::Incomplete(needed)) => Err(crate::downloader::error::Error::NomIncomplete(
            msg.to_string(),
            needed,
        )),
        Err(Err::Error(e)) => Err(crate::downloader::error::Error::Custom(format!(
            "parse {msg} err: {e:?}"
        ))),
        Err(Err::Failure(f)) => Err(crate::downloader::error::Error::Custom(format!(
            "{msg} Failure: {f:?}"
        ))),
    }
}

/// 码流停顿看门狗的默认阈值。
///
/// 语义是「连续多久一个字节都没收到」——每收到一个 chunk 就重置，不是连接总时长。
/// 保持 30 秒是为了回滚安全：只对确认被上游掐断的房间通过配置下调。
pub const DEFAULT_STALL_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Connection {
    resp: Response,
    buffer: BytesMut,
    http_status: u16,
    content_encoding: Option<String>,
    transfer_encoding: Option<String>,
    received_bytes: u64,
    started_at: Instant,
    /// 最后一次成功收到 chunk 的时刻；构造时等于 `started_at`
    last_chunk_at: Instant,
    stall_timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct ConnectionDiagnostics {
    pub http_status: u16,
    pub content_encoding: Option<String>,
    pub transfer_encoding: Option<String>,
    pub received_bytes: u64,
    pub connected_for: Duration,
    /// 上游最后一个字节到现在的静默时长。缺口的大头在这一段里，
    /// 而不是在报错之后的重连里。
    pub silent_for: Duration,
    pub stall_timeout: Duration,
    pub buffered: usize,
}

impl Connection {
    pub fn new(resp: Response) -> Connection {
        Connection::with_stall_timeout(resp, DEFAULT_STALL_TIMEOUT)
    }

    pub fn with_stall_timeout(resp: Response, stall_timeout: Duration) -> Connection {
        let http_status = resp.status().as_u16();
        let content_encoding = header_value(&resp, reqwest::header::CONTENT_ENCODING);
        let transfer_encoding = header_value(&resp, reqwest::header::TRANSFER_ENCODING);
        let started_at = Instant::now();
        Connection {
            resp,
            buffer: BytesMut::with_capacity(8 * 1024),
            http_status,
            content_encoding,
            transfer_encoding,
            received_bytes: 0,
            started_at,
            last_chunk_at: started_at,
            stall_timeout,
        }
    }

    pub fn diagnostics(&self) -> ConnectionDiagnostics {
        ConnectionDiagnostics {
            http_status: self.http_status,
            content_encoding: self.content_encoding.clone(),
            transfer_encoding: self.transfer_encoding.clone(),
            received_bytes: self.received_bytes,
            connected_for: self.started_at.elapsed(),
            silent_for: self.last_chunk_at.elapsed(),
            stall_timeout: self.stall_timeout,
            buffered: self.buffer.len(),
        }
    }

    pub async fn read_frame(
        &mut self,
        chunk_size: usize,
    ) -> crate::downloader::error::Result<Bytes> {
        // let mut buf = [0u8; 8 * 1024];
        loop {
            if chunk_size <= self.buffer.len() {
                let bytes = Bytes::copy_from_slice(&self.buffer[..chunk_size]);
                self.buffer.advance(chunk_size);
                return Ok(bytes);
            }
            // BytesMut::with_capacity(0).deref_mut()
            // tokio::fs::File::open("").read()
            // self.resp.chunk()
            match timeout(self.stall_timeout, self.resp.chunk()).await {
                Ok(Ok(Some(chunk))) => {
                    self.received_bytes = self.received_bytes.saturating_add(chunk.len() as u64);
                    self.last_chunk_at = Instant::now();
                    self.buffer.put(chunk);
                }
                Ok(Ok(None)) => {
                    let buffered = self.buffer.len();
                    if buffered == 0 {
                        return Ok(self.buffer.split().freeze());
                    }
                    warn!(
                        buffered,
                        "httpflv chunk stream ended before requested frame was complete"
                    );
                    return Err(crate::downloader::error::Error::HttpFlvIncompleteFrame {
                        buffered,
                    });
                }
                Ok(Err(err)) => {
                    let err = err.without_url();
                    warn!(error = ?err, buffered = self.buffer.len(), "httpflv chunk read failed");
                    return Err(err.into());
                }
                Err(err) => {
                    let buffered = self.buffer.len();
                    warn!(
                        error = %err,
                        buffered,
                        stall_timeout_secs = self.stall_timeout.as_secs(),
                        connected_ms = self.started_at.elapsed().as_millis() as u64,
                        "httpflv chunk read timed out"
                    );
                    return Err(crate::downloader::error::Error::HttpFlvReadTimeout { buffered });
                }
            }
            // let n = match self.resp.read(&mut buf).await {
            //     Ok(n) => n,
            //     Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            //     Err(e) => return Err(e),
            // };

            // if n == 0 {
            //     return Ok(self.buffer.split().freeze());
            // }
            // self.buffer.put_slice(&buf[..n]);
        }
    }
}

fn header_value(resp: &Response, name: reqwest::header::HeaderName) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::{Connection, DEFAULT_STALL_TIMEOUT, DtsBackwardRollup};
    use crate::downloader::util::LifecycleFile;
    use bytes::{Buf, BufMut, Bytes, BytesMut};
    use futures::StreamExt;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tracing::field::{Field, Visit};
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<BTreeMap<String, String>>>>);

    struct Collector<'a>(&'a mut BTreeMap<String, String>);
    impl Visit for Collector<'_> {
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
        fn record_u64(&mut self, field: &Field, value: u64) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0
                .insert(field.name().to_string(), format!("{value:?}"));
        }
    }

    impl<S: tracing::Subscriber> Layer<S> for Captured {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if event.metadata().target() != super::EVENT_TARGET {
                return;
            }
            let mut fields = BTreeMap::new();
            event.record(&mut Collector(&mut fields));
            self.0.lock().unwrap().push(fields);
        }
    }

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

    /// A complete stream: metadata, both sequence headers, then keyframes carrying real payload.
    fn splittable_flv(keyframes: usize, payload: usize) -> Vec<u8> {
        let mut bytes = vec![b'F', b'L', b'V', 1, 5, 0, 0, 0, 9, 0, 0, 0, 0];
        let mut metadata = vec![0x02, 0x00, 0x0a];
        metadata.extend_from_slice(b"onMetaData");
        metadata.push(0x05); // AMF null: enough to be a valid onMetaData tag
        append_tag(&mut bytes, 18, &metadata, 0);
        append_tag(&mut bytes, 8, &[0xaf, 0x00, 0x12, 0x10], 0);
        append_tag(&mut bytes, 9, &[0x17, 0x00, 0, 0, 0, 0x01, 0x64, 0x00], 0);
        for index in 0..keyframes {
            let mut frame = vec![0x17, 0x01, 0, 0, 0];
            frame.resize(5 + payload, 0x41);
            append_tag(&mut bytes, 9, &frame, (index as u32 + 1) * 1_000);
        }
        bytes
    }

    /// 同一条流里反复重发 timestamp=0 的 onMetaData——抖音 CDN 的实际行为。
    /// 每个关键帧前插一个，保证下一个关键帧看到的 `prev_timestamp` 是 0。
    fn flv_with_repeated_metadata(keyframes: usize, payload: usize) -> Vec<u8> {
        let mut bytes = vec![b'F', b'L', b'V', 1, 5, 0, 0, 0, 9, 0, 0, 0, 0];
        let mut metadata = vec![0x02, 0x00, 0x0a];
        metadata.extend_from_slice(b"onMetaData");
        metadata.push(0x05);
        append_tag(&mut bytes, 18, &metadata, 0);
        append_tag(&mut bytes, 8, &[0xaf, 0x00, 0x12, 0x10], 0);
        append_tag(&mut bytes, 9, &[0x17, 0x00, 0, 0, 0, 0x01, 0x64, 0x00], 0);
        for index in 0..keyframes {
            append_tag(&mut bytes, 18, &metadata, 0);
            let mut frame = vec![0x17, 0x01, 0, 0, 0];
            frame.resize(5 + payload, 0x41);
            append_tag(&mut bytes, 9, &frame, (index as u32 + 1) * 1_000);
        }
        bytes
    }

    /// 重连之后的抖音 FLV：正常帧带着绝对媒体时钟（约 9 小时），CDN 在其间反复重发
    /// timestamp=0 的**关键帧**（#32 那条覆盖的是重发 Script tag，两者路径不同）。
    fn flv_with_absolute_base_and_zero_keyframes(keyframes: usize, payload: usize) -> Vec<u8> {
        const BASE: u32 = 32_891_256;
        let mut bytes = vec![b'F', b'L', b'V', 1, 5, 0, 0, 0, 9, 0, 0, 0, 0];
        let mut metadata = vec![0x02, 0x00, 0x0a];
        metadata.extend_from_slice(b"onMetaData");
        metadata.push(0x05);
        append_tag(&mut bytes, 18, &metadata, 0);
        append_tag(&mut bytes, 8, &[0xaf, 0x00, 0x12, 0x10], 0);
        append_tag(&mut bytes, 9, &[0x17, 0x00, 0, 0, 0, 0x01, 0x64, 0x00], 0);
        for index in 0..keyframes {
            // CDN 重发的初始化关键帧：时间戳 0，负载只有几个字节
            append_tag(&mut bytes, 9, &[0x17, 0x01, 0, 0, 0, 0x41], 0);
            let mut frame = vec![0x17, 0x01, 0, 0, 0];
            frame.resize(5 + payload, 0x41);
            append_tag(&mut bytes, 9, &frame, BASE + (index as u32 + 1) * 1_000);
        }
        bytes
    }

    fn complete_response(body: Vec<u8>) -> reqwest::Response {
        reqwest::Response::from(http::Response::new(reqwest::Body::from(body)))
    }

    /// The close reason used to be read after the counters had already been reset, so every
    /// configured split reported `Unknown`. The recorded reason must name the limit that fired.
    #[tokio::test]
    async fn a_size_split_closes_the_segment_with_split_limit() {
        let directory = tempfile::tempdir().unwrap();
        let template = directory.path().join("split").display().to_string();
        let captured = Captured::default();
        let _guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));

        let mut connection = Connection::new(complete_response(splittable_flv(6, 2_000)));
        // The caller consumes the 9 byte FLV header before handing the stream to the parser.
        connection.read_frame(9).await.unwrap();
        let file = crate::downloader::util::LifecycleFile::new(&template, "flv");
        let segment = crate::downloader::util::Segmentable::new(None, Some(4_000));
        let mut progress = super::FlvProgress::default();
        super::parse_flv(&mut connection, file, segment, &mut progress)
            .await
            .unwrap();

        let events = captured.0.lock().unwrap().clone();
        let closes: Vec<_> = events
            .iter()
            .filter(|fields| {
                fields.get("event_name").map(String::as_str) == Some("recording.segment_closed")
            })
            .collect();
        assert!(
            progress.splits >= 2,
            "the fixture must split more than once"
        );
        assert_eq!(
            closes.len() as u32,
            progress.splits + 1,
            "one close per file"
        );
        let reasons: Vec<_> = closes
            .iter()
            .map(|fields| fields["reason_code"].as_str())
            .collect();
        assert!(
            reasons
                .iter()
                .filter(|reason| **reason == "split_limit")
                .count()
                >= 2,
            "configured splits must be reported as split_limit: {reasons:?}"
        );
        assert_eq!(reasons.last(), Some(&"stream_end"));
    }

    /// 起点曾经用 `prev_timestamp == 0` 判断是否已初始化，而 `prev_timestamp` 是上一批写出
    /// tag 的最后一个时间戳。重发的 timestamp=0 Script tag 会让每个关键帧都把 start 推到当前，
    /// 定时分段永远不满足（issue #32：配置 30 分钟，实测单段录了 3 小时 16 分）。
    #[tokio::test]
    async fn repeated_zero_timestamp_metadata_does_not_postpone_the_timed_split() {
        let directory = tempfile::tempdir().unwrap();
        let template = directory
            .path()
            .join("metadata-flood")
            .display()
            .to_string();

        let mut connection = Connection::new(complete_response(flv_with_repeated_metadata(10, 16)));
        connection.read_frame(9).await.unwrap();
        let file = crate::downloader::util::LifecycleFile::new(&template, "flv");
        // 关键帧步进 1s，10 个关键帧覆盖 10s；3s 一刀应该切出 3 刀。
        let segment = crate::downloader::util::Segmentable::new(Some(Duration::from_secs(3)), None);
        let mut progress = super::FlvProgress::default();
        super::parse_flv(&mut connection, file, segment, &mut progress)
            .await
            .unwrap();

        assert_eq!(
            progress.splits, 3,
            "timestamp=0 的元数据重发不该重置本段计时起点"
        );
    }

    /// issue #35：`elapsed` 曾是 `current - start`，一个 timestamp=0 的关键帧把 start 拉到 0，
    /// 下一个带绝对时钟的关键帧立刻满足时间条件，切出十几秒、几百字节的碎片（随后被判无效删除）。
    #[tokio::test]
    async fn a_zero_timestamp_keyframe_does_not_shatter_the_segment() {
        let directory = tempfile::tempdir().unwrap();
        let template = directory.path().join("rebase").display().to_string();
        let captured = Captured::default();
        let _guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));

        let mut connection = Connection::new(complete_response(
            flv_with_absolute_base_and_zero_keyframes(10, 4_000),
        ));
        connection.read_frame(9).await.unwrap();
        let file = crate::downloader::util::LifecycleFile::new(&template, "flv");
        // 正常帧步进 1s，10 帧覆盖 10s；3s 一刀。
        let segment = crate::downloader::util::Segmentable::new(Some(Duration::from_secs(3)), None);
        let mut progress = super::FlvProgress::default();
        super::parse_flv(&mut connection, file, segment, &mut progress)
            .await
            .unwrap();

        assert!(
            progress.splits <= 3,
            "重发的 timestamp=0 关键帧不该触发额外切片，实测 {} 刀",
            progress.splits
        );
        assert!(progress.splits >= 2, "配置的 3s 定时分段仍必须生效");

        let events = captured.0.lock().unwrap().clone();
        let shards: Vec<_> = events
            .iter()
            .filter(|fields| {
                fields.get("event_name").map(String::as_str) == Some("recording.segment_closed")
                    && fields.get("reason_code").map(String::as_str) == Some("split_limit")
                    && fields
                        .get("size_bytes")
                        .and_then(|size| size.parse::<u64>().ok())
                        .is_some_and(|size| size < 4_000)
            })
            .collect();
        assert!(
            shards.is_empty(),
            "定时分段不该产出装不下一个关键帧的碎片：{shards:?}"
        );
    }

    /// The old per-tag DTS warning stays one to one; the native stream reports the first jump of
    /// a segment and then one rollup, and a new segment always starts its own record.
    #[test]
    fn dts_rollup_reports_the_first_jump_then_one_summary_per_segment() {
        let directory = tempfile::tempdir().unwrap();
        let template = directory.path().join("dts-%Y%m%d").display().to_string();
        let captured = Captured::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let (first_id, second_id) = tracing::subscriber::with_default(subscriber, || {
            let mut file = LifecycleFile::new(&template, "flv");
            let mut rollup = DtsBackwardRollup::default();

            file.create().unwrap();
            let first_id = file.identity().unwrap().segment_id.clone();
            rollup.record(&file, 1_000, 400);
            rollup.record(&file, 1_200, 900);
            rollup.record(&file, 1_500, 100);

            // A split allocates a new identity; the counts must not leak across it.
            file.create().unwrap();
            let second_id = file.identity().unwrap().segment_id.clone();
            rollup.record(&file, 2_000, 1_900);
            rollup.flush(&file);
            (first_id, second_id)
        });

        let events = captured.0.lock().unwrap().clone();
        let names: Vec<_> = events
            .iter()
            .filter(|fields| {
                fields.get("event_name").map(String::as_str) == Some("recording.dts_backward")
            })
            .cloned()
            .collect();
        assert_eq!(names.len(), 3, "first jump, first summary, second jump");

        assert_eq!(names[0]["segment_id"], first_id);
        assert_eq!(names[0]["previous_ms"], "1000");
        assert_eq!(names[0]["current_ms"], "400");
        assert!(!names[0].contains_key("count"));

        assert_eq!(names[1]["segment_id"], first_id);
        assert_eq!(names[1]["count"], "3", "count includes the first jump");
        assert_eq!(names[1]["first_ms"], "400");
        assert_eq!(names[1]["last_ms"], "100");
        assert_eq!(names[1]["max_backward_ms"], "1400");

        assert_eq!(names[2]["segment_id"], second_id);
        assert_eq!(names[2]["previous_ms"], "2000");
        // A single jump in the new segment produces no summary of its own.
        assert!(!names[2].contains_key("count"));
    }

    /// 构造一个「先吐若干 chunk、之后永远不再产出」的响应。
    /// 用来模拟上游停发但连接未关闭——本 effort 里真实发生的正是这种静默。
    fn stalling_response(chunks: Vec<&'static [u8]>) -> reqwest::Response {
        let stream = futures::stream::iter(
            chunks
                .into_iter()
                .map(|chunk| Ok::<_, std::io::Error>(Bytes::from_static(chunk))),
        )
        .chain(futures::stream::pending());
        reqwest::Response::from(http::Response::new(reqwest::Body::wrap_stream(stream)))
    }

    /// 构造一个持续按 `interval` 产出 chunk 的响应。
    fn dripping_response(
        chunk: &'static [u8],
        count: usize,
        interval: Duration,
    ) -> reqwest::Response {
        let stream = futures::stream::unfold(0usize, move |sent| async move {
            if sent >= count {
                return None;
            }
            tokio::time::sleep(interval).await;
            Some((Ok::<_, std::io::Error>(Bytes::from_static(chunk)), sent + 1))
        });
        reqwest::Response::from(http::Response::new(reqwest::Body::wrap_stream(stream)))
    }

    #[tokio::test]
    async fn connection_new_keeps_the_thirty_second_default() {
        let connection = Connection::new(stalling_response(vec![]));
        let diagnostics = connection.diagnostics();
        assert_eq!(diagnostics.stall_timeout, DEFAULT_STALL_TIMEOUT);
        assert_eq!(diagnostics.stall_timeout, Duration::from_secs(30));
        // 刚建连时静默时长应接近 0，而不是未初始化的大数
        assert!(diagnostics.silent_for < Duration::from_millis(100));
    }

    #[tokio::test]
    async fn read_frame_gives_up_after_the_configured_stall_timeout() {
        let mut connection = Connection::with_stall_timeout(
            stalling_response(vec![b"abcd"]),
            Duration::from_millis(300),
        );
        let started = std::time::Instant::now();
        let error = connection
            .read_frame(8)
            .await
            .expect_err("stalled upstream must be judged dead");
        let elapsed = started.elapsed();

        assert!(
            matches!(
                error,
                crate::downloader::error::Error::HttpFlvReadTimeout { buffered: 4 }
            ),
            "unexpected error: {error:?}"
        );
        // 按配置的阈值判死，而不是等满 30 秒
        assert!(elapsed >= Duration::from_millis(300), "elapsed {elapsed:?}");
        assert!(elapsed < Duration::from_secs(5), "elapsed {elapsed:?}");
        // 静默口径覆盖「上游最后一个字节 → 判死」这一段
        let silent = connection.diagnostics().silent_for;
        assert!(silent >= Duration::from_millis(300), "silent {silent:?}");
        assert!(silent < Duration::from_millis(1000), "silent {silent:?}");
    }

    #[tokio::test]
    async fn stall_timeout_is_reset_by_every_chunk() {
        // 8 × 100ms = 800ms 总时长，远超 300ms 阈值；只要计时按 chunk 重置就不该超时。
        let mut connection = Connection::with_stall_timeout(
            dripping_response(b"ab", 8, Duration::from_millis(100)),
            Duration::from_millis(300),
        );
        let frame = connection
            .read_frame(16)
            .await
            .expect("steady stream must not trip the stall watchdog");
        assert_eq!(frame.len(), 16);
        assert_eq!(connection.diagnostics().received_bytes, 16);
    }

    #[tokio::test]
    async fn silent_for_tracks_the_time_since_the_last_byte() {
        let mut connection = Connection::with_stall_timeout(
            stalling_response(vec![b"abcd"]),
            Duration::from_secs(30),
        );
        connection.read_frame(4).await.expect("first frame");
        tokio::time::sleep(Duration::from_millis(250)).await;

        let diagnostics = connection.diagnostics();
        let silent = diagnostics.silent_for;
        assert!(
            silent >= Duration::from_millis(250) && silent < Duration::from_millis(600),
            "silent {silent:?}"
        );
        // 连接总时长与静默时长是两个口径，不能混用
        assert!(diagnostics.connected_for >= silent);
    }

    #[test]
    fn byte_it_works() -> Result<(), Box<dyn std::error::Error>> {
        let mut bb = bytes::BytesMut::with_capacity(10);
        println!("chunk {:?}", bb.chunk());
        println!("capacity {}", bb.capacity());
        bb.put(&b"hello"[..]);
        println!("chunk {:?}", bb.chunk());
        println!("remaining {}", bb.remaining());
        bb.advance(5);
        println!("capacity {}", bb.capacity());
        println!("chunk {:?}", bb.chunk());
        println!("remaining {}", bb.remaining());
        bb.put(&b"hello"[..]);
        bb.put(&b"hello"[..]);
        println!("chunk {:?}", bb.chunk());
        println!("capacity {}", bb.capacity());
        println!("remaining {}", bb.remaining());

        let mut buf = BytesMut::with_capacity(11);
        buf.put(&b"hello world"[..]);

        let other = buf.split();
        // buf.advance_mut()

        assert!(buf.is_empty());
        assert_eq!(0, buf.capacity());
        assert_eq!(11, other.capacity());
        assert_eq!(other, b"hello world"[..]);

        Ok(())
    }

    #[test]
    fn it_works() -> Result<(), Box<dyn std::error::Error>> {
        // download(
        //     "test.flv")?;
        Ok(())
    }
}
