use crate::downloader::flv_parser::{
    AACPacketType, AVCPacketType, CodecId, FrameType, SoundFormat, TagData, TagHeader, TagType,
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
    reason_code: &'static str,
    count: u64,
    first_ms: u64,
    last_ms: u64,
    max_backward_ms: u64,
}

impl DtsBackwardRollup {
    fn record(
        &mut self,
        file: &LifecycleFile<'_>,
        previous_ms: u64,
        current_ms: u64,
        emitted_ms: u64,
        reason_code: &'static str,
    ) {
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
            self.reason_code = reason_code;
            emit_dts_first(file, previous_ms, current_ms, emitted_ms, reason_code);
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
        self.reason_code = "";
        self.first_ms = 0;
        self.last_ms = 0;
        self.max_backward_ms = 0;
    }
}

/// 段内允许的最大前跳。与 `util.rs` 的 `MAX_STEP` 同源同理由：段内的真实空档由停顿看门狗
/// （默认 30s 不来字节就断连重连，重连会重进 `parse_flv`）兜住，超过它的前跳只可能是换基准。
const REBASE_MAX_STEP_MS: i64 = 30_000;
/// 换基准处给新基准留的名义间隔。取 10ms 是因为它小于任何真实帧间隔（60fps 约 16ms，
/// 一帧 AAC 约 23ms），既保证同流严格递增，又不会在 CDN 逐帧交替重发时把时长撑长。
const REBASE_NOMINAL_GAP_MS: i64 = 10;

#[derive(Default, Clone, Copy)]
struct StreamBase {
    /// 本流已确认基准上的最后一个源时间戳
    last_src: Option<i64>,
    /// 本流最后写出的时间戳
    last_emit: Option<i64>,
    /// 已见过一次、还等第二个样本确认的候选新基准
    pending: Option<i64>,
}

/// 写盘侧的时间戳重基。
///
/// CDN 在分段中途重发 script tag 并换时间基准（通常归零，偶尔大幅前跳）时，原样落盘的
/// 时间戳会让下游解复用器按 32 位 wrap 展开成天文数字，转码据此拒稿（issue #13）。
/// 这里在写出前统一套一层 `emit = src + offset`，`offset` 只在基准确认改变时更新。
///
/// 连续性判据按 **tag 类型分别** 维护：audio 与 video 交错送达，跨流比较会把每个合法交错的
/// audio tag 误判成换基准，把音画同步打散；各自流内则本来就是单调的。修正量 `offset` 反过来
/// 必须全局唯一，同基准内的相对关系（音画同步）才原样保留。
#[derive(Default)]
struct TimestampRebase {
    offset: i64,
    /// 已写出的最大时间戳。换基准时新基准接在它之后，不能用「上一个写出值」——
    /// 合法交错本来就允许小于它。
    high_water: i64,
    streams: [StreamBase; 3],
}

