//! Task 12: the recording file layer assigns a stable identity and reports it natively.
//!
//! These tests assert on the `biliup::event` target only. The old lines keep their own target,
//! so a change here can never be mistaken for a change in the old output.

use biliup::downloader::util::{LifecycleFile, SegmentCloseReason, close_reason_code};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};

/// One captured event: the target it was routed to, and its fields as text.
type Record = (String, BTreeMap<String, String>);

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<Record>>>);

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
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        let mut fields = BTreeMap::new();
        event.record(&mut Collector(&mut fields));
        self.0
            .lock()
            .unwrap()
            .push((event.metadata().target().to_string(), fields));
    }
}

impl Captured {
    /// Native events only: anything on another target belongs to the old sinks.
    fn native(&self, name: &str) -> Vec<BTreeMap<String, String>> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(target, fields)| {
                target == "biliup::event"
                    && fields.get("event_name").map(String::as_str) == Some(name)
            })
            .map(|(_, fields)| fields.clone())
            .collect()
    }

    fn targets(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|(target, _)| target.clone())
            .collect()
    }
}

#[test]
fn each_created_file_gets_its_own_identity_and_reports_it_on_close() {
    let directory = tempfile::tempdir().unwrap();
    let template = directory
        .path()
        .join("segment-%Y%m%d")
        .display()
        .to_string();
    let captured = Captured::default();
    let closed: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let hook_closed = closed.clone();

    let subscriber = tracing_subscriber::registry().with(captured.clone());
    tracing::subscriber::with_default(subscriber, || {
        let mut file =
            LifecycleFile::with_hook(&template, "flv", move |name, _reason, identity| {
                hook_closed
                    .lock()
                    .unwrap()
                    .push((identity.segment_id.clone(), name.to_string()));
            });

        let path = file.create().unwrap().to_path_buf();
        let first = file.identity().unwrap().clone();
        std::fs::write(&path, b"first-segment").unwrap();
        file.finalize(SegmentCloseReason::TimedSplit).unwrap();

        let path = file.create().unwrap().to_path_buf();
        let second = file.identity().unwrap().clone();
        std::fs::write(&path, b"second").unwrap();
        file.finalize(SegmentCloseReason::StreamEnded).unwrap();

        assert_ne!(first.segment_id, second.segment_id);
        assert_eq!(first.original_file, second.original_file, "same template");
    });

    let created = captured.native("recording.segment_created");
    assert_eq!(created.len(), 2);
    assert_ne!(created[0]["segment_id"], created[1]["segment_id"]);
    assert!(created[0]["segment_id"].starts_with("seg-"));

    let closed_events = captured.native("recording.segment_closed");
    assert_eq!(closed_events.len(), 2);
    assert_eq!(closed_events[0]["reason_code"], "split_limit");
    assert_eq!(closed_events[1]["reason_code"], "stream_end");
    assert_eq!(closed_events[0]["size_bytes"], "13");
    // The identity reported at close is the one assigned at creation, not a fresh one.
    assert_eq!(closed_events[0]["segment_id"], created[0]["segment_id"]);

    let hook_calls = closed.lock().unwrap().clone();
    assert_eq!(hook_calls.len(), 2);
    assert_eq!(hook_calls[0].0, created[0]["segment_id"]);
    assert!(hook_calls[0].1.ends_with(".flv"));

    // The old "Save to ..." line still goes to its own target and is untouched.
    assert!(
        captured
            .targets()
            .iter()
            .any(|target| target.starts_with("biliup::downloader")),
        "old output must keep its own target: {:?}",
        captured.targets()
    );
}

#[test]
fn close_reasons_map_onto_the_frozen_v1_vocabulary() {
    assert_eq!(
        close_reason_code(SegmentCloseReason::TimedSplit),
        "split_limit"
    );
    assert_eq!(
        close_reason_code(SegmentCloseReason::SizeSplit),
        "split_limit"
    );
    assert_eq!(
        close_reason_code(SegmentCloseReason::StreamEnded),
        "stream_end"
    );
    assert_eq!(
        close_reason_code(SegmentCloseReason::TransportError),
        "transport_error"
    );
    assert_eq!(
        close_reason_code(SegmentCloseReason::Cancelled),
        "user_cancel"
    );
    assert_eq!(close_reason_code(SegmentCloseReason::Unknown), "unknown");
}
