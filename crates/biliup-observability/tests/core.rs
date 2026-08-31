use biliup_observability::*;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tracing_subscriber::{Layer, layer::SubscriberExt};

struct Memory(Arc<Mutex<Vec<Event>>>);
impl Consumer for Memory {
    fn write(&mut self, batch: &[Event]) -> Result<Commit, StorageError> {
        self.0.lock().unwrap().extend_from_slice(batch);
        Ok(Commit::default())
    }
}
fn memory() -> (Runtime, Arc<Mutex<Vec<Event>>>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let runtime = Runtime::start(
        "synthetic",
        "test",
        Options {
            enabled: true,
            bridge: true,
            ..Options::default()
        },
        move || Ok(Memory(sink.clone())),
    )
    .unwrap();
    (runtime, events)
}

#[test]
fn snapshot_delayed_record_nested_override_and_ids() {
    let (mut runtime, events) = memory();
    let subscriber =
        tracing_subscriber::registry().with(CaptureLayer::new(runtime.emitter()).filtered());
    tracing::subscriber::with_default(subscriber, || {
        let parent = tracing::info_span!(
            "recording",
            streamer_info_id = 1,
            upload_session_id = 2,
            segment_id = tracing::field::Empty
        );
        let _parent = parent.enter();
        parent.record("segment_id", "segment-a");
        tracing::info!(target: "biliup::event", event_name="recording.started", "开始录制");
        parent.record("segment_id", "segment-b");
        let child = tracing::info_span!(
            "upload",
            segment_id = "segment-child",
            upload_attempt_id = "attempt-1"
        );
        let _child = child.enter();
        tracing::warn!(target: "biliup::event", event_name="upload.failed", segment_id="segment-event", "上传失败");
        tracing::info!("旧诊断");
    });
    assert!(runtime.shutdown(Duration::from_secs(2)).closed);
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(
        events[0].data().fields.get("segment_id").unwrap(),
        "segment-a"
    );
    assert_eq!(
        events[1].data().fields.get("segment_id").unwrap(),
        "segment-event"
    );
    assert_eq!(
        events[1].data().fields.get("streamer_info_id").unwrap(),
        "1"
    );
    assert_eq!(
        events[1].data().fields.get("upload_session_id").unwrap(),
        "2"
    );
    assert_eq!(events[2].data().capture_kind, CaptureKind::LegacyBridge);
    assert_ne!(events[0].data().event_uid, events[1].data().event_uid);
}

#[test]
fn explicit_carriers_survive_async_blocking_channel_and_callback_interleaving() {
    use tracing::{Instrument, instrument::WithSubscriber};
    let (mut runtime, events) = memory();
    let emitter = runtime.emitter();
    let subscriber =
        tracing_subscriber::registry().with(CaptureLayer::new(emitter.clone()).filtered());
    let dispatch = tracing::Dispatch::new(subscriber);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut tasks = Vec::new();
        for task in ["alpha", "beta"] {
            let span = tracing::dispatcher::with_default(&dispatch, || {
                tracing::info_span!("task", task_id = task)
            });
            let dispatch = dispatch.clone();
            let emitter = emitter.clone();
            tasks.push(tokio::spawn(
                async move {
                    tokio::task::yield_now().await;
                    tracing::info!(target:"biliup::event", event_name="system.started", "开始");
                    let context = Context(Fields::new().with("task_id", task));
                    let (tx, rx) = std::sync::mpsc::channel();
                    tx.send(context).unwrap();
                    tokio::task::spawn_blocking(move || {
                        let context = rx.recv().unwrap();
                        let callback = || {
                            emitter.emit_with(Level::Info, || {
                                let mut d = Draft::new("system.stopped", "完成");
                                d.context = context.clone();
                                d
                            })
                        };
                        assert!(callback());
                    })
                    .await
                    .unwrap();
                }
                .instrument(span)
                .with_subscriber(dispatch),
            ));
        }
        for t in tasks {
            t.await.unwrap();
        }
    });
    runtime.shutdown(Duration::from_secs(2));
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 4);
    for name in ["alpha", "beta"] {
        assert_eq!(
            events
                .iter()
                .filter(|e| e.data().fields.get("task_id").unwrap() == name)
                .count(),
            2
        );
    }
}

