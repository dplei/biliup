use crate::error::{Kind, Result};
use futures::Stream;
use futures::StreamExt;

use reqwest::header::CONTENT_LENGTH;
use reqwest::{Body, header};

use serde::{Deserialize, Serialize};
use serde_json::json;
use std::ffi::OsStr;
use std::path::Path;
use std::time::{Duration, Instant};
use tracing::warn;

use crate::client::StatelessClient;
use crate::retry;
use crate::uploader::bilibili::Video;

pub struct Upos {
    client: StatelessClient,
    bucket: Bucket,
    url: String,
    upload_id: String,
    /// `upcdn` key, carried only so chunk failures can say which line they were on.
    line_key: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Bucket {
    pub chunk_size: usize,
    auth: String,
    endpoint: String,
    biz_id: usize,
    upos_uri: String,
}

/// 上传完成后仍能把源对象取回来所需的一切。
///
/// `auth` **只能在 preupload 时拿到**：事后重新 preupload 得到的新 auth 去访问旧对象是
/// 403，投稿账号的 Cookie 也是 403，凭证与具体对象绑定（实测见 dplei/biliup#13）。
/// 上传成功就把 `Bucket` 丢掉，等于永久失去灾后取回源文件的唯一通道——那次站内异步转码
/// 报错时，本地源文件已按钩子删除，日志和数据库里都没留下 auth。
///
/// 这里面的 `auth` 是凭证：**不要写进日志、事件或告警**。
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UposRecovery {
    pub endpoint: String,
    pub upos_uri: String,
    pub auth: String,
}

impl UposRecovery {
    /// 该对象的 URL。上传和下载是同一个地址，只是方法不同。
    pub fn object_url(&self) -> String {
        format!(
            "https:{}/{}",
            self.endpoint,
            self.upos_uri.replace("upos://", "")
        )
    }
}

impl Bucket {
    pub fn recovery(&self) -> UposRecovery {
        UposRecovery {
            endpoint: self.endpoint.clone(),
            upos_uri: self.upos_uri.clone(),
            auth: self.auth.clone(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Protocol<'a> {
    upload_id: &'a str,
    chunks: usize,
    total: u64,
    chunk: usize,
    size: usize,
    part_number: usize,
    start: u64,
    end: u64,
}

/// Per-request cap on one chunk PUT. With `retry`'s three attempts this bounds a single stuck
/// chunk at roughly thirteen minutes; the attempt-level watchdog fires long before that.
pub const CHUNK_REQUEST_TIMEOUT: Duration = Duration::from_secs(240);

impl Upos {
    pub async fn from(client: StatelessClient, bucket: Bucket, line_key: String) -> Result<Self> {
        let url = bucket.recovery().object_url(); // 视频上传路径
        let upload_id: serde_json::Value = client
            .client_with_middleware
            .post(format!("{url}?uploads&output=json"))
            .header("X-Upos-Auth", header::HeaderValue::from_str(&bucket.auth)?)
            .timeout(Duration::from_secs(60))
            .send()
            .await?
            .json()
            .await?;
        let upload_id = upload_id
            .get("upload_id")
            .and_then(|s| s.as_str())
            .ok_or_else(|| Kind::Custom(upload_id.to_string()))?
            .into();
        // = upload_id["upload_id"].as_str().unwrap().into();
        // let ret =  &upload.ret;
        // let chunk_size = ret["chunk_size"].as_u64().unwrap() as usize;
        // let auth = ret["auth"].as_str().unwrap();
        // let endpoint = ret["endpoint"].as_str().unwrap();
        // let biz_id = &ret["biz_id"];
        // let upos_uri = ret["upos_uri"].as_str().unwrap();
        Ok(Upos {
            client,
            bucket,
            url,
            upload_id,
            line_key,
        })
    }

    pub async fn upload_stream<'a, F, B>(
        &'a self,
        // file: std::fs::File,
        stream: F,
        total_size: u64,
        limit: usize,
    ) -> Result<impl Stream<Item = Result<(serde_json::Value, usize)>> + 'a>
    where
        F: Stream<Item = Result<(B, usize)>> + 'a,
        B: Into<Body> + Clone,
    {
        // let mut parts = Vec::new();

        // let total_size = file.metadata()?.len();
        // let parts = Vec::new();
        // let parts_cell = &RefCell::new(parts);
        let chunk_size = self.bucket.chunk_size;
        // 获取分块数量
        let chunks_num = (total_size as f64 / chunk_size as f64).ceil() as usize;
        // let file = tokio::io::BufReader::with_capacity(chunk_size, file);
        let client = &self.client.client;
        let url = &self.url;
        let upload_id = &*self.upload_id;
        let stream = stream
            // let mut chunks = read_chunk(file, chunk_size)
            .enumerate()
            .map(move |(i, chunk)| async move {
                let (chunk, len) = chunk?;
                // let len = chunk.len();
                // println!("{}", len);
                let params = Protocol {
                    upload_id,
                    chunks: chunks_num,
                    total: total_size,
                    chunk: i,
                    size: len,
                    part_number: i + 1,
                    start: i as u64 * chunk_size as u64,
                    end: i as u64 * chunk_size as u64 + len as u64,
                };
                // Chunk-level failures used to surface only as an untagged line inside `retry`,
                // so a post-mortem could not say which chunk on which line had stalled or for how
                // long. Each attempt now logs its own structured line.
                let mut attempt = 0usize;
                let params = &params;
                retry(|| {
                    attempt += 1;
                    let current_attempt = attempt;
                    let chunk = chunk.clone();
                    async move {
                        let started = Instant::now();
                        let result = async {
                            let response = client
                                .put(url)
                                .header(
                                    "X-Upos-Auth",
                                    header::HeaderValue::from_str(&self.bucket.auth)?,
                                )
                                .query(params)
                                .timeout(CHUNK_REQUEST_TIMEOUT)
                                .header(CONTENT_LENGTH, len)
                                .body(chunk)
                                .send()
                                .await?;
                            response.error_for_status()?;
                            Ok::<_, Kind>(())
                        }
                        .await;
                        if let Err(error) = &result {
                            warn!(
                                line = %self.line_key,
                                chunk_index = i,
                                attempt = current_attempt,
                                elapsed_ms = started.elapsed().as_millis() as u64,
                                chunk_bytes = len,
                                timeout_secs = CHUNK_REQUEST_TIMEOUT.as_secs(),
                                error = %error,
                                "upload chunk request failed"
                            );
                        }
                        result
                    }
                })
                .await?;

                Ok::<_, Kind>((json!({"partNumber": params.chunk + 1, "eTag": "etag"}), len))
            })
            .buffer_unordered(limit);
        Ok(stream)
    }

    /// 通知视频上传完成并获取视频信息
    pub(crate) async fn get_ret_video_info(
        &self,
        parts: &[serde_json::Value],
        path: &Path,
    ) -> Result<Video> {
        // println!("{:?}", parts_cell.borrow());
        let url = reqwest::Url::parse_with_params(
            &self.url,
            [
                (
                    "name",
                    path.file_name().and_then(OsStr::to_str).unwrap_or_default(),
                ),
                ("uploadId", &self.upload_id),
                ("biz_id", &self.bucket.biz_id.to_string()),
                ("output", "json"),
                ("profile", "ugcupos/bup"),
            ],
        )
        .map_err(|e| Kind::Custom(e.to_string()))?;
        let res: serde_json::Value = self
            .client
            .client_with_middleware
            .post(url)
            .header(
                "X-Upos-Auth",
                header::HeaderValue::from_str(&self.bucket.auth)?,
            )
            .json(&json!({ "parts": parts }))
            .timeout(Duration::from_secs(60))
            .send()
            .await?
            .json()
            .await?;
        if res["OK"] != 1 {
            return Err(Kind::Custom(res.to_string()));
        }
        let filename = Path::new(&self.bucket.upos_uri)
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap();

        // B站限制分P视频标题不能超过80字符，需要截断filename字段
        let truncated_filename = if filename.chars().count() >= 80 {
            Video::truncate_title(filename, 80)
        } else {
            filename.to_string()
        };

        Ok(Video {
            title: None,
            filename: truncated_filename,
            desc: "".into(),
        })
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;

    #[test]
    fn object_url_drops_the_upos_scheme_and_keeps_the_protocol_relative_endpoint() {
        let recovery = UposRecovery {
            endpoint: "//upos-example.bilivideo.com".into(),
            upos_uri: "upos://ugcexample/n000000000.flv".into(),
            auth: "irrelevant".into(),
        };
        assert_eq!(
            recovery.object_url(),
            "https://upos-example.bilivideo.com/ugcexample/n000000000.flv"
        );
    }

    /// 真实回归：每条线路上传一个几字节的临时对象，再用 preupload 当时拿到的
    /// `X-Upos-Auth` 把它取回来。
    ///
    /// 会在账号下产生真实的 preupload 与上传（**不投稿、不产生稿件**），所以默认 ignore，
    /// 且必须显式给出 cookie 路径——UID/cookie 文件名不进这个公开仓库：
    ///
    /// ```text
    /// BILIUP_TEST_COOKIES=<路径> cargo test -p biliup upos_recovery -- --ignored --nocapture
    /// ```
    ///
    /// 2026-09-02 实测结论（同一 bucket、同一 auth 机制，差别只在 endpoint）：
    ///
    /// | 线路 | HEAD | GET |
    /// | --- | --- | --- |
    /// | `bldsa`（B 站自建） | 200 | **403** |
    /// | `tx` / `bda` / `alia` | 200 | 200，逐字节一致 |
    ///
    /// 也就是说**灾后取回通道是按线路存在的**：走 bldsa 上传的分段，凭证能证明对象还在
    /// （HEAD 200），但拉不回内容。这条测试就是那个结论的守卫——B 站哪天改了策略，它会先响。
    #[tokio::test]
    #[ignore]
    async fn upos_recovery_round_trip_by_line() {
        let cookies = std::env::var("BILIUP_TEST_COOKIES")
            .expect("需要 BILIUP_TEST_COOKIES=<cookie 文件绝对路径>");
        let bili = crate::uploader::credential::login_by_cookies(&cookies, None)
            .await
            .expect("login_by_cookies");

        let payload = b"upos-recovery-probe";
        let probe = std::env::temp_dir().join("upos_recovery_probe.txt");
        std::fs::write(&probe, payload).expect("write probe");
        let client = StatelessClient::default();

        let mut downloadable = Vec::new();
        for (name, line) in [
            ("bldsa", crate::uploader::line::bldsa()),
            ("tx", crate::uploader::line::tx()),
            ("bda", crate::uploader::line::bda()),
            ("alia", crate::uploader::line::alia()),
        ] {
            let video_file = crate::uploader::VideoFile::new(&probe).expect("VideoFile");
            let parcel = match line.pre_upload(&bili, video_file).await {
                Ok(parcel) => parcel,
                Err(error) => {
                    eprintln!("[upos] {name}: pre_upload 失败 {error}");
                    continue;
                }
            };
            // 描述符必须在上传之前取走：`upload` 会消耗 parcel。
            let recovery = parcel.recovery();
            if let Err(error) = parcel
                .upload(client.clone(), 3, |stream| {
                    stream.map(|chunk| {
                        let chunk = chunk?;
                        let len = chunk.len();
                        Ok((chunk, len))
                    })
                })
                .await
            {
                eprintln!("[upos] {name}: upload 失败 {error}");
                continue;
            }

            let url = recovery.object_url();
            let anonymous = client.client.head(&url).send().await.expect("anonymous HEAD");
            assert_eq!(
                anonymous.status(),
                403,
                "{name}: 不带凭证不该访问得到对象"
            );
            let head = client
                .client
                .head(&url)
                .header("X-Upos-Auth", recovery.auth.clone())
                .send()
                .await
                .expect("authorized HEAD");
            assert_eq!(head.status(), 200, "{name}: 原始 auth 应当能确认对象还在");

            let response = client
                .client
                .get(&url)
                .header("X-Upos-Auth", recovery.auth.clone())
                .send()
                .await
                .expect("authorized GET");
            let status = response.status();
            let body = response.bytes().await.unwrap_or_default();
            eprintln!("[upos] {name}: endpoint={} GET={status}", recovery.endpoint);
            if status == 200 {
                assert_eq!(
                    body.as_ref(),
                    payload,
                    "{name}: GET 成功就必须逐字节一致，半截内容比拿不到更危险"
                );
                downloadable.push(name);
            }
        }

        let _ = std::fs::remove_file(&probe);
        assert!(
            !downloadable.is_empty(),
            "没有任何线路支持取回，灾后恢复通道已经不存在——先改 steps/06 的前提再说"
        );
        eprintln!("[upos] 可取回的线路: {downloadable:?}");
    }
}
