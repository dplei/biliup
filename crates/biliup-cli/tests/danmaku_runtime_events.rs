//! A danmaku recorder that dies *after* a successful start.
//!
//! `download()` only spawns the recorder, so everything that fails later has no return value and
//! no state to inspect: before this boundary existed the session simply produced no danmaku and
//! said nothing. The event reports the classification only — the raw error stays in the unchanged
//! log line, and a session that ends normally produces no event at all.
use biliup_cli::observe::RecordingIdentity;
use biliup_cli::server::core::downloader::{DanmakuClient, RustDanmakuClient};
use biliup_observability::*;
use danmaku_client::RecorderConfig;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing_subscriber::layer::SubscriberExt;

struct Memory(Arc<Mutex<Vec<Event>>>);
impl Consumer for Memory {
    fn write(&mut self, batch: &[Event]) -> Result<Commit, StorageError> {
        self.0.lock().unwrap().extend_from_slice(batch);
        Ok(Commit::default())
    }
}

fn runtime(sink: Arc<Mutex<Vec<Event>>>) -> Runtime {
    Runtime::start(
        "synthetic",
        "test",
        Options {
            enabled: true,
            ..Options::default()
        },
        move || Ok(Memory(sink.clone())),
    )
    .unwrap()
}

fn field(data: &EventData, key: &str) -> String {
    data.fields
        .get(key)
        .map(|value| value.as_str().unwrap_or_default().to_owned())
        .unwrap_or_default()
}

/// An output path that cannot be created: the recorder dies while opening its XML file, which is
/// the earliest and least visible way for a started session to end.
fn dying_client(identity: RecordingIdentity) -> RustDanmakuClient {
    let config = RecorderConfig::new(
        "https://www.twitch.tv/shroud",
        "/dev/null/unwritable/danmaku",
    );
    RustDanmakuClient::with_identity(config, identity)
}

#[cfg(unix)]
#[tokio::test]
async fn a_recorder_that_dies_after_start_is_reported_once_with_this_session_identity() {
    let events = Arc::new(Mutex::new(Vec::<Event>::new()));
    let mut rt = runtime(events.clone());
    let subscriber =
        tracing_subscriber::registry().with(CaptureLayer::new(rt.emitter()).filtered());
    {
        let _guard = tracing::subscriber::set_default(subscriber);
        let client = dying_client(RecordingIdentity::server(7, 42, "room"));
        // The caller is told the danmaku recording started, because it did.
        client.download().await.expect("start only spawns the task");

        for _ in 0..200 {
            if !events.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Stopping a recorder that is already dead stays harmless.
        client.stop().await.expect("stop is infallible");
    }
    // Draining first: a batch that is still queued is not yet evidence of anything.
    rt.shutdown(Duration::from_secs(5));

    let collected = events.lock().unwrap().clone();
    let reported: Vec<EventData> = collected
        .iter()
        .map(Event::data)
        .filter(|data| data.event_name == "recording.auxiliary_failed")
        .cloned()
        .collect();
    assert_eq!(reported.len(), 1, "the end is reported exactly once");

    let data = &reported[0];
    assert_eq!(field(data, "stage"), "danmaku_runtime");
    assert_eq!(field(data, "outcome"), "failed");
    assert_eq!(field(data, "reason_code"), "danmaku_output_failed");
    assert_eq!(data.level, Level::Warn);
    // The identity is the one the caller passed in, not something recovered from a file name.
    assert_eq!(field(data, "live_streamer_id"), "7");
    assert_eq!(field(data, "streamer_info_id"), "42");
    // The classification travels; the underlying error text does not.
    assert!(
        !data.message.contains("/dev/null"),
        "the event must not carry the failing path: {}",
        data.message
    );
}

/// The collection is additive: with no collector installed the same failing recorder must still
/// start, still be stoppable, and still not disturb the caller.
#[cfg(unix)]
#[tokio::test]
async fn the_business_path_is_unchanged_when_collection_is_off() {
    let client = dying_client(RecordingIdentity::server(7, 42, "room"));
    client.download().await.expect("start only spawns the task");
    tokio::time::sleep(Duration::from_millis(200)).await;
    client.stop().await.expect("stop is infallible");
}
