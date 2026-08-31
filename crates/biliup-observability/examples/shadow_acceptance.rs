//! P2 dual-sink workload; only synthetic data, caller chooses a private output directory.
use biliup_observability::{
    Level, legacy_output,
    shadow::{self, Config, Shadow},
};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use tracing_subscriber::{Layer, layer::SubscriberExt};
fn ticks(log: bool) -> (Vec<u128>, Vec<u128>) {
    let start = Instant::now();
    let mut wake = Vec::new();
    let mut emit = Vec::new();
    for tick in 0..10000 {
        let deadline = start + Duration::from_millis(tick);
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
        wake.push(
            Instant::now()
                .saturating_duration_since(deadline)
                .as_nanos(),
        );
        for task in ["alpha", "beta"] {
            let at = Instant::now();
            if log {
                tracing::info!(
                    task_id = task,
                    size_bytes = 1048576,
                    "synthetic tick {tick} task {task}"
                );
            }
            emit.push(at.elapsed().as_nanos());
        }
    }
    wake.sort_unstable();
    emit.sort_unstable();
    (wake, emit)
}
fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("private output directory"));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("events.sqlite");
    let shadow = Shadow::start(
        Config {
            path: path.clone(),
            instance: "synthetic".into(),
            level: Level::Info,
        },
        "shadow-v1",
    )
    .unwrap();
    let file = tracing_appender::rolling::never(&dir, "legacy.log");
    let (writer, guard) = tracing_appender::non_blocking(file);
    let errors = writer.error_counter();
    let timer = tracing_subscriber::fmt::time::LocalTime::new(time::macros::format_description!(
        "[year]-[month]-[day] [hour]:[minute]:[second]"
    ));
    let old_console = tracing_subscriber::fmt::layer()
        .with_writer(std::io::sink)
        .with_timer(timer.clone())
        .with_target(false)
        .with_file(true)
        .with_line_number(true)
        .with_thread_ids(true);
    let old_file = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_timer(timer)
        .with_ansi(false)
        .with_file(true)
        .with_line_number(true)
        .with_thread_ids(true);
    let subscriber = tracing_subscriber::registry()
        .with(
            old_console
                .and_then(old_file)
                .with_filter(tracing_subscriber::filter::filter_fn(legacy_output))
                .with_filter(tracing_subscriber::EnvFilter::new("info")),
        )
        .with(shadow.layer().map(|l| l.filtered()));
    let (baseline, _) = ticks(false);
    let (loaded, emit) = tracing::subscriber::with_default(subscriber, || ticks(true));
    let at = Instant::now();
    while shadow.health().unwrap().delivered < 20000 && at.elapsed() < Duration::from_secs(2) {
        assert!(
            std::fs::metadata(dir.join("legacy.log")).unwrap().len() <= 256 * 1024 * 1024,
            "old file budget: stop pilot"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let flush = at.elapsed().as_millis();
    let health = shadow.health().unwrap();
    let db = std::fs::metadata(&path).unwrap().len();
    let wal = std::fs::metadata(path.with_extension("sqlite-wal"))
        .map(|m| m.len())
        .unwrap_or(0);
    drop(guard);
    drop(shadow);
    let old = std::fs::metadata(dir.join("legacy.log")).unwrap().len();
    let emit_us = emit[19800] as f64 / 1000.;
    let baseline_us = baseline[9900] as f64 / 1000.;
    let loaded_us = loaded[9900] as f64 / 1000.;
    assert_eq!(health.delivered, 20000);
    assert_eq!(health.dropped, [0; 5]);
    assert_eq!(errors.dropped_lines(), 0);
    assert!(emit_us <= 500.);
    assert!(loaded_us - baseline_us <= 10000.);
    assert!(health.peak_queue_bytes <= 16 * 1024 * 1024);
    assert!(db + wal <= 208 * 1024 * 1024);
    assert!(wal <= 16 * 1024 * 1024);
    assert!(old <= 256 * 1024 * 1024);
    assert!(db + wal + old <= 464 * 1024 * 1024);
    let report = serde_json::json!({"p99_emit_us":emit_us,"baseline_p99_lateness_us":baseline_us,"loaded_p99_lateness_us":loaded_us,"flush_ms":flush,"db_bytes":db,"wal_bytes":wal,"legacy_bytes":old,"old_dropped":errors.dropped_lines(),"health":health});
    std::fs::write(
        dir.join("report.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    let request = serde_json::json!({"database":path,"since_ms":0,"until_ms":i64::MAX,"source_version":"synthetic-shadow-v1","tasks":[{"task_id":"alpha","state":"finished"},{"task_id":"beta","state":"finished"}],"capture_config":{"enabled":true,"legacy_filter":"info","new_filter":"info","bridge":true,"native_range":[],"legacy_dropped":errors.dropped_lines()},"health":shadow::health_snapshot(),"grace_ms":2000,"legacy":[{"path":dir.join("legacy.log"),"timezone":"Asia/Shanghai"}]});
    std::fs::write(
        dir.join("request.json"),
        serde_json::to_vec_pretty(&request).unwrap(),
    )
    .unwrap();
    println!("{report}");
}
