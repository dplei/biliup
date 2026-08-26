use crate::error::Result;
use crate::uploader::{Uploader, VideoFile, VideoStream};
use futures::{Stream, StreamExt, TryStreamExt};
use reqwest::{Body, RequestBuilder};

use serde::{Deserialize, Serialize};
use serde_json::json;
use std::ffi::OsStr;

use crate::client::StatelessClient;
use crate::error::Kind::{Custom, RateLimit};
use crate::uploader::bilibili::{BiliBili, Video};
use crate::uploader::line::upos::Upos;
use std::time::{Duration, Instant};
use tokio::time::{Instant as TokioInstant, sleep_until};
use tracing::{info, warn};

pub mod upos;

pub struct Parcel {
    // line: &'a Line,
    line: Bucket,
    video_file: VideoFile,
}

/// Progress reported only after the upload server has accepted a chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadProgress {
    pub chunk_bytes: usize,
    pub uploaded_bytes: u64,
    pub total_bytes: u64,
    pub chunk_index: usize,
}

impl Parcel {
    pub async fn upload<F, S, B>(
        self,
        client: StatelessClient,
        limit: usize,
        progress: F,
    ) -> Result<Video>
    where
        F: FnOnce(VideoStream) -> S,
        S: Stream<Item = Result<(B, usize)>>,
        B: Into<Body> + Clone,
    {
        self.upload_with_observer(client, limit, progress, |_| {})
            .await
    }

    pub async fn upload_with_observer<F, S, B, O>(
        self,
        client: StatelessClient,
        limit: usize,
        progress: F,
        mut observer: O,
    ) -> Result<Video>
    where
        F: FnOnce(VideoStream) -> S,
        S: Stream<Item = Result<(B, usize)>>,
        B: Into<Body> + Clone,
        O: FnMut(UploadProgress),
    {
        let mut video = match self.line {
            Bucket::Upos(bucket) => {
                // let bucket: crate::uploader::upos::Bucket = self.pre_upload(client).await?;
                let chunk_size = bucket.chunk_size;
                let upos = Upos::from(client, bucket).await?;
                let mut parts = Vec::new();
                let stream = upos
                    .upload_stream(
                        progress(self.video_file.get_stream(chunk_size)?),
                        self.video_file.total_size,
                        limit,
                    )
                    .await?;
                tokio::pin!(stream);
                let mut uploaded_bytes = 0u64;
                while let Some((part, size)) = stream.try_next().await? {
                    uploaded_bytes = uploaded_bytes.saturating_add(size as u64);
                    let chunk_index = part
                        .get("partNumber")
                        .and_then(serde_json::Value::as_u64)
                        .and_then(|part| usize::try_from(part.saturating_sub(1)).ok())
                        .unwrap_or(parts.len());
                    observer(UploadProgress {
                        chunk_bytes: size,
                        uploaded_bytes,
                        total_bytes: self.video_file.total_size,
                        chunk_index,
                    });
                    parts.push(part);
                }
                upos.get_ret_video_info(&parts, &self.video_file.filepath)
                    .await?
            }
        };

        if video.title.is_none()
            && let Some(filename) = self.video_file.filepath.file_stem().and_then(OsStr::to_str)
        {
            // B站限制分P视频标题不能超过80字符，需要截断
            video.title = Some(if filename.chars().count() >= 80 {
                Video::truncate_title(filename, 80)
            } else {
                filename.to_string()
            });
        };
        Ok(video)
    }
}

#[derive(Deserialize, Serialize, Debug)]
pub struct Probe {
    #[serde(rename = "OK")]
    ok: u8,
    lines: Vec<Line>,
    probe: serde_json::Value,
}

pub fn choose_fastest_successful_line<I>(candidates: I) -> Result<Line>
where
    I: IntoIterator<Item = (Line, bool)>,
{
    candidates
        .into_iter()
        .filter_map(|(line, ok)| ok.then_some(line))
        .min_by_key(|line| line.cost)
        .ok_or_else(|| Custom("no upload line probe succeeded".to_string()))
}

