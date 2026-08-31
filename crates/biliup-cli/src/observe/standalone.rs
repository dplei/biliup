//! Observation context for independent upload invocations. It never owns business state.

use super::{SubmissionIdentity, UploadIdentity, submission_completed, submission_decided};
use crate::server::errors::AppResult;
use biliup::error::Kind;
use biliup::uploader::bilibili::ResponseData;
use std::path::Path;

pub struct UploadTask {
    pub submission: SubmissionIdentity,
}

impl Default for UploadTask {
    fn default() -> Self {
        let task = Self {
            submission: SubmissionIdentity {
                task_id: Some(uuid::Uuid::new_v4().to_string()),
                ..Default::default()
            },
        };
        submission_decided(&task.submission, "waiting", "preparing_upload", 0);
        task
    }
}

impl UploadTask {
    /// User supplied files have no known recording segment or durable upload session.
    /// Never infer those identities from a path or a CLI checkpoint.
    pub fn file(&self, path: &Path, position: usize) -> UploadIdentity {
        UploadIdentity {
            task_id: self.submission.task_id.clone(),
            original_file: Some(path.to_string_lossy().into_owned()),
            segment_order: Some(position as i64),
            ..Default::default()
        }
    }

    /// Before a remote submission starts, a local failure is known, not an uncertain submit.
    pub fn check<T>(&self, result: AppResult<T>, reason: &str) -> AppResult<T> {
        result.inspect_err(|_| submission_decided(&self.submission, "failed", reason, 0))
    }

    pub async fn submit(
        &self,
        future: impl std::future::Future<Output = AppResult<ResponseData>>,
    ) -> AppResult<ResponseData> {
        super::submission_started(&self.submission, "files_ready");
        let result = future.await;
        let (outcome, reason) = submission_result(&result);
        submission_completed(&self.submission, outcome, reason);
        result
    }
}

/// Do not parse the core uploader's untyped error strings into a remote outcome. An error
/// after starting a request can mean a lost response; only an explicit result proves success.
pub fn submission_result(result: &AppResult<ResponseData>) -> (&'static str, &'static str) {
    match result {
        Ok(response) if response.code != 0 => ("failed", "remote_error"),
        Ok(response)
            if response.data.as_ref().is_some_and(|data| {
                data["aid"].as_u64().is_some_and(|id| id > 0)
                    || data["bvid"].as_str().is_some_and(|id| !id.is_empty())
            }) =>
        {
            ("succeeded", "submitted")
        }
        Ok(_) => ("unknown", "missing_remote_id"),
        Err(_) => ("unknown", "request_failed"),
    }
}

/// Inspect typed errors before callers erase them into a generic application report.
/// No raw response, signed URL, account id or credentials enter these events.
pub fn failure_reason(error: &Kind) -> &'static str {
    match error {
        Kind::RateLimit { .. } => "rate_limited",
        Kind::IO(_) => "source_io",
        Kind::Reqwest(_) | Kind::ReqwestMiddleware(_) => "network_error",
        Kind::SerdeJson(_) | Kind::SerdeYaml(_) | Kind::SerdeUrl(_) => "invalid_response",
        _ => "remote_error",
    }
}

pub fn failed(identity: &UploadIdentity, error: &Kind) {
    super::upload_failed(identity, failure_reason(error), "上传操作未完成");
}
