//! Frozen P0 synthetic budget, no application entry points or business database.
use biliup_observability::{sqlite::*, *};
use std::{
    path::Path,
    time::{Duration, Instant},
};

fn workload(emitter: Option<&Emitter>) -> (Vec<u128>, Vec<u128>) {
    let start = Instant::now();
    let mut lateness = Vec::with_capacity(10_000);
    let mut emit = Vec::with_capacity(20_000);
    for tick in 0..10_000 {
        let deadline = start + Duration::from_millis(tick);
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
        lateness.push(
            Instant::now()
                .saturating_duration_since(deadline)
                .as_nanos(),
        );
        for task in ["alpha", "beta"] {
            let at = Instant::now();
            if let Some(emitter) = emitter {
                assert!(emitter.emit_with(Level::Info, || {
                    let mut d = Draft::new("recording.segment_closed", "合成分段关闭");
                    d.context = Context(
                        Fields::new()
                            .with("task_id", task)
                            .with("streamer_info_id", task)
                            .with("upload_session_id", format!("upload-{task}"))
                            .with("segment_id", format!("{task}-{}", tick / 100)),
                    );
                    d.fields = Fields::new()
                        .with("size_bytes", 1048576)
                        .with("duration_ms", 1000)
                        .with("outcome", "executed")
                        .with("reason_code", "split_limit");
                    d
                }));
            }
            emit.push(at.elapsed().as_nanos());
        }
    }
    lateness.sort_unstable();
    emit.sort_unstable();
    (lateness, emit)
}
fn main() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.sqlite");
    let options = StoreOptions::new(&path);
    let mut runtime = Runtime::start_with_identity(
        "synthetic",
        "acceptance-v1",
        Options {
            enabled: true,
            ..Options::default()
        },
        move |instance_id, process_run_id| {
            SqliteStore::open(options.clone(), instance_id, process_run_id)
        },
    )
    .unwrap();
    let emitter = runtime.emitter();
    let (baseline, _) = workload(None);
    let (loaded, latencies) = workload(Some(&emitter));
    let flush = Instant::now();
    while emitter.health().delivered < 20_000 && flush.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(5));
    }
    let flush_ms = flush.elapsed().as_millis();
    let db_bytes = std::fs::metadata(&path).unwrap().len();
    let wal_bytes = std::fs::metadata(format!("{}-wal", path.display()))
        .map(|m| m.len())
        .unwrap_or(0);
    let health = runtime.shutdown(Duration::from_secs(2));
    assert!(health.closed);
    assert_eq!(health.dropped, [0; 5]);
    assert_eq!(health.delivered, 20_000);
    assert_eq!(health.committed_id, 20_000);
    assert!(health.peak_queue_bytes <= 16 * 1024 * 1024);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (query_us, count) = rt.block_on(async {
        let repo = Repository::open(&path).await.unwrap();
        let mut cursor = 0;
        let mut count = 0;
        let mut elapsed = Vec::new();
        loop {
            let at = Instant::now();
            let page = repo
                .query(&Query {
                    after_id: cursor,
                    limit: 200,
                    ..Query::default()
                })
                .await
                .unwrap();
            elapsed.push(at.elapsed().as_nanos());
            if page.events.is_empty() {
                break;
            }
            for e in page.events {
                assert!(e.id > cursor);
                cursor = e.id;
                count += 1;
            }
        }
        repo.close().await;
        elapsed.sort_unstable();
        (elapsed[elapsed.len() * 99 / 100] as f64 / 1000.0, count)
    });
    let p99_us = latencies[19800] as f64 / 1000.0;
    let baseline_us = baseline[9900] as f64 / 1000.0;
    let loaded_us = loaded[9900] as f64 / 1000.0;
    assert_eq!(count, 20_000);
    assert!(p99_us <= 500.0);
    assert!(loaded_us - baseline_us <= 10_000.0);
    assert!(query_us <= 250_000.0);
    assert!(db_bytes + wal_bytes <= 208 * 1024 * 1024);
    assert!(wal_bytes <= 16 * 1024 * 1024);
    let report = serde_json::json!({"contract":"v1","events":count,"duration_secs":10,"p99_emit_us":p99_us,
        "baseline_p99_lateness_us":baseline_us,"loaded_p99_lateness_us":loaded_us,"query_p99_us":query_us,
        "flush_ms":flush_ms,"db_bytes":db_bytes,"wal_bytes":wal_bytes,"health":health});
    println!("{report}");
    if let Some(path) = std::env::args().nth(1) {
        let path = Path::new(&path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
}