const PROBE_INDEX_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_LINE_TIMEOUT: Duration = Duration::from_secs(4);
const PROBE_TOTAL_TIMEOUT: Duration = Duration::from_secs(10);
const PROBE_CONCURRENCY: usize = 4;

impl Probe {
    pub async fn probe(client: &reqwest::Client) -> Result<Line> {
        let res: Self = client
            .get("https://member.bilibili.com/preupload?r=probe")
            .timeout(PROBE_INDEX_TIMEOUT)
            .send()
            .await?
            .json()
            .await?;

        let total_lines = res.lines.len();
        let probe = res.probe;
        let client = client.clone();
        let probe_lines = futures::stream::iter(res.lines.into_iter().map(|line| {
            let probe = probe.clone();
            let client = client.clone();
            async move { Probe::probe_line(probe, line, client).await }
        }))
        .buffer_unordered(PROBE_CONCURRENCY);

        let deadline = TokioInstant::now() + PROBE_TOTAL_TIMEOUT;
        let mut candidates = Vec::new();
        tokio::pin!(probe_lines);
        loop {
            tokio::select! {
                _ = sleep_until(deadline) => {
                    warn!(
                        completed = candidates.len(),
                        total = total_lines,
                        timeout_ms = PROBE_TOTAL_TIMEOUT.as_millis(),
                        "upload line probe total deadline elapsed"
                    );
                    break;
                }
                candidate = probe_lines.next() => {
                    match candidate {
                        Some(candidate) => candidates.push(candidate),
                        None => break,
                    }
                }
            }
        }

        choose_fastest_successful_line(candidates)
    }

    async fn probe_line(
        probe: serde_json::Value,
        mut line: Line,
        client: reqwest::Client,
    ) -> (Line, bool) {
        let url = format!("https:{}", line.probe_url);
        let instant = Instant::now();
        let ping_result = Probe::ping(&probe, &url, &client)
            .timeout(PROBE_LINE_TIMEOUT)
            .send()
            .await;
        match ping_result {
            Ok(resp) if resp.status().is_success() => {
                line.cost = instant.elapsed().as_millis();
                info!(query = %line.query, cost = line.cost, "upload line probe succeeded");
                (line, true)
            }
            Ok(resp) => {
                let status = resp.status();
                warn!(query = %line.query, %status, "upload line probe returned non-success status");
                (line, false)
            }
            Err(err) => {
                warn!(query = %line.query, error = %err, "upload line probe failed");
                (line, false)
            }
        }
    }

    fn ping(probe: &serde_json::Value, url: &str, client: &reqwest::Client) -> RequestBuilder {
        if !probe["get"].is_null() {
            client.get(url)
        } else {
            client
                .post(url)
                .body(vec![0; (1024. * 1024. * 10.) as usize]) // 10MB chunk
        }
    }
}

