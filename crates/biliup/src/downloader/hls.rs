use crate::downloader::error::{Error, Result};
use crate::downloader::util::{LifecycleFile, SegmentCloseReason, Segmentable};
use m3u8_rs::Playlist;

use std::fs::File;
use std::io::{BufWriter, Write};
use std::time::Duration;
use tracing::{debug, info, warn};
use url::Url;

use crate::client::StatelessClient;

pub async fn download(
    url: &str,
    client: &StatelessClient,
    file: LifecycleFile<'_>,
    splitting: Segmentable,
) -> Result<()> {
    download_with_ready(url, client, file, splitting, || {}).await
}

/// Notify the server only after a nonempty media segment has actually been received. Reading
/// nine playlist bytes (or an HTML error page) is not evidence of a recovered media connection.
pub async fn download_with_ready(
    url: &str,
    client: &StatelessClient,
    file: LifecycleFile<'_>,
    splitting: Segmentable,
    on_ready: impl FnOnce() + Send,
) -> Result<()> {
    let owner = file.owner().clone();
    let result = download_inner(url, client, file, splitting, on_ready).await;
    if let Err(error) = &result {
        let reason = match error {
            Error::HlsInvalidPlaylist => "invalid_playlist",
            Error::IOError(_) => "source_io",
            Error::ReqwestError(e) if e.status().is_some() => "http_error",
            Error::ReqwestError(e) if e.is_timeout() => "read_timeout",
            Error::UrlParseError(_) => "invalid_playlist",
            _ => "transport_error",
        };
        warn!(target: super::util::EVENT_TARGET,
            event_name = "recording.disconnected", outcome = "failed", reason_code = reason,
            stage = "hls", live_streamer_id = owner.live_streamer_id(),
            streamer_info_id = owner.streamer_info_id(), task_id = owner.task_id(),
            download_attempt_id = owner.download_attempt_id(), "HLS 下载未完成");
    }
    result
}

async fn download_inner(
    url: &str,
    client: &StatelessClient,
    file: LifecycleFile<'_>,
    mut splitting: Segmentable,
    on_ready: impl FnOnce() + Send,
) -> Result<()> {
    let mut on_ready = Some(on_ready);
    let stream_host = Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(ToString::to_string))
        .unwrap_or_else(|| "unknown".to_string());
    info!(stream_host, protocol = "hls", "starting hls download");
    let resp = client
        .retryable(url)
        .await
        .map_err(reqwest::Error::without_url)?;
    info!("{}", resp.status());
    // let mut resp = resp.bytes_stream();
    let bytes = resp.bytes().await.map_err(reqwest::Error::without_url)?;

    let mut media_url = Url::parse(url)?;
    let mut pl = match m3u8_rs::parse_playlist(&bytes) {
        Ok((_i, Playlist::MasterPlaylist(pl))) => {
            info!(
                variant_count = pl.variants.len(),
                "parsed hls master playlist"
            );
            // Pick the highest-bandwidth playable variant. The first variant is not
            // necessarily the best quality (e.g. Twitch orders transcodes ahead of the
            // source), so prefer the highest-bandwidth stream that carries a resolution.
            // Skip I-frame (trick-play) streams, which are not full playable renditions.
            // Fall back to the highest-bandwidth non-I-frame variant.
            let best = pl
                .variants
                .iter()
                .filter(|v| !v.is_i_frame && v.resolution.is_some())
                .max_by_key(|v| v.bandwidth)
                .or_else(|| {
                    pl.variants
                        .iter()
                        .filter(|v| !v.is_i_frame)
                        .max_by_key(|v| v.bandwidth)
                })
                .ok_or(Error::HlsInvalidPlaylist)?;
            info!(
                "Selected variant: bandwidth={}, resolution={:?}, video={:?}",
                best.bandwidth, best.resolution, best.video
            );
            media_url = media_url.join(&best.uri)?;
            info!(
                media_host = media_url.host_str().unwrap_or("unknown"),
                "selected hls media playlist"
            );
            let resp = client
                .retryable(media_url.as_str())
                .await
                .map_err(reqwest::Error::without_url)?;
            let bs = resp.bytes().await.map_err(reqwest::Error::without_url)?;
            // println!("{:?}", bs);
            match m3u8_rs::parse_media_playlist(&bs) {
                Ok((_, pl)) => pl,
                Err(_) => return Err(Error::HlsInvalidPlaylist),
            }
        }
        Ok((_i, Playlist::MediaPlaylist(pl))) => {
            info!(
                media_sequence = pl.media_sequence,
                segment_count = pl.segments.len(),
                "parsed hls media playlist"
            );
            pl
        }
        Err(_) => return Err(Error::HlsInvalidPlaylist),
    };
    let mut ts_file = TsFile::new(file)?;
    let mut previous_last_segment: Option<u64> = None;
    loop {
        if pl.segments.is_empty() && pl.end_list {
            info!("Segments array is empty - stream finished");
            break;
        }
        for (index, segment) in pl.segments.iter().enumerate() {
            let seq = pl
                .media_sequence
                .checked_add(index as u64)
                .ok_or(Error::HlsInvalidPlaylist)?;
            if previous_last_segment.is_none_or(|previous| seq > previous) {
                if let Some(previous) = previous_last_segment.filter(|previous| seq - previous > 1)
                {
                    warn!("SEGMENT INFO SKIPPED");
                    boundary(
                        &ts_file.file,
                        "recording.hls_gap",
                        "media_sequence_gap",
                        seq,
                        Some(previous),
                        Some(seq - previous - 1),
                    );
                }
                debug!("Yield segment");
                if segment.discontinuity {
                    warn!("#EXT-X-DISCONTINUITY");
                    ts_file.create_new(SegmentCloseReason::Unknown)?;
                    boundary(
                        &ts_file.file,
                        "recording.hls_discontinuity",
                        "hls_discontinuity",
                        seq,
                        None,
                        None,
                    );
                    // splitting = Segment::from_seg(splitting);
                    splitting.reset();
                }
                let length = download_to_file(
                    media_url.join(&segment.uri)?,
                    client,
                    &mut ts_file.buf_writer,
                )
                .await?;
                if length > 0
                    && let Some(on_ready) = on_ready.take()
                {
                    on_ready();
                }
                splitting.increase_size(length);
                let duration = Duration::try_from_secs_f64(segment.duration as f64)
                    .map_err(|_| Error::HlsInvalidPlaylist)?;
                splitting.increase_time(duration);
                if splitting.needed() {
                    let reason = if splitting.size_needed() {
                        SegmentCloseReason::SizeSplit
                    } else {
                        SegmentCloseReason::TimedSplit
                    };
                    ts_file.create_new(reason)?;
                    splitting.reset();
                }
                previous_last_segment = Some(seq);
            }
        }
        if pl.end_list {
            break;
        }
        // Do not spin on an unchanged live playlist; leave cancellation responsive.
        tokio::time::sleep(Duration::from_secs(pl.target_duration.max(1))).await;
        let resp = client
            .retryable(media_url.as_str())
            .await
            .map_err(reqwest::Error::without_url)?;
        let bs = resp.bytes().await.map_err(reqwest::Error::without_url)?;
        pl = m3u8_rs::parse_media_playlist(&bs)
            .map_err(|_| Error::HlsInvalidPlaylist)?
            .1;
    }
    info!("Done...");
    ts_file.finish(SegmentCloseReason::StreamEnded)?;
    Ok(())
}

