use crate::downloader::flv_parser::{
    AACPacketType, AVCPacketType, CodecId, FrameType, SoundFormat, TagData, TagHeader,
    aac_audio_packet_header, avc_video_packet_header, script_data, tag_data, tag_header,
};
use crate::downloader::flv_writer::{FlvFile, FlvTag, TagDataHeader};
use crate::downloader::util::{LifecycleFile, SegmentCloseReason, Segmentable};
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

pub async fn download_with_context(
    mut connection: Connection,
    file: LifecycleFile<'_>,
    segment: Segmentable,
    log_context: HttpFlvLogContext,
) -> crate::downloader::error::Result<()> {
    download_inner(&mut connection, file, segment, Some(&log_context)).await
}

async fn download_inner(
    connection: &mut Connection,
    file: LifecycleFile<'_>,
    segment: Segmentable,
    log_context: Option<&HttpFlvLogContext>,
) -> crate::downloader::error::Result<()> {
    let file_name = file.file_name.clone();
    match parse_flv(connection, file, segment).await {
        Ok(_) => {
            info!("Done... {}", file_name);
            Ok(())
        }
        Err(e) => {
            let diagnostics = connection.diagnostics();
            warn!(
                error = ?e,
                http_status = diagnostics.http_status,
                content_encoding = diagnostics.content_encoding.as_deref().unwrap_or("none"),
                transfer_encoding = diagnostics.transfer_encoding.as_deref().unwrap_or("none"),
                received_bytes = diagnostics.received_bytes,
                connected_ms = diagnostics.connected_for.as_millis() as u64,
                buffered = diagnostics.buffered,
                attempt_id = log_context.map(|context| context.attempt_id.as_str()).unwrap_or("untracked"),
                stream_host = log_context.map(|context| context.stream_host.as_str()).unwrap_or("unknown"),
                protocol = log_context.map(|context| context.protocol.as_str()).unwrap_or("flv"),
                quality = log_context.and_then(|context| context.quality.as_deref()).unwrap_or("unknown"),
                "httpflv download failed"
            );
            Err(e)
        }
    }
}

pub(crate) async fn parse_flv(
    connection: &mut Connection,
    file: LifecycleFile<'_>,
    mut segment: Segmentable,
) -> crate::downloader::error::Result<()> {
    let mut flv_tags_cache: Vec<(TagHeader, Bytes, Bytes)> = Vec::new();
    // println!("parse_flv Segment: {:?}", segment);
    let _previous_tag_size = connection.read_frame(4).await?;

    let mut out = FlvFile::new(file)?;
    let result: crate::downloader::error::Result<()> = async {
    segment.set_size_position(9 + 4);
    // let mut downloaded_size = 9 + 4;
    let mut on_meta_data = None;
    let mut aac_sequence_header = None;
    let mut h264_sequence_header: Option<(TagHeader, Bytes, Bytes)> = None;
    let mut prev_timestamp = 0;
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
                if prev_timestamp == 0 && timestamp != 0 {
                    segment.set_start_time(Duration::from_millis(timestamp));
                }
                segment.set_time_position(Duration::from_millis(timestamp));
                for (tag_header, flv_tag_data, previous_tag_size_bytes) in &flv_tags_cache {
                    if tag_header.timestamp < prev_timestamp {
                        warn!(
                            "Non-monotonous DTS in output stream; previous: {prev_timestamp}, current: {};",
                            tag_header.timestamp
                        );
                    }
                    out.write_tag(tag_header, flv_tag_data, previous_tag_size_bytes)?;
                    segment.increase_size((11 + tag_header.data_size + 4) as u64);
                    // downloaded_size += (11 + tag_header.data_size + 4) as u64;
                    prev_timestamp = tag_header.timestamp
                    // println!("{downloaded_size}");
                }
                flv_tags_cache.clear();

                if segment.needed() || create_new {
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
                    let reason = if segment.size_needed() {
                        SegmentCloseReason::SizeSplit
                    } else if segment.time_needed() {
                        SegmentCloseReason::TimedSplit
                    } else {
                        SegmentCloseReason::Unknown
                    };
                    out.create_new(reason)?;
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

pub struct Connection {
    resp: Response,
    buffer: BytesMut,
    http_status: u16,
    content_encoding: Option<String>,
    transfer_encoding: Option<String>,
    received_bytes: u64,
    started_at: Instant,
}

#[derive(Debug, Clone)]
pub struct ConnectionDiagnostics {
    pub http_status: u16,
    pub content_encoding: Option<String>,
    pub transfer_encoding: Option<String>,
    pub received_bytes: u64,
    pub connected_for: Duration,
    pub buffered: usize,
}

impl Connection {
    pub fn new(resp: Response) -> Connection {
        let http_status = resp.status().as_u16();
        let content_encoding = header_value(&resp, reqwest::header::CONTENT_ENCODING);
        let transfer_encoding = header_value(&resp, reqwest::header::TRANSFER_ENCODING);
        Connection {
            resp,
            buffer: BytesMut::with_capacity(8 * 1024),
            http_status,
            content_encoding,
            transfer_encoding,
            received_bytes: 0,
            started_at: Instant::now(),
        }
    }

    pub fn diagnostics(&self) -> ConnectionDiagnostics {
        ConnectionDiagnostics {
            http_status: self.http_status,
            content_encoding: self.content_encoding.clone(),
            transfer_encoding: self.transfer_encoding.clone(),
            received_bytes: self.received_bytes,
            connected_for: self.started_at.elapsed(),
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
            match timeout(Duration::from_secs(30), self.resp.chunk()).await {
                Ok(Ok(Some(chunk))) => {
                    self.received_bytes = self.received_bytes.saturating_add(chunk.len() as u64);
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
                    warn!(error = %err, buffered, "httpflv chunk read timed out");
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
    use bytes::{Buf, BufMut, BytesMut};

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