enum Bucket {
    Upos(upos::Bucket),
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Line {
    os: Uploader,
    probe_url: String,
    query: String,
    #[serde(skip)]
    cost: u128,
}

impl Line {
    pub async fn pre_upload(&self, bili: &BiliBili, video_file: VideoFile) -> Result<Parcel> {
        let total_size = video_file.total_size;
        let file_name = video_file.file_name.clone();
        let profile = "ugcupos/bup"; // ugcfx/bup 需上传视频metadata和frame.zip
        let params = json!({
            // "probe_version": "20221109",
            // "upcdn": "",
            // "zone": "",
            "name": file_name,
            "r": self.os, // upos
            "profile": profile,
            "ssl": 0,
            "version": "2.14.0",
            "build": 2140000,
            "size": total_size,
        });
        info!("pre_upload: {}", params);

        let response = bili
            .client
            .get(format!(
                "https://member.bilibili.com/preupload?{}",
                self.query
            ))
            .query(&params)
            .send()
            .await?;

        let status = response.status();
        let response_bytes = response.bytes().await?;
        // B 站在不同网关上可能用非 2xx，也可能用 HTTP 200 + JSON code 表达 601。
        // 必须在反序列化线路 bucket 前统一识别，避免限流被降级成普通 JSON 错误。
        if let Some(error) = parse_rate_limit(&response_bytes) {
            return Err(error);
        }

        if !status.is_success() {
            let summary: String = String::from_utf8_lossy(&response_bytes)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(2048)
                .collect();
            return Err(Custom(format!(
                "Failed to pre_upload with HTTP {status}: {summary}"
            )));
        }

        match self.os {
            Uploader::Upos => Ok(Parcel {
                line: Bucket::Upos(serde_json::from_slice(&response_bytes)?),
                video_file,
            }),
            // _ => {
            //     panic!("unsupported")
            // }
        }
    }
}

fn parse_rate_limit(bytes: &[u8]) -> Option<crate::error::Kind> {
    let error_json = serde_json::from_slice::<serde_json::Value>(bytes).ok()?;
    let code = error_json.get("code").and_then(|code| code.as_i64())?;
    if code != 601 {
        return None;
    }
    let message = error_json
        .get("message")
        .and_then(|message| message.as_str())
        .unwrap_or("上传过快")
        .to_string();
    Some(RateLimit { code, message })
}

impl Default for Line {
    fn default() -> Self {
        Line {
            cost: u128::MAX,
            ..bldsa()
        }
    }
}

#[cfg(test)]
mod rate_limit_tests {
    use super::parse_rate_limit;
    use crate::error::Kind;