#[test]
fn safety_unknown_debug_bounded_error_and_chunked_diagnostic() {
    struct Forbidden;
    impl std::fmt::Debug for Forbidden {
        fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            panic!("unknown fields must never format");
        }
    }
    let (mut runtime, events) = memory();
    let subscriber =
        tracing_subscriber::registry().with(CaptureLayer::new(runtime.emitter()).filtered());
    tracing::subscriber::with_default(subscriber, || {
        tracing::warn!(target:"biliup::event", event_name="upload.failed", request=?Forbidden,
            error=%"错误".repeat(10_000), "失败 Authorization: Bearer synthetic-secret");
        tracing::warn!(
            url = "https://example.invalid/?sign=synthetic",
            "带签名 https://example.invalid/?sign=synthetic"
        );
        // The old uploader prints the complete typed response, not `aid=123` text.
        tracing::info!(
            target: "legacy::upload",
            "通过页面上传成功 {}",
            r#"ResponseData { code: 0, data: Some(Object {"aid": Number(987654321), "bvid": String("synthetic-archive")}) }"#
        );
    });
    let mut capture = DiagnosticCapture::new();
    capture.push(b"fatal: decoder failed\nAuthoriz");
    capture.push(b"ation: synthetic-secret\n");
    capture.push(&vec![b'x'; 100_000]);
    capture.push(b"\n");
    for _ in 0..1000 {
        capture.push("普通诊断\n".as_bytes());
    }
    let diagnostic = capture.finish(Some(1));
    assert_eq!(diagnostic.first_fatal(), Some("fatal: decoder failed"));
    assert!(diagnostic.tail().len() <= 8192);
    assert!(diagnostic.truncated());
    assert!(diagnostic.total_bytes() > 100_000);
    let mut sensitive_fatal = DiagnosticCapture::new();
    sensitive_fatal.push(b"fatal: token=synthetic-secret\n");
    assert_eq!(
        sensitive_fatal.finish(Some(1)).first_fatal(),
        Some("[REDACTED]")
    );
    assert!(
        !serde_json::to_string(&diagnostic)
            .unwrap()
            .contains("synthetic-secret")
    );
    runtime.shutdown(Duration::from_secs(2));
    let events = events.lock().unwrap();
    let payload = serde_json::to_string(events[0].data()).unwrap();
    assert!(!payload.contains("synthetic-secret"));
    assert!(!payload.contains("request"));
    assert!(events[0].data().fields.quality().truncated > 0);
    assert!(events[0].data().fields.quality().rejected > 0);
    assert_eq!(events[1].data().message, "[REDACTED]");
    assert_eq!(events[2].data().message, "[REDACTED]");
    assert!(
        !serde_json::to_string(events[2].data())
            .unwrap()
            .contains("987654321")
    );
    let f = Fields::new()
        .with("gap_ms", -1)
        .with("original_file", "/private/example/segment.flv")
        .with("task_id", "task-a");
    assert!(f.get("gap_ms").is_none());
    assert_eq!(f.get("original_file").unwrap(), "segment.flv");
    // An id the call site does not have is unknown, not a dropped field: nothing is stored and
    // the quality counters stay clean, so a standalone command is not reported as lossy.
    let unknown = Fields::new()
        .with("live_streamer_id", "")
        .with("task_id", "task-a")
        .with("segment_id", "seg with space");
    assert!(unknown.get("live_streamer_id").is_none());
    assert_eq!(unknown.get("task_id").unwrap(), "task-a");
    assert!(unknown.get("segment_id").is_none());
    assert_eq!(
        unknown.quality().rejected,
        1,
        "only the malformed id counts"
    );
}

