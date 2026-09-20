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
    /// `upcdn` key of the line this parcel was pre-uploaded on. Carried so chunk-level failures
    /// can name the line they happened on — the incident post-mortem could not tell which line a
    /// stalled chunk belonged to.
    line_key: String,
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
    /// 本次 preupload 拿到的取回描述符。**必须在上传前后取走并持久化**：`auth` 事后无法
    /// 重新申请，见 [`UposRecovery`](upos::UposRecovery)。
    pub fn recovery(&self) -> upos::UposRecovery {
        match &self.line {
            Bucket::Upos(bucket) => bucket.recovery(),
        }
    }

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
                let upos = Upos::from(client, bucket, self.line_key.clone()).await?;
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

#[derive(Debug)]
pub struct ProbeFailure {
    pub line_key: String,
    pub error: crate::error::Kind,
}

fn choose_line_and_failures<I>(candidates: I) -> Result<(Line, Vec<ProbeFailure>)>
where
    I: IntoIterator<Item = (Line, Option<crate::error::Kind>)>,
{
    let mut failures = Vec::new();
    let line = choose_fastest_successful_line(candidates.into_iter().map(|(line, error)| {
        let succeeded = error.is_none();
        if let Some(error) = error {
            failures.push(ProbeFailure {
                line_key: line.key().to_string(),
                error,
            });
        }
        (line, succeeded)
    }))?;
    Ok((line, failures))
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
const DEFAULT_PROBE_POST_MB: f64 = 0.1;
const MAX_PROBE_POST_MB: f64 = 10.0;

impl Probe {
    pub async fn probe(client: &reqwest::Client) -> Result<Line> {
        Self::probe_excluding(client, &[]).await
    }

    pub async fn probe_excluding(client: &reqwest::Client, excluded: &[String]) -> Result<Line> {
        Self::probe_excluding_with_failures(client, excluded)
            .await
            .map(|(line, _)| line)
    }

    pub async fn probe_excluding_with_failures(
        client: &reqwest::Client,
        excluded: &[String],
    ) -> Result<(Line, Vec<ProbeFailure>)> {
        Self::probe_filtered_with_failures(client, &[], excluded).await
    }

    /// 同上，但可以额外**限定**只在 `allowed` 这几条线路里选。`allowed` 为空表示不限定。
    ///
    /// 调用方用它表达「优先只考虑某个子集」——例如只在能把源对象 GET 回来的线路里挑，
    /// 子集全军覆没时再退回不限定的探测。
    pub async fn probe_filtered_with_failures(
        client: &reqwest::Client,
        allowed: &[String],
        excluded: &[String],
    ) -> Result<(Line, Vec<ProbeFailure>)> {
        let res = Self::fetch_index(client).await?;

        let lines = retained_lines(res.lines, allowed, excluded);
        let total_lines = lines.len();
        let probe = res.probe;
        let client = client.clone();
        let probe_lines = futures::stream::iter(lines.into_iter().map(|line| {
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

        choose_line_and_failures(candidates)
    }

    /// B 站当前公布的线路索引。
    async fn fetch_index(client: &reqwest::Client) -> Result<Self> {
        Ok(client
            .get("https://member.bilibili.com/preupload?r=probe")
            .timeout(PROBE_INDEX_TIMEOUT)
            .send()
            .await?
            .json()
            .await?)
    }

    /// 索引里每条线路的 `upcdn` key，按 B 站给出的顺序。页面下拉靠它渲染，不再维护登记表。
    pub async fn index_keys(client: &reqwest::Client) -> Result<Vec<String>> {
        Ok(Self::fetch_index(client).await?.keys())
    }

    fn keys(&self) -> Vec<String> {
        self.lines.iter().map(|line| line.key().to_string()).collect()
    }

    async fn probe_line(
        probe: serde_json::Value,
        mut line: Line,
        client: reqwest::Client,
    ) -> (Line, Option<crate::error::Kind>) {
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
                (line, None)
            }
            Ok(resp) => {
                let status = resp.status();
                warn!(query = %line.query, %status, "upload line probe returned non-success status");
                (
                    line,
                    Some(Custom(format!("upload line probe returned HTTP {status}"))),
                )
            }
            Err(err) => {
                warn!(query = %line.query, error = %err, "upload line probe failed");
                (line, Some(err.into()))
            }
        }
    }

    fn ping(probe: &serde_json::Value, url: &str, client: &reqwest::Client) -> RequestBuilder {
        if !probe["get"].is_null() {
            client.get(url)
        } else {
            client.post(url).body(vec![0; probe_post_bytes(probe)])
        }
    }
}

fn probe_post_bytes(probe: &serde_json::Value) -> usize {
    let mb = probe["post"]
        .as_f64()
        .filter(|mb| mb.is_finite() && *mb > 0.0)
        .unwrap_or(DEFAULT_PROBE_POST_MB)
        .min(MAX_PROBE_POST_MB);
    (mb * 1024.0 * 1024.0) as usize
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
    pub fn key(&self) -> &str {
        self.query
            .split('&')
            .find_map(|part| part.strip_prefix("upcdn="))
            .unwrap_or("auto")
    }

    /// 按 `upcdn` key 构造一条显式线路。
    ///
    /// B 站 `preupload?r=probe` 索引里每条线路的 `query` 都是 `zone=cs&upcdn=<key>&probe_version=…`，
    /// 只有 key 不同，所以显式选线不需要预先登记表。`probe_url` 只在 AUTO 探测时读取，显式路径
    /// 直接 `pre_upload`，这里留空。
    pub fn explicit(upcdn: &str) -> Line {
        Line {
            os: Uploader::Upos,
            query: format!("zone=cs&upcdn={upcdn}&probe_version=20221109"),
            probe_url: String::new(),
            cost: 0,
        }
    }

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
                line_key: self.key().to_string(),
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

/// 探测前的候选过滤：先剔除 `excluded`，再在 `allowed` 非空时只保留其中的线路。
fn retained_lines(lines: Vec<Line>, allowed: &[String], excluded: &[String]) -> Vec<Line> {
    lines
        .into_iter()
        .filter(|line| !excluded.iter().any(|key| key == line.key()))
        .filter(|line| allowed.is_empty() || allowed.iter().any(|key| key == line.key()))
        .collect()
}

#[cfg(test)]
mod probe_filter_tests {
    use super::*;

    fn keys(lines: &[Line]) -> Vec<&str> {
        lines.iter().map(Line::key).collect()
    }

    fn owned(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn empty_allowed_means_no_restriction() {
        let lines = vec![
            Line::explicit("bldsa"),
            Line::explicit("bda2"),
            Line::explicit("tx"),
        ];
        assert_eq!(
            keys(&retained_lines(lines, &[], &[])),
            ["bldsa", "bda2", "tx"]
        );
    }

    #[test]
    fn allowed_keeps_only_the_listed_lines() {
        let lines = vec![
            Line::explicit("bldsa"),
            Line::explicit("bda2"),
            Line::explicit("tx"),
            Line::explicit("alia"),
        ];
        assert_eq!(
            keys(&retained_lines(lines, &owned(&["bda2", "tx"]), &[])),
            ["bda2", "tx"]
        );
    }

    /// 冷却优先于白名单：一条既在白名单又在冷却里的线路必须被剔除。
    #[test]
    fn excluded_wins_over_allowed() {
        let lines = vec![Line::explicit("bda2"), Line::explicit("tx")];
        assert_eq!(
            keys(&retained_lines(
                lines,
                &owned(&["bda2", "tx"]),
                &owned(&["bda2"])
            )),
            ["tx"]
        );
    }

    #[test]
    fn an_allow_list_matching_nothing_yields_no_candidate() {
        let lines = vec![Line::explicit("bldsa")];
        assert!(retained_lines(lines, &owned(&["bda2"]), &[]).is_empty());
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

    #[test]
    fn successful_auto_probe_preserves_other_line_failures_for_breaker() {
        let mut healthy = Line::explicit("bda2");
        healthy.cost = 20;
        let (selected, failures) = choose_line_and_failures(vec![
            (
                Line::explicit("bldsa"),
                Some(Custom("certificate has expired".to_string())),
            ),
            (healthy, None),
        ])
        .unwrap();

        assert_eq!(selected.key(), "bda2");
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].line_key, "bldsa");
    }

    /// 显式线路只靠 key 就能构造：`pre_upload` 只读 `query`，而 `query` 里除 key 外全是常量。
    #[test]
    fn explicit_line_carries_the_key_in_its_query() {
        for key in ["bldsa", "bda2", "tx", "alia", "estx"] {
            let line = Line::explicit(key);
            assert_eq!(line.key(), key);
            let params: std::collections::BTreeSet<&str> = line.query.split('&').collect();
            let upcdn = format!("upcdn={key}");
            assert_eq!(
                params,
                ["zone=cs", upcdn.as_str(), "probe_version=20221109"]
                    .into_iter()
                    .collect()
            );
        }
    }

    /// 2026-09-20 `preupload?r=probe` 的真实响应形状（probe_url 域名保留，auth 无关）。
    #[test]
    fn index_keys_follow_the_order_bilibili_publishes() {
        let index: Probe = serde_json::from_value(serde_json::json!({
            "OK": 1,
            "lines": [
                {"os": "upos", "query": "probe_version=20221109&upcdn=estx&zone=cs", "probe_url": "//e17962d5cstx.esheep.com/OK"},
                {"os": "upos", "query": "probe_version=20221109&upcdn=akbd&zone=cs", "probe_url": "//bb27c891csbd.aikobo.cn/OK"},
                {"os": "upos", "query": "probe_version=20221109&upcdn=bldsa&zone=cs", "probe_url": "//upos-cs-upcdnbldsa.bilivideo.com/OK"},
                {"os": "upos", "query": "probe_version=20221109&upcdn=bda2&zone=cs", "probe_url": "//upos-cs-upcdnbda2.bilivideo.com/OK"},
                {"os": "upos", "query": "probe_version=20221109&upcdn=tx&zone=cs", "probe_url": "//upos-cs-upcdntx.bilivideo.com/OK"}
            ],
            "probe": {"post": 0.1}
        }))
        .unwrap();
        assert_eq!(index.keys(), ["estx", "akbd", "bldsa", "bda2", "tx"]);
    }

    #[test]
    fn probe_body_follows_server_hint_with_bounded_fallback() {
        let hinted = probe_post_bytes(&serde_json::json!({"post": 0.1}));
        assert_eq!(hinted, (0.1 * 1024.0 * 1024.0) as usize);
        assert!(hinted * PROBE_CONCURRENCY < 512 * 1024);
        assert_eq!(
            probe_post_bytes(&serde_json::json!({})),
            (DEFAULT_PROBE_POST_MB * 1024.0 * 1024.0) as usize
        );
        assert_eq!(
            probe_post_bytes(&serde_json::json!({"post": -1})),
            (DEFAULT_PROBE_POST_MB * 1024.0 * 1024.0) as usize
        );
        assert_eq!(
            probe_post_bytes(&serde_json::json!({"post": 1000})),
            (MAX_PROBE_POST_MB * 1024.0 * 1024.0) as usize
        );
    }
}