    #[test]
    fn recognizes_601_even_when_http_status_would_be_successful() {
        let error = parse_rate_limit(r#"{"code":601,"message":"上传过快"}"#.as_bytes())
            .expect("601 should be recognized before bucket decoding");
        assert!(matches!(error, Kind::RateLimit { code: 601, .. }));
    }

    #[test]
    fn ordinary_json_is_not_a_rate_limit() {
        assert!(parse_rate_limit(br#"{"OK":1}"#).is_none());
    }
}

/// B站自建DSA
pub fn bldsa() -> Line {
    Line {
        os: Uploader::Upos,
        query: "zone=cs&upcdn=bldsa&probe_version=20221109".into(),
        probe_url: "//upos-cs-upcdnbldsa.bilivideo.com/OK".into(),
        cost: 0,
    }
}

/// B站自建DSA
pub fn cnbldsa() -> Line {
    Line {
        os: Uploader::Upos,
        query: "zone=cs&upcdn=cnbldsa&probe_version=20221109".into(),
        probe_url: "//upos-cs-upcdnbldsa.bilivideo.cn/OK".into(),
        cost: 0,
    }
}

/// B站自建DSA
pub fn andsa() -> Line {
    Line {
        os: Uploader::Upos,
        query: "zone=cs&upcdn=andsa&probe_version=20221109".into(),
        probe_url: "//c3350892csdsa.anitama.cn/OK".into(),
        cost: 0,
    }
}

/// B站自建DSA
pub fn atdsa() -> Line {
    Line {
        os: Uploader::Upos,
        query: "zone=cs&upcdn=atdsa&probe_version=20221109".into(),
        probe_url: "//c3350892csdsa.anitama.net/OK".into(),
        cost: 0,
    }
}

/// 百度云
pub fn bda2() -> Line {
    Line {
        os: Uploader::Upos,
        query: "probe_version=20221109&upcdn=bda2&zone=cs".into(),
        probe_url: "//upos-cs-upcdnbda2.bilivideo.com/OK".into(),
        cost: 0,
    }
}

/// 百度云
pub fn cnbd() -> Line {
    Line {
        os: Uploader::Upos,
        query: "probe_version=20221109&upcdn=cnbd&zone=cs".into(),
        probe_url: "//upos-cs-upcdnbd.bilivideo.cn/OK".into(),
        cost: 0,
    }
}

/// 百度云
pub fn anbd() -> Line {
    Line {
        os: Uploader::Upos,
        query: "probe_version=20221109&upcdn=anbd&zone=cs".into(),
        probe_url: "//c3350892csbd.anitama.cn/OK".into(),
        cost: 0,
    }
}

/// 百度云
pub fn atbd() -> Line {
    Line {
        os: Uploader::Upos,
        query: "probe_version=20221109&upcdn=atbd&zone=cs".into(),
        probe_url: "//c3350892csbd.anitama.net/OK".into(),
        cost: 0,
    }
}

/// 腾讯云EO
pub fn tx() -> Line {
    Line {
        os: Uploader::Upos,
        query: "zone=cs&upcdn=tx&probe_version=20221109".into(),
        probe_url: "//upos-cs-upcdntx.bilivideo.com/OK".into(),
        cost: 0,
    }
}

/// 腾讯云EO
pub fn cntx() -> Line {
    Line {
        os: Uploader::Upos,
        query: "zone=cs&upcdn=cntx&probe_version=20221109".into(),
        probe_url: "//upos-cs-upcdntx.bilivideo.com/OK".into(),
        cost: 0,
    }
}

/// 腾讯云EO
pub fn antx() -> Line {
    Line {
        os: Uploader::Upos,
        query: "zone=cs&upcdn=antx&probe_version=20221109".into(),
        probe_url: "//c3350892cstx.anitama.cn/OK".into(),
        cost: 0,
    }
}

/// 腾讯云EO
pub fn attx() -> Line {
    Line {
        os: Uploader::Upos,
        query: "zone=cs&upcdn=attx&probe_version=20221109".into(),
        probe_url: "//c3350892cstx.anitama.net/OK".into(),
        cost: 0,
    }
}

/// 百度云海外（Cloudflare）
pub fn bda() -> Line {
    Line {
        os: Uploader::Upos,
        query: "zone=cs&upcdn=bda&probe_version=20221109".into(),
        probe_url: "//upos-cs-upcdnbda.bilivideo.com/OK".into(),
        cost: 0,
    }
}

/// 腾讯云EO海外
pub fn txa() -> Line {
    Line {
        os: Uploader::Upos,
        query: "zone=cs&upcdn=txa&probe_version=20221109".into(),
        probe_url: "//upos-cs-upcdntxa.bilivideo.com/OK".into(),
        cost: 0,
    }
}

/// 阿里云海外
pub fn alia() -> Line {
    Line {
        os: Uploader::Upos,
        query: "zone=cs&upcdn=alia&probe_version=20221109".into(),
        probe_url: "//upos-cs-upcdnalia.bilivideo.com/OK".into(),
        cost: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_with_cost(query: &str, cost: u128) -> Line {
        Line {
            os: Uploader::Upos,
            probe_url: format!("//{query}.example.com/OK"),
            query: query.to_string(),
            cost,
        }
    }

    #[test]
    fn choose_fastest_successful_line_ignores_failures() {
        let candidates = vec![
            (line_with_cost("slow", 300), true),
            (line_with_cost("down", 10), false),
            (line_with_cost("fast", 20), true),
        ];

        let selected = choose_fastest_successful_line(candidates).unwrap();

        assert_eq!(selected.query, "fast");
        assert_eq!(selected.cost, 20);
    }

    #[test]
    fn choose_fastest_successful_line_fails_when_all_fail() {
        let candidates = vec![
            (line_with_cost("down-1", 10), false),
            (line_with_cost("down-2", 20), false),
        ];

        let err = choose_fastest_successful_line(candidates).unwrap_err();

        assert!(err.to_string().contains("no upload line probe succeeded"));
    }
}