fn boundary(
    file: &LifecycleFile<'_>,
    event_name: &str,
    reason: &str,
    sequence: u64,
    previous: Option<u64>,
    missing: Option<u64>,
) {
    let owner = file.owner();
    let identity = file.identity();
    warn!(target: super::util::EVENT_TARGET, event_name, outcome = "executed", reason_code = reason,
        media_sequence = sequence, previous_media_sequence = previous, missing_segments = missing,
        segment_id = identity.map(|i| i.segment_id.as_str()).unwrap_or(""),
        original_file = identity.map(|i| i.original_file.as_str()).unwrap_or(""),
        live_streamer_id = owner.live_streamer_id(), streamer_info_id = owner.streamer_info_id(),
        task_id = owner.task_id(), download_attempt_id = owner.download_attempt_id(),
        "HLS 媒体序列出现缺口或不连续，继续录制");
}

async fn download_to_file(url: Url, client: &StatelessClient, out: &mut impl Write) -> Result<u64> {
    debug!(
        segment_host = url.host_str().unwrap_or("unknown"),
        "downloading hls segment"
    );
    let mut response = client
        .retryable(url.as_str())
        .await
        .map_err(reqwest::Error::without_url)?;
    let mut length: u64 = 0;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(reqwest::Error::without_url)?
    {
        length += chunk.len() as u64;
        out.write_all(&chunk)?;
    }
    // let mut out = File::options()
    //     .append(true)
    //     .open(format!("{file_name}.ts"))?;
    // let length = response.copy_to(out)?;
    Ok(length)
}

pub struct TsFile<'a> {
    pub buf_writer: BufWriter<File>,
    pub file: LifecycleFile<'a>,
    drop_reason: SegmentCloseReason,
}

impl<'a> TsFile<'a> {
    pub fn new(mut file: LifecycleFile<'a>) -> std::io::Result<Self> {
        let path = file.create()?;
        Ok(Self {
            buf_writer: Self::create(path)?,
            file,
            drop_reason: SegmentCloseReason::TransportError,
        })
    }

    pub fn create_new(&mut self, reason: SegmentCloseReason) -> std::io::Result<()> {
        self.buf_writer.flush()?;
        self.file.finalize(reason)?;
        let path = self.file.create()?;
        self.buf_writer = Self::create(path)?;
        Ok(())
    }

    pub fn finish(&mut self, reason: SegmentCloseReason) -> std::io::Result<()> {
        self.buf_writer.flush()?;
        self.file.finalize(reason)
    }

    fn create<P: AsRef<std::path::Path>>(path: P) -> std::io::Result<BufWriter<File>> {
        let path = path.as_ref();
        let out = match File::create(path) {
            Ok(o) => o,
            Err(e) => {
                return Err(std::io::Error::new(
                    e.kind(),
                    format!("Unable to create file {}", path.display()),
                ));
            }
        };
        info!("create file {}", path.display());
        Ok(BufWriter::new(out))
    }
}

impl Drop for TsFile<'_> {
    fn drop(&mut self) {
        if self.file.is_active() {
            let _ = self.buf_writer.flush();
            let reason = self.file.fallback_reason(self.drop_reason);
            let _ = self.file.finalize(reason);
        }
    }
}

#[cfg(test)]
mod tests {
    use reqwest::Url;

    #[test]
    fn test_url() -> Result<(), Box<dyn std::error::Error>> {
        let url = Url::parse("h://host.path/to/remote/resource.m3u8")?;
        let scheme = url.scheme();
        let new_url = url.join("http://path.host/remote/resource.ts")?;
        println!("{url}, {scheme}");
        println!("{new_url}, {scheme}");
        Ok(())
    }

    #[test]
    fn it_works() -> Result<(), Box<dyn std::error::Error>> {
        // download(
        //     "test.ts")?;
        Ok(())
    }
}