struct Mapped {
    emit: u32,
    /// 本 tag 偏离了所在流的基准：`(该流上一个源时间戳, reason_code)`。
    deviation: Option<(u64, &'static str)>,
}

/// `to` 是否仍落在以 `from` 为基准的同一条时间轴上。允许停在原地（CDN 重发同一时间戳），
/// 不允许倒退，也不允许超过看门狗阈值的前跳。
fn follows(from: i64, to: i64) -> bool {
    to >= from && to - from <= REBASE_MAX_STEP_MS
}

impl TimestampRebase {
    fn map(&mut self, tag_type: TagType, src: u32) -> Mapped {
        let slot = match tag_type {
            TagType::Audio => 0,
            TagType::Video => 1,
            TagType::Script => 2,
        };
        let src = src as i64;
        let stream = self.streams[slot];
        let mut deviation = None;
        let emit = match stream.last_src {
            // 本流的第一个 tag：没有判据可用，沿用当前基准。
            None => {
                self.streams[slot].last_src = Some(src);
                src + self.offset
            }
            Some(last) if follows(last, src) => {
                self.streams[slot].last_src = Some(src);
                // 时钟停在原地不算推进，不能清掉待确认的新基准，否则 CDN 逐帧交替重发
                // （`[0, B+1000, 0, B+2000, …]`）时新基准永远等不到第二个样本，整条时间轴
                // 会被压成每帧 `REBASE_NOMINAL_GAP_MS`。`util.rs` 的 `set_time_position`
                // 靠 `number == last` 提前返回拿到同一效果。
                if src > last {
                    self.streams[slot].pending = None;
                }
                src + self.offset
            }
            Some(last) => {
                deviation = Some((
                    last as u64,
                    if src < last {
                        "timestamp_backward"
                    } else {
                        "timestamp_jump_forward"
                    },
                ));
                match stream.pending {
                    // 连续两个样本落在同一条新时间轴上：确认换基准，把它接到已写出的最大值之后。
                    Some(pending) if follows(pending, src) => {
                        self.offset = self.high_water + REBASE_NOMINAL_GAP_MS - src;
                        self.streams[slot].last_src = Some(src);
                        self.streams[slot].pending = None;
                        // 其它流也一起换了基准，但它们各自的新起点还没见过：清掉判据，让下一个
                        // tag 直接落到新 offset 上，而不是再触发一次重基。
                        for other in 0..self.streams.len() {
                            if other != slot {
                                self.streams[other].last_src = None;
                                self.streams[other].pending = None;
                            }
                        }
                        src + self.offset
                    }
                    // 首次偏离：还分不清是孤立噪声（重发的初始化帧）还是新基准，先接在已写出的
                    // 最大值之后，`last_src` 不动，等下一个样本表态。
                    _ => {
                        self.streams[slot].pending = Some(src);
                        self.high_water + REBASE_NOMINAL_GAP_MS
                    }
                }
            }
        };
        // 同一个 tag 类型的输出必须严格递增；跨类型的小幅倒退是合法交错，不碰。
        let emit = match self.streams[slot].last_emit {
            Some(last_emit) => emit.max(last_emit + 1),
            None => emit,
        }
        .clamp(0, u32::MAX as i64);
        self.streams[slot].last_emit = Some(emit);
        self.high_water = self.high_water.max(emit);
        Mapped {
            emit: emit as u32,
            deviation,
        }
    }
}

/// 切段时重发的段首 prelude（onMetaData / sequence header）要带当前时间戳。沿用建档时的
/// 旧值（常是 0）会在每次切段制造一次假的基准跳变。
fn restamp(tag: &(TagHeader, Bytes, Bytes), timestamp: u32) -> (TagHeader, Bytes, Bytes) {
    let mut header = tag.0;
    header.timestamp = timestamp;
    (header, tag.1.clone(), tag.2.clone())
}

fn emit_dts_first(
    file: &LifecycleFile<'_>,
    previous_ms: u64,
    current_ms: u64,
    emitted_ms: u64,
    reason_code: &'static str,
) {
    let owner = file.owner();
    warn!(
        target: EVENT_TARGET,
        event_name = "recording.dts_backward",
        outcome = "executed",
        reason_code,
        segment_id = segment_id(file),
        original_file = original_file(file),
        previous_ms,
        current_ms,
        emitted_ms,
        live_streamer_id = owner.live_streamer_id(),
        streamer_info_id = owner.streamer_info_id(),
        task_id = owner.task_id(),
        download_attempt_id = owner.download_attempt_id(),
        "检测到时间戳基准跳变，已重基后继续录制"
    );
}

fn emit_dts_summary(file: &LifecycleFile<'_>, rollup: &DtsBackwardRollup) {
    let owner = file.owner();
    warn!(
        target: EVENT_TARGET,
        event_name = "recording.dts_backward",
        outcome = "executed",
        reason_code = rollup.reason_code,
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
        "本分段时间戳基准跳变汇总"
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
    // 写出前的时间戳重基。跨段保持，不在切段处复位：各段仍是「CDN 绝对时间轴 + 累计修正」。
    let mut rebase = TimestampRebase::default();
    // 本段起点是否已锚定。曾经用「上一批写出 tag 的最后一个时间戳是否为 0」代替这个状态，
    // 但抖音重发的 timestamp=0 Script tag 会让下一个关键帧误判成「流刚初始化」，把 start
    // 推到当前，定时分段从此失效（issue #32）。
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
                    let source_ms = tag_header.timestamp;
                    let mapped = rebase.map(tag_header.tag_type, source_ms);
                    if let Some((previous_ms, reason_code)) = mapped.deviation {
                        warn!(
                            "Non-monotonous DTS in output stream; previous: {previous_ms}, current: {source_ms}, written as {};",
                            mapped.emit
                        );
                        dts_rollup.record(
                            &out.file,
                            previous_ms,
                            source_ms as u64,
                            mapped.emit as u64,
                            reason_code,
                        );
                    }
                    let mut tag_header = *tag_header;
                    tag_header.timestamp = mapped.emit;
                    out.write_tag(&tag_header, flv_tag_data, previous_tag_size_bytes)?;
                    segment.increase_size((11 + tag_header.data_size + 4) as u64);
                    // 进度记的是 CDN 的原始时间戳：它的用途是跨连接相减算缺口，和文件里写的
                    // 重基后时间轴不是一回事（见 `FlvProgress` 的字段说明）。
                    progress.first_timestamp_ms.get_or_insert(source_ms as u64);
                    progress.last_timestamp_ms = Some(source_ms as u64);
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

                    // onMetaData
                    flv_tags_cache.push(restamp(
                        on_meta_data.as_ref().expect("on_meta_data does not exist"),
                        tag_header.timestamp,
                    ));
                    // AACSequenceHeader
                    flv_tags_cache.push(restamp(
                        aac_sequence_header
                            .as_ref()
                            .expect("aac_sequence_header does not exist"),
                        tag_header.timestamp,
                    ));
                    if !create_new {
                        // H264SequenceHeader
                        flv_tags_cache.push(restamp(
                            h264_sequence_header
                                .as_ref()
                                .expect("h264_sequence_header does not exist"),
                            tag_header.timestamp,
                        ));
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
            rollup.record(&file, 1_000, 400, 1_010, "timestamp_backward");
            rollup.record(&file, 1_200, 900, 1_020, "timestamp_backward");
            rollup.record(&file, 1_500, 100, 1_030, "timestamp_backward");

            // A split allocates a new identity; the counts must not leak across it.
            file.create().unwrap();
            let second_id = file.identity().unwrap().segment_id.clone();
            rollup.record(&file, 2_000, 1_900, 2_010, "timestamp_backward");
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
        assert_eq!(names[0]["emitted_ms"], "1010", "事件要说清实际写出的值");
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

    /// 重基是 `parse_flv` 唯一改写落盘时间戳的地方，判据全在 `TimestampRebase` 里，
    /// 所以这一组直接喂时间戳序列，不跑网络也不落文件。
    mod rebase {
        use super::super::{Mapped, TimestampRebase};
        use crate::downloader::flv_parser::TagType;

        fn video(rebase: &mut TimestampRebase, src: u32) -> u32 {
            rebase.map(TagType::Video, src).emit
        }

        fn video_mapped(rebase: &mut TimestampRebase, src: u32) -> Mapped {
            rebase.map(TagType::Video, src)
        }

        /// 回归底线：没有跳变时本改动必须是恒等映射。
        #[test]
        fn a_clean_stream_is_mapped_one_to_one() {
            let mut rebase = TimestampRebase::default();
            for src in [0, 40, 80, 120, 30_000, 30_040] {
                assert_eq!(video(&mut rebase, src), src, "src {src}");
            }
        }

        /// audio/video 交错送达时流内各自单调，判据按 tag 类型分开维护，
        /// 因此两条流都保持恒等，相对关系原样保留。
        #[test]
        fn interleaved_audio_and_video_keep_their_own_timeline() {
            let mut rebase = TimestampRebase::default();
            let feed = [
                (TagType::Video, 1_000u32),
                (TagType::Audio, 980),
                (TagType::Video, 1_040),
                (TagType::Audio, 1_020),
                (TagType::Video, 1_080),
                (TagType::Audio, 1_060),
            ];
            for (tag_type, src) in feed {
                let mapped = rebase.map(tag_type, src);
                assert_eq!(mapped.emit, src, "{tag_type:?} {src}");
                assert!(mapped.deviation.is_none(), "合法交错不该被判成基准跳变");
            }
        }

        /// issue #13 的真实形态：段中途换基准后一直用新基准。
        /// 跳变处只推进一个名义间隔，之后按真实增量累加。
        #[test]
        fn a_single_rebase_keeps_the_real_frame_intervals() {
            let mut rebase = TimestampRebase::default();
            for src in [98_000, 99_000, 100_000] {
                video(&mut rebase, src);
            }
            let jump = video_mapped(&mut rebase, 0);
            assert_eq!(jump.emit, 100_010);
            assert_eq!(
                jump.deviation,
                Some((100_000, "timestamp_backward"))
            );
            assert_eq!(video(&mut rebase, 1_000), 100_020, "第二个样本确认新基准");
            assert_eq!(video(&mut rebase, 2_000), 101_020);
            assert_eq!(video(&mut rebase, 3_000), 102_020, "此后按真实增量累加");
        }

        /// 换基准不一定归零，也可能跳到更大的值——不能当成真的过了这么久。
        #[test]
        fn a_forward_jump_beyond_the_watchdog_is_a_rebase_too() {
            let mut rebase = TimestampRebase::default();
            video(&mut rebase, 1_000);
            video(&mut rebase, 2_000);
            let jump = video_mapped(&mut rebase, 3_600_000);
            assert_eq!(jump.emit, 2_010);
            assert_eq!(
                jump.deviation.map(|(_, reason)| reason),
                Some("timestamp_jump_forward")
            );
            assert_eq!(video(&mut rebase, 3_601_000), 2_020);
            assert_eq!(video(&mut rebase, 3_602_000), 3_020);
        }

        /// spec 断言「交替重发下不需要二次确认」——推演结果是**反的**：没有 `pending`
        /// 时每一帧都走不连续分支，10 秒内容会被压成 100 毫秒。这条测试锁住真实增量必须留下。
        #[test]
        fn alternating_resends_do_not_collapse_the_timeline() {
            const BASE: u32 = 32_891_256;
            let mut rebase = TimestampRebase::default();
            let mut emits = Vec::new();
            for index in 0..10u32 {
                // CDN 重发的初始化关键帧：时间戳恒为 0
                emits.push(video(&mut rebase, 0));
                emits.push(video(&mut rebase, BASE + (index + 1) * 1_000));
            }
            assert!(
                emits.windows(2).all(|pair| pair[0] < pair[1]),
                "同一条流写出的时间戳必须严格递增：{emits:?}"
            );
            let span = emits.last().unwrap() - emits.first().unwrap();
            assert!(
                (8_000..12_000).contains(&span),
                "10 帧 × 1s 的真实时长必须保住，实测 {span}ms：{emits:?}"
            );
        }

        /// 孤立的一帧噪声不该改基准：下一帧回到原基准时，原基准继续。
        #[test]
        fn one_off_noise_does_not_move_the_base() {
            let mut rebase = TimestampRebase::default();
            video(&mut rebase, 1_000);
            video(&mut rebase, 2_000);
            assert_eq!(video(&mut rebase, 0), 2_010, "噪声接在已写出的最大值之后");
            assert_eq!(video(&mut rebase, 3_000), 3_000, "基准没变，恒等映射继续");
        }
    }

    /// 段中途换基准的完整码流：audio/video 交错、中途时间戳归零，并且跨越一次切段。
    /// 落盘文件里每条流的时间戳都必须严格递增——这是 issue #13 里被 B 站转码拒稿的那个性质。
    fn flv_with_a_mid_stream_base_change(frames: usize, payload: usize) -> Vec<u8> {
        const BASE: u32 = 100_000;
        let mut bytes = vec![b'F', b'L', b'V', 1, 5, 0, 0, 0, 9, 0, 0, 0, 0];
        let mut metadata = vec![0x02, 0x00, 0x0a];
        metadata.extend_from_slice(b"onMetaData");
        metadata.push(0x05);
        append_tag(&mut bytes, 18, &metadata, 0);
        append_tag(&mut bytes, 8, &[0xaf, 0x00, 0x12, 0x10], 0);
        append_tag(&mut bytes, 9, &[0x17, 0x00, 0, 0, 0, 0x01, 0x64, 0x00], 0);
        for index in 0..frames {
            let index = index as u32;
            // 后半段 CDN 重发 script tag 并把基准换成从 0 起算
            let timestamp = if (index as usize) < frames / 2 {
                BASE + (index + 1) * 1_000
            } else {
                if index as usize == frames / 2 {
                    append_tag(&mut bytes, 18, &metadata, 0);
                }
                (index + 1 - frames as u32 / 2) * 1_000
            };
            // audio 比 video 早 20ms 到：合法交错，不该被判成换基准
            let mut audio = vec![0xaf, 0x01];
            audio.resize(2 + payload / 4, 0x41);
            append_tag(&mut bytes, 8, &audio, timestamp.saturating_sub(20));
            let mut frame = vec![0x17, 0x01, 0, 0, 0];
            frame.resize(5 + payload, 0x41);
            append_tag(&mut bytes, 9, &frame, timestamp);
        }
        bytes
    }

    /// 按 FLV 结构走一遍落盘文件，返回 `(tag_type, timestamp)`。
    fn written_tags(path: &std::path::Path) -> Vec<(u8, u32)> {
        let data = std::fs::read(path).unwrap();
        let mut offset = 13;
        let mut tags = Vec::new();
        while offset + 11 <= data.len() {
            let size = u32::from_be_bytes([0, data[offset + 1], data[offset + 2], data[offset + 3]])
                as usize;
            let timestamp = u32::from_be_bytes([
                data[offset + 7],
                data[offset + 4],
                data[offset + 5],
                data[offset + 6],
            ]);
            tags.push((data[offset], timestamp));
            offset += 11 + size + 4;
        }
        tags
    }

    #[tokio::test]
    async fn a_mid_stream_base_change_is_written_as_a_monotonous_timeline() {
        let directory = tempfile::tempdir().unwrap();
        // 同一秒内切段会撞同名文件，用纳秒占位符把两段分开
        let template = directory.path().join("rebased-%f").display().to_string();
        let captured = Captured::default();
        let _guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(captured.clone()));

        let mut connection =
            Connection::new(complete_response(flv_with_a_mid_stream_base_change(10, 4_000)));
        connection.read_frame(9).await.unwrap();
        let file = crate::downloader::util::LifecycleFile::new(&template, "flv");
        // 切一刀，顺带覆盖段首 prelude 重发
        let segment = crate::downloader::util::Segmentable::new(None, Some(20_000));
        let mut progress = super::FlvProgress::default();
        super::parse_flv(&mut connection, file, segment, &mut progress)
            .await
            .unwrap();

        let mut files: Vec<_> = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "flv"))
            .collect();
        files.sort();
        assert!(files.len() >= 2, "本用例要覆盖切段，实测 {} 个文件", files.len());

        for path in &files {
            let tags = written_tags(path);
            assert!(tags.len() > 3, "{path:?} 没写出内容");
            for tag_type in [8u8, 9, 18] {
                let stamps: Vec<_> = tags
                    .iter()
                    .filter(|(kind, _)| *kind == tag_type)
                    .map(|(_, timestamp)| *timestamp)
                    .collect();
                assert!(
                    stamps.windows(2).all(|pair| pair[0] < pair[1]),
                    "{path:?} 的 tag_type={tag_type} 时间戳不单调：{stamps:?}"
                );
            }
        }

        let events = captured.0.lock().unwrap().clone();
        assert!(
            events.iter().any(|fields| {
                fields.get("event_name").map(String::as_str) == Some("recording.dts_backward")
            }),
            "换基准仍然要留下事件，运维才知道文件被重基过"
        );
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