#[test]
fn off_is_lazy_and_new_filter_is_independent_from_legacy_error_filter() {
    let (mut runtime, events) = memory();
    let emitter = runtime.emitter();
    emitter.set_enabled(false);
    assert!(!emitter.emit_with(Level::Info, || panic!("off must not construct")));
    let old = Arc::new(Mutex::new(Vec::new()));
    #[derive(Clone)]
    struct Output(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let old_writer = old.clone();
    let old_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(move || Output(old_writer.clone()))
        .with_filter(tracing_subscriber::filter::filter_fn(|m| {
            legacy_output(m) && *m.level() <= tracing::Level::ERROR
        }));
    let subscriber = tracing_subscriber::registry()
        .with(old_layer)
        .with(CaptureLayer::new(emitter.clone()).filtered());
    tracing::subscriber::with_default(subscriber, || {
        emitter.set_enabled(true);
        tracing::info!(target:"biliup::event", event_name="system.started", "原生INFO");
        tracing::error!(target:"biliup::event", event_name="system.stopped", "原生ERROR");
        tracing::error!("旧ERROR");
        tracing::info!(target:"sqlx::query", "内部SQL不能递归");
    });
    runtime.shutdown(Duration::from_secs(2));
    assert_eq!(events.lock().unwrap().len(), 3);
    let text = String::from_utf8(old.lock().unwrap().clone()).unwrap();
    assert!(text.contains("旧ERROR"));
    assert!(!text.contains("原生"));
}

#[test]
fn queue_reservation_byte_limit_and_bounded_shutdown() {
    struct Blocked(std::sync::mpsc::Receiver<()>);
    impl Consumer for Blocked {
        fn write(&mut self, _: &[Event]) -> Result<Commit, StorageError> {
            let _ = self.0.recv();
            Ok(Commit::default())
        }
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let mut rx = Some(rx);
    let mut runtime = Runtime::start(
        "synthetic",
        "test",
        Options {
            enabled: true,
            queue_count: 8,
            queue_bytes: 64 * 1024,
            batch_count: 1,
            ..Options::default()
        },
        move || Ok(Blocked(rx.take().unwrap())),
    )
    .unwrap();
    let emitter = runtime.emitter();
    emitter.emit_with(Level::Info, || Draft::new("system.started", "开始"));
    for _ in 0..100 {
        if emitter.health().in_flight == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    for _ in 0..20 {
        emitter.emit_with(Level::Info, || Draft::new("system.started", "填充"));
    }
    assert!(emitter.emit_with(Level::Error, || Draft::new("system.stopped", "重要")));
    let health = emitter.health();
    assert!(health.dropped[2] > 0);
    assert!(health.queue_depth <= 8);
    assert!(health.queue_bytes <= 64 * 1024);
    assert!(health.peak_queue_bytes <= 64 * 1024);
    let at = std::time::Instant::now();
    let health = runtime.shutdown(Duration::from_millis(30));
    assert!(at.elapsed() < Duration::from_millis(200));
    assert!(health.shutdown_timed_out);
    assert_eq!(health.queue_depth, 0);
    tx.send(()).unwrap();
    runtime.shutdown(Duration::from_secs(1));
}

#[test]
fn failed_consumer_counts_loss_and_recovers_without_recursion() {
    struct Flaky(Arc<std::sync::atomic::AtomicBool>);
    impl Consumer for Flaky {
        fn write(&mut self, _: &[Event]) -> Result<Commit, StorageError> {
            if self.0.load(std::sync::atomic::Ordering::SeqCst) {
                Err(StorageError::new("synthetic_failure"))
            } else {
                Ok(Commit { high_water: 1 })
            }
        }
    }
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let f = fail.clone();
    let mut runtime = Runtime::start(
        "synthetic",
        "test",
        Options {
            enabled: true,
            ..Options::default()
        },
        move || Ok(Flaky(f.clone())),
    )
    .unwrap();
    let emitter = runtime.emitter();
    emitter.emit_with(Level::Warn, || Draft::new("system.started", "失败"));
    for _ in 0..100 {
        if emitter.health().dropped[3] > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(emitter.health().storage_failures, 3);
    assert_eq!(emitter.health().dropped[3], 1);
    fail.store(false, std::sync::atomic::Ordering::SeqCst);
    emitter.emit_with(Level::Info, || Draft::new("system.started", "恢复"));
    let h = runtime.shutdown(Duration::from_secs(2));
    assert_eq!(h.recoveries, 1);
    assert_eq!(h.committed_id, 1);
}

#[test]
fn concurrent_sequence_and_full_synthetic_chain_preserve_semantics() {
    let (mut runtime, events) = memory();
    let emitter = runtime.emitter();
    let mut workers = Vec::new();
    for task in ["alpha", "beta"] {
        let emitter = emitter.clone();
        workers.push(std::thread::spawn(move || {
            let context = Context(
                Fields::new()
                    .with("task_id", task)
                    .with("streamer_info_id", task)
                    .with("upload_session_id", format!("upload-{task}"))
                    .with("segment_id", format!("segment-{task}")),
            );
            for (name, outcome, reason, attempt) in [
                ("recording.started", "executed", "live_detected", "one"),
                ("recording.segment_closed", "executed", "split_limit", "one"),
                ("recording.disconnected", "failed", "transport_error", "one"),
                (
                    "recording.reconnected",
                    "recovered",
                    "transport_error",
                    "two",
                ),
                ("processing.decided", "skipped", "no_audio", "one"),
                ("processing.completed", "fallback", "invalid_output", "one"),
                ("upload.failed", "failed", "network_error", "one"),
                ("upload.completed", "succeeded", "eligible", "two"),
                ("submission.decided", "waiting", "pending_segments", "two"),
                (
                    "submission.completed",
                    "unknown",
                    "missing_remote_id",
                    "two",
                ),
            ] {
                assert!(emitter.emit_with(Level::Info, || {
                    let mut d = Draft::new(name, "合成决策");
                    d.context = context.clone();
                    d.fields = Fields::new()
                        .with("outcome", outcome)
                        .with("reason_code", reason)
                        .with("download_attempt_id", format!("download-{attempt}"))
                        .with("upload_attempt_id", format!("upload-{attempt}"));
                    d
                }));
                std::thread::yield_now();
            }
        }));
    }
    for worker in workers {
        worker.join().unwrap();
    }
    runtime.shutdown(Duration::from_secs(2));
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 20);
    let sequences: std::collections::HashSet<_> =
        events.iter().map(|e| e.data().sequence).collect();
    assert_eq!(sequences.len(), 20);
    for task in ["alpha", "beta"] {
        let chain: Vec<_> = events
            .iter()
            .filter(|e| e.data().fields.get("task_id").unwrap() == task)
            .collect();
        assert_eq!(chain.len(), 10);
        assert_eq!(
            chain.last().unwrap().data().fields.get("outcome").unwrap(),
            "unknown"
        );
        assert!(
            chain
                .iter()
                .all(|e| e.data().fields.get("segment_id").unwrap() == &format!("segment-{task}"))
        );
    }
}

#[test]
fn per_layer_off_does_not_format_and_invalid_native_is_counted() {
    struct Forbidden;
    impl std::fmt::Debug for Forbidden {
        fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            panic!("off formatter");
        }
    }
    let (mut runtime, _) = memory();
    let emitter = runtime.emitter();
    emitter.set_enabled(false);
    let subscriber =
        tracing_subscriber::registry().with(CaptureLayer::new(emitter.clone()).filtered());
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target:"biliup::event",event_name="system.started",error=?Forbidden,"关闭");
        emitter.set_enabled(true);
        tracing::info!(target:"biliup::event",event_name="invalid","不符合契约");
    });
    assert_eq!(emitter.health().dropped[2], 1);
    runtime.shutdown(Duration::from_secs(2));
}
