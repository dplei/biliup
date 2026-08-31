//! Controlled legacy formatter workload; never reads credentials or starts a server.
use std::{io, time::Instant};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt};

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "wheel".into());
    let dir = tempfile::tempdir().unwrap();
    let timer = tracing_subscriber::fmt::time::LocalTime::new(time::macros::format_description!(
        "[year]-[month]-[day] [hour]:[minute]:[second]"
    ));
    let (file, guard) = tracing_appender::non_blocking(tracing_appender::rolling::never(
        dir.path(),
        "synthetic.log",
    ));
    let dropped = file.error_counter();
    let subscriber: Box<dyn tracing::Subscriber + Send + Sync> = match mode.as_str() {
        "rust" => Box::new(
            tracing_subscriber::registry()
                .with(EnvFilter::new("tower_http=debug,info"))
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_timer(timer)
                        .with_writer(io::sink),
                ),
        ),
        "python" => Box::new(
            tracing_subscriber::FmtSubscriber::builder()
                .with_timer(timer.clone())
                .with_writer(io::sink)
                .finish()
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_timer(timer)
                        .with_ansi(false)
                        .with_writer(file),
                ),
        ),
        "wheel" => Box::new(
            tracing_subscriber::registry()
                .with(EnvFilter::new("info"))
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_timer(timer.clone())
                        .with_target(false)
                        .with_file(true)
                        .with_line_number(true)
                        .with_thread_ids(true)
                        .with_writer(io::sink),
                )
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_timer(timer)
                        .with_ansi(false)
                        .with_file(true)
                        .with_line_number(true)
                        .with_thread_ids(true)
                        .with_writer(file),
                ),
        ),
        _ => panic!("expected rust, wheel or python"),
    };
    let mut latencies = Vec::with_capacity(20_000);
    let started = Instant::now();
    tracing::subscriber::with_default(subscriber, || {
        for n in 0..20_000 {
            let at = Instant::now();
            tracing::info!(
                task_id = n % 2,
                segment_order = n / 100,
                confirmed_bytes = n * 1024,
                "合成分段上传进度"
            );
            latencies.push(at.elapsed().as_nanos());
        }
    });
    let emit_ms = started.elapsed().as_millis();
    drop(guard);
    latencies.sort_unstable();
    let bytes = std::fs::metadata(dir.path().join("synthetic.log"))
        .unwrap()
        .len();
    println!(
        "{}",
        serde_json::json!({"mode": mode, "events": 20000,
        "emit_ms": emit_ms, "flush_total_ms": started.elapsed().as_millis(),
        "p95_us": latencies[19000] as f64 / 1000.0, "p99_us": latencies[19800] as f64 / 1000.0,
        "legacy_file_bytes": bytes, "dropped": dropped.dropped_lines()})
    );
}
