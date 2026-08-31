//! Independent invocation identities, typed failures and uncertain remote results.
use biliup::error::Kind;
use biliup::uploader::bilibili::{ResponseData, Studio};
use biliup::uploader::credential::bilibili_from_info;
use biliup::uploader::util::SubmitOption;
use biliup_cli::observe::{self, standalone::UploadTask};
use biliup_cli::server::errors::AppError;
use biliup_observability::*;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::instrument::WithSubscriber;
use tracing_subscriber::layer::SubscriberExt;

struct Memory(Arc<Mutex<Vec<Event>>>);
impl Consumer for Memory {
    fn write(&mut self, batch: &[Event]) -> Result<Commit, StorageError> {
        self.0.lock().unwrap().extend_from_slice(batch);
        Ok(Commit::default())
    }
}

fn response(code: i32, data: serde_json::Value) -> ResponseData {
    serde_json::from_value(serde_json::json!({"code":code,"data":data,"message":"synthetic"}))
        .unwrap()
}

#[tokio::test]
async fn independent_tasks_keep_identity_and_do_not_invent_remote_success() {
    let events = Arc::new(Mutex::new(Vec::<Event>::new()));
    let sink = events.clone();
    let mut runtime = Runtime::start(
        "synthetic",
        "test",
        Options {
            enabled: true,
            ..Options::default()
        },
        move || Ok(Memory(sink.clone())),
    )
    .unwrap();
    let subscriber =
        tracing_subscriber::registry().with(CaptureLayer::new(runtime.emitter()).filtered());
    async {
        // These futures deliberately interleave while carrying explicit invocation identities.
        let run = |label: &'static str| async move {
            let task = UploadTask::default();
            let file = task.file(std::path::Path::new("/private/input.flv"), 1);
            let first = file.with_attempt(&uuid::Uuid::new_v4().to_string());
            observe::upload_queued(&first, "awaiting_pre_upload");
            observe::standalone::failed(&first, &Kind::RateLimit {
                code: 601, message: "authorization=secret-value https://example.invalid/signed".into(),
            });
            tokio::task::yield_now().await;
            let retry = file.with_attempt(&uuid::Uuid::new_v4().to_string());
            observe::upload_started(&retry, "bda2", 1024);
            observe::upload_completed(&retry, "transferred", 1);
            task.submit(async { Ok(response(0, serde_json::json!({"aid":1}))) }).await.unwrap();
            (label, task.submission.task_id.unwrap(), first.upload_attempt_id.unwrap(), retry.upload_attempt_id.unwrap())
        };
        let (a,b) = tokio::join!(run("a"),run("b"));
        assert_ne!(a.1,b.1);
        assert_ne!(a.2,a.3);
        assert_ne!(b.2,b.3);
        for data in [serde_json::json!({}),serde_json::json!({"aid":0,"bvid":""})] {
            let task = UploadTask::default();
            task.submit(async { Ok(response(0,data)) }).await.unwrap();
        }
        let task = UploadTask::default();
        // A response error is observed without rewriting the caller's existing return value.
        task.submit(async { Ok(response(-1,serde_json::Value::Null)) }).await.unwrap();
        let task = UploadTask::default();
        assert!(task.submit(async { Err(AppError::Custom("synthetic lost response".into()).into()) }).await.is_err());

        // Exercise real early-return paths, not just the event helper.
        let dir = tempfile::tempdir().unwrap();
        assert!(biliup_cli::uploader::upload_by_command(serde_json::from_value::<Studio>(serde_json::json!({"tid":171,"title":"synthetic"})).unwrap(),dir.path().join("missing.json"),
            vec![dir.path().join("input.flv")],Some(biliup_cli::UploadLine::Bda2),1,SubmitOption::App,None).await.is_err());
        let bili = bilibili_from_info(serde_json::from_value(serde_json::json!({
            "cookie_info":{"cookies":[]}, "sso":[],
            "token_info":{"access_token":"synthetic","expires_in":0,"mid":0,"refresh_token":"synthetic"}
        })).unwrap(),None).unwrap();
        assert!(biliup_cli::uploader::upload(&[dir.path().join("missing.flv")],&bili,Some(biliup_cli::UploadLine::Bda2),1).await.is_err());
    }.with_subscriber(subscriber).await;
    assert!(runtime.shutdown(Duration::from_secs(2)).closed);
    let events = events.lock().unwrap();
    let native: Vec<_> = events
        .iter()
        .filter(|e| e.data().capture_kind == CaptureKind::Native)
        .collect();
    for e in &native {
        let fields = &e.data().fields;
        assert!(fields.get("task_id").is_some());
        for missing in [
            "segment_id",
            "upload_session_id",
            "live_streamer_id",
            "streamer_info_id",
            "missing_id",
        ] {
            assert!(
                fields.get(missing).is_none(),
                "independent input must not invent {missing}"
            );
        }
        if let Some(path) = fields.get("original_file") {
            assert!(!path.as_str().unwrap().contains('/'));
        }
    }
    let count = |name: &str, key: &str, value: &str| {
        native
            .iter()
            .filter(|e| {
                e.data().event_name == name && e.data().fields.get(key).is_some_and(|v| v == value)
            })
            .count()
    };
    assert_eq!(count("upload.failed", "reason_code", "rate_limited"), 2);
    assert_eq!(count("upload.failed", "reason_code", "source_io"), 1);
    assert_eq!(
        count("submission.decided", "reason_code", "authentication_failed"),
        1
    );
    assert_eq!(count("submission.completed", "outcome", "succeeded"), 2);
    assert_eq!(
        count("submission.completed", "reason_code", "missing_remote_id"),
        2
    );
    assert_eq!(count("submission.completed", "outcome", "failed"), 1);
    assert_eq!(
        count("submission.completed", "reason_code", "request_failed"),
        1
    );
    let encoded =
        serde_json::to_string(&events.iter().map(|e| e.data()).collect::<Vec<_>>()).unwrap();
    assert!(!encoded.contains("secret-value"));
    assert!(!encoded.contains("example.invalid"));
}
