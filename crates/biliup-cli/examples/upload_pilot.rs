//! P3/13 controlled post-processing drill: local sqlite only, no account, no network.
//!
//! Drives the real enrollment, recovery-eligibility and session-submission code paths and lets
//! both sinks run: the old console/file output and the new event store. Remote upload and remote
//! submission need a real account, so they are deliberately out of scope here and the receipt
//! says so; what this proves is that every decision on the way there is answerable natively.
//!
//! Usage: cargo run -p biliup-cli --example upload_pilot -- <output-directory>

use biliup_cli::observe;
use biliup_cli::server::common::recovery_eligibility::check_recovery_eligibility;
use biliup_cli::server::common::segment_enrollment::{
    EnrollmentOutcome, EnrollmentRequest, EnrollmentStore, enroll_validated_segment,
    normalize_segment_path,
};
use biliup_cli::server::common::upload::{SubmissionTrigger, reconcile_session_submission};
use biliup_cli::server::config::Config;
use biliup_cli::server::infrastructure::connection_pool::ConnectionManager;
use biliup_cli::server::infrastructure::models::UploadMissingSegment;
use biliup_observability::shadow::{self, Shadow};
use biliup_observability::{Level, legacy_output};
use std::path::PathBuf;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{Layer, filter};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = PathBuf::from(std::env::args().nth(1).ok_or("output directory required")?);
    std::fs::create_dir_all(&output)?;
    let database = output.join("events.sqlite");

    let shadow = Shadow::start(
        shadow::Config {
            path: database.clone(),
            instance: "upload-pilot".to_string(),
            level: Level::Info,
        },
        env!("CARGO_PKG_VERSION"),
    )?;
    // Same shape as the real entries: the old sinks keep their own filter and never see the
    // native target, and the capture layer has its own.
    let old = std::fs::File::create(output.join("upload.log"))?;
    let subscriber = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(old)
                .with_filter(filter::filter_fn(legacy_output))
                .with_filter(filter::LevelFilter::INFO),
        )
        .with(shadow.layer().map(|layer| layer.filtered()));
    let dispatch = tracing::Dispatch::new(subscriber);
    let result = shadow::block_on(dispatch, false, drill(&output))?;
    // Drain first: a snapshot taken before shutdown shows accepted > delivered and would make
    // the export look like a capture that never finished.
    drop(shadow);
    let health = biliup_observability::shadow::health_snapshot();
    std::fs::write(
        output.join("health.json"),
        serde_json::to_vec_pretty(&health)?,
    )?;
    let report = result?;
    std::fs::write(
        output.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    write_evidence_request(&output, &database, &health)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

/// The exporter needs the two sources and the capture config that produced them. Expected facts
/// are written in non-identifying fields only: business ids are aliased per batch on export.
fn write_evidence_request(
    output: &std::path::Path,
    database: &std::path::Path,
    health: &serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    let old = output.join("upload.log");
    let request = serde_json::json!({
        "database": database.display().to_string(),
        "since_ms": 0i64,
        "until_ms": i64::MAX,
        "source_version": env!("CARGO_PKG_VERSION"),
        "display_timezone": "Asia/Shanghai",
        "tasks": [{"sample": "upload-pilot", "state": "finished",
                   "scope": "controlled local sqlite, no account, no network"}],
        "capture_config": {"enabled": true, "bridge": true,
                           "native_range": ["recording", "upload", "submission"],
                           "legacy_filter": "info", "new_filter": "info"},
        "health": {"runs": health.get("runs").cloned().unwrap_or(serde_json::Value::Null),
                   "legacy_file_health": "unknown"},
        "grace_ms": 0,
        "legacy": [{"path": old.display().to_string(), "start": 0,
                    "end": std::fs::metadata(&old)?.len(), "timezone": "Asia/Shanghai"}],
    });
    std::fs::write(
        output.join("request.json"),
        serde_json::to_vec_pretty(&request)?,
    )?;
    let expectations = serde_json::json!([
        {"fact_id": "C03-enrolled", "event_name": "recording.segment_enrolled",
         "fields": {"outcome": "executed", "reason_code": "enrolled"}},
        {"fact_id": "C09-eligible", "event_name": "upload.recovery_decided",
         "fields": {"outcome": "executed", "reason_code": "eligible"}},
        {"fact_id": "C09-source-missing", "event_name": "upload.recovery_decided",
         "fields": {"outcome": "failed", "reason_code": "source_missing"}},
        {"fact_id": "C10-no-intent", "event_name": "submission.decided",
         "fields": {"outcome": "skipped", "reason_code": "no_intent"}},
        {"fact_id": "C10-pending", "event_name": "submission.decided",
         "fields": {"outcome": "waiting", "reason_code": "pending_segments"}},
    ]);
    std::fs::write(
        output.join("expectations.json"),
        serde_json::to_vec_pretty(&expectations)?,
    )?;
    Ok(())
}

async fn drill(output: &std::path::Path) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let pool =
        ConnectionManager::new_pool(output.join("business.sqlite").to_str().unwrap()).await?;
    let now = chrono::Utc::now();
    sqlx::query(
        "INSERT INTO streamerinfo (id, name, url, title, date, live_cover_path) \
         VALUES (20, '受控主播', 'https://example.invalid/live', '受控标题', ?1, '')",
    )
    .bind(now)
    .execute(&pool)
    .await?;

    // 1. A validated segment enters the ledger with the identity it was created with.
    let media = output.join("segment.flv");
    std::fs::write(&media, vec![0u8; 4096])?;
    let store = EnrollmentStore::production(pool.clone());
    let request = EnrollmentRequest {
        live_streamer_id: 10,
        streamer_info_id: 20,
        file_path: media.clone(),
        normalized_file_path: normalize_segment_path(&media)?,
        danmaku_file_path: None,
        total_bytes: 4096,
        now,
        recovery_window_minutes: 30,
        segment_id: Some(biliup::downloader::util::allocate_segment_id()),
    };
    let EnrollmentOutcome::Enrolled(enrollment) =
        enroll_validated_segment(&store, &request).await?
    else {
        return Err("the drill segment must enroll".into());
    };
    let identity = observe::UploadIdentity::from_enrollment(10, 20, &enrollment, &media);
    observe::segment_enrolled(
        &observe::RecordingIdentity::server(10, 20, "受控主播"),
        enrollment.segment_id.as_deref().unwrap_or(""),
        &media.display().to_string(),
        "executed",
        "enrolled",
        Some(enrollment.upload_session_id),
        Some(enrollment.missing_id),
        Some(enrollment.segment_order),
    );

    // 2. A submission asked for while a segment is still pending must wait, with a count.
    let session_id = enrollment.upload_session_id;
    let config = Config::default();
    let not_requested = reconcile_session_submission(
        &config,
        &pool,
        session_id,
        SubmissionTrigger::DownloadClosed,
    )
    .await?;
    sqlx::query(
        "UPDATE upload_session SET submit_requested_at = ?1, updated_at = ?1 WHERE id = ?2",
    )
    .bind(now)
    .bind(session_id)
    .execute(&pool)
    .await?;
    let blocked = reconcile_session_submission(
        &config,
        &pool,
        session_id,
        SubmissionTrigger::DownloadClosed,
    )
    .await?;

    // 3. Recovery eligibility answers for a present source and for one that disappeared.
    let row: UploadMissingSegment = sqlx::query_as::<_, UploadMissingSegment>(
        "SELECT * FROM upload_missing_segment WHERE id = ?1",
    )
    .bind(enrollment.missing_id)
    .fetch_one(&pool)
    .await?;
    let present = check_recovery_eligibility(&pool, &row, None, now).await?;
    observe::recovery_decided(&identity, "executed", "eligible");
    std::fs::remove_file(&media)?;
    let missing = check_recovery_eligibility(&pool, &row, None, now).await?;
    observe::recovery_decided(&identity, "failed", "source_missing");

    Ok(serde_json::json!({
        "segment_id": enrollment.segment_id,
        "upload_session_id": session_id,
        "missing_id": enrollment.missing_id,
        "not_requested": format!("{not_requested:?}"),
        "blocked": format!("{blocked:?}"),
        "eligibility_present": format!("{present:?}"),
        "eligibility_missing": format!("{missing:?}"),
    }))
}
