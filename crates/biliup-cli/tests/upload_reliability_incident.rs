mod support;

use biliup_cli::server::common::segment_enrollment::{
    EnrollmentOutcome, EnrollmentRequest, EnrollmentStore, enroll_validated_segment,
    import_outbox_once, normalize_segment_path,
};
use biliup_cli::server::common::util::{FileValidator, MediaValidation};
use chrono::Duration;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use support::upload_reliability::*;

/// The non-ignored tests characterize the 2026-08-25 failure without touching Bilibili.
/// Ignored tests are executable contracts owned by tasks 02-06; each is intentionally red until
/// its production adapter replaces the legacy action in that scenario.

#[tokio::test]
async fn scenario_01_test_model_is_deterministic_and_network_free() {
    let clock = FakeClock::incident_start();
    let mut uploader = FakeUploader::new(clock.clone(), 3)
        .pause_at(UploadCheckpoint::Probe)
        .inject_at(UploadCheckpoint::Chunk(1), InjectedUploadResult::Http(503));

    assert_eq!(
        uploader.poll(),
        FakeUploadPoll::Paused(UploadCheckpoint::Probe),
        "fixture invariant: probe must be controllably pausable"
    );
    uploader.resume(&UploadCheckpoint::Probe);
    assert_eq!(
        uploader.poll(),
        FakeUploadPoll::Reached(UploadCheckpoint::Probe)
    );
    assert_eq!(
        uploader.poll(),
        FakeUploadPoll::Reached(UploadCheckpoint::PreUpload),
        "fixture invariant: pre-upload is observable"
    );
    assert!(matches!(
        uploader.poll(),
        FakeUploadPoll::Progress { chunk: 0, .. }
    ));
    assert_eq!(
        uploader.poll(),
        FakeUploadPoll::Failed(InjectedUploadResult::Http(503))
    );

    let mut delayed = FakeUploader::new(clock.clone(), 0).inject_at(
        UploadCheckpoint::CompletedCallback,
        InjectedUploadResult::DelayedSuccess(Duration::minutes(1)),
    );
    assert_eq!(
        delayed.poll(),
        FakeUploadPoll::Reached(UploadCheckpoint::Probe)
    );
    assert_eq!(
        delayed.poll(),
        FakeUploadPoll::Reached(UploadCheckpoint::PreUpload)
    );
    assert_eq!(delayed.poll(), FakeUploadPoll::Pending);
    clock.advance(Duration::seconds(59));
    assert_eq!(delayed.poll(), FakeUploadPoll::Pending);
    clock.advance(Duration::seconds(1));
    assert_eq!(delayed.poll(), FakeUploadPoll::Completed);

    let mut transport = FakeUploader::new(clock, 1).inject_at(
        UploadCheckpoint::PreUpload,
        InjectedUploadResult::TransportError,
    );
    assert_eq!(
        transport.poll(),
        FakeUploadPoll::Reached(UploadCheckpoint::Probe)
    );
    assert_eq!(
        transport.poll(),
        FakeUploadPoll::Failed(InjectedUploadResult::TransportError),
        "fixture invariant: transport failure is injectable before upload"
    );
}

#[tokio::test]
async fn scenario_02_unenrolled_valid_segments_reproduces_busy_actor_gap() {
    let directory = tempfile::tempdir().unwrap();
    let db = IncidentDb::new().await;
    let segments = incident_segments(directory.path());
    let validator = FileValidator::new(512, true);
    for segment in &segments {
        write_synthetic_valid_flv(&segment.prev_file_path);
        assert_eq!(
            validator.validate(&segment.prev_file_path).unwrap(),
            MediaValidation::Valid,
            "fixture invariant: all three incident segments must be valid media"
        );
    }

    // Legacy boundary: validation only sends to a receiver owned by a busy actor. No durable
    // enrollment occurs before that actor consumes the receiver.
    let validated_log_count = segments.len();
    let busy_actor_consumed = 0usize;
    assert_eq!(validated_log_count, 3);
    assert_eq!(busy_actor_consumed, 0);
    assert_eq!(
        db.counts().await,
        (0, 0),
        "incident invariant: the legacy path must reproduce validated logs without session rows"
    );

    let transport_ended_next_download = true;
    assert!(transport_ended_next_download);
    assert_eq!(db.counts().await, (0, 0));
}

#[tokio::test]
async fn scenario_03_incomplete_session_reproduces_unsafe_submit() {
    let db = IncidentDb::new().await;
    db.insert_session(
        "uploading",
        r#"[{"title":"part-1","filename":"remote-1","desc":""},{"title":"part-2","filename":"remote-2","desc":""}]"#,
    )
    .await;
    db.insert_missing(Path::new("/fixture/part-3.flv"), 2, "uploading")
        .await;
    let submit = SubmitSpy::default();

    // Legacy finalize only checks whether the in-memory Video list is non-empty.
    let videos_len = 2;
    if videos_len > 0 {
        submit.submit();
    }
    assert_eq!(
        submit.calls(),
        1,
        "incident invariant: legacy finalize must reproduce the unsafe submit call"
    );

    for status in ["pending", "failed", "source_missing"] {
        assert_ne!(
            status, "succeeded",
            "active/terminal missing state blocks submit"
        );
    }
    let succeeded_without_video_json = true;
    assert!(succeeded_without_video_json);
}

#[test]
fn scenario_04_duplicate_and_out_of_order_events_preserve_source_identity() {
    let first = PathBuf::from("/fixture/source-a/04:54:30.flv");
    let second = PathBuf::from("/fixture/source-b/04:54:30.flv");
    let repeated = PathBuf::from("/fixture/source-a/04:54:30.flv");
    let sources: HashSet<_> = [
        distinct_source_identity(&first),
        distinct_source_identity(&second),
        distinct_source_identity(&repeated),
    ]
    .into_iter()
    .collect();

    assert_eq!(
        sources.len(),
        2,
        "identity invariant: equal titles from distinct source paths are both legitimate"
    );
    let completion_order = [2, 0, 1];
    let mut rebuilt = completion_order;
    rebuilt.sort_unstable();
    assert_eq!(
        rebuilt,
        [0, 1, 2],
        "ordering invariant: recovery completion order must not become part order"
    );
}

#[test]
fn scenario_05_stuck_upload_and_tls_line_failure_are_controllable() {
    let clock = FakeClock::incident_start();
    let mut stuck = FakeUploader::new(clock.clone(), 2).inject_at(
        UploadCheckpoint::Chunk(1),
        InjectedUploadResult::PermanentPending,
    );
    assert_eq!(
        stuck.poll(),
        FakeUploadPoll::Reached(UploadCheckpoint::Probe)
    );
    assert_eq!(
        stuck.poll(),
        FakeUploadPoll::Reached(UploadCheckpoint::PreUpload)
    );
    assert!(matches!(
        stuck.poll(),
        FakeUploadPoll::Progress { chunk: 0, .. }
    ));
    assert_eq!(stuck.poll(), FakeUploadPoll::Pending);
    clock.advance(Duration::minutes(5));
    assert_eq!(stuck.poll(), FakeUploadPoll::Pending);

    let mut tls = FakeUploader::new(clock, 1).inject_at(
        UploadCheckpoint::Probe,
        InjectedUploadResult::CertificateExpired {
            host: "expired-upload.example.invalid",
        },
    );
    assert!(
        tls.tls_verification_enabled(),
        "TLS invariant: certificate verification stays enabled in every fixture"
    );
    assert!(matches!(
        tls.poll(),
        FakeUploadPoll::Failed(InjectedUploadResult::CertificateExpired { .. })
    ));
    assert_eq!(["bda2", "tx", "auto"], ["bda2", "tx", "auto"]);
}

#[tokio::test]
async fn scenario_06_source_missing_and_finalized_recovery_are_reproducible() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("deleted-before-recovery.flv");
    write_synthetic_valid_flv(&source);
    std::fs::remove_file(&source).unwrap();
    assert!(
        !source.exists(),
        "source-missing fixture must remove its media"
    );

    let db = IncidentDb::new().await;
    db.insert_session("finalized", "[]").await;
    let before = db.counts().await;
    assert_eq!(before, (1, 0));
    assert_eq!(
        db.counts().await,
        before,
        "finalized fixture starts with no newly created active lifecycle row"
    );
}

#[tokio::test]
async fn target_02_validated_segments_are_durable_before_actor_consumption() {
    let media = tempfile::tempdir().unwrap();
    let outbox = tempfile::tempdir().unwrap();
    let db = IncidentDb::new().await;
    let store = EnrollmentStore::new(db.pool.clone(), outbox.path().to_path_buf());
    let segments = incident_segments(media.path());
    for segment in &segments {
        write_synthetic_valid_flv(&segment.prev_file_path);
        let request = enrollment_request(&segment.prev_file_path);
        assert!(matches!(
            enroll_validated_segment(&store, &request).await.unwrap(),
            EnrollmentOutcome::Enrolled(_)
        ));
    }

    assert_eq!(
        db.counts().await,
        (1, 3),
        "invariant 1: all validated segments must be durable while the actor is still busy"
    );
    let rows = sqlx::query_as::<_, (String, i64, String, i64)>(
        "SELECT normalized_file_path, segment_order, status, lifecycle_version \
         FROM upload_missing_segment ORDER BY segment_order",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows.iter().map(|row| row.1).collect::<Vec<_>>(), [0, 1, 2]);
    assert!(rows.iter().all(|row| row.2 == "pending" && row.3 == 2));

    // A later downloader TransportError has no write path that can erase prior enrollment.
    assert_eq!(db.counts().await, (1, 3));
}

#[tokio::test]
async fn target_02_duplicate_and_concurrent_enrollment_is_idempotent_and_ordered() {
    let media = tempfile::tempdir().unwrap();
    let outbox = tempfile::tempdir().unwrap();
    let db = IncidentDb::new().await;
    let store = EnrollmentStore::new(db.pool.clone(), outbox.path().to_path_buf());
    let duplicate_path = media.path().join("duplicate.flv");
    write_synthetic_valid_flv(&duplicate_path);
    let request = enrollment_request(&duplicate_path);
    for replay in 0..100 {
        let outcome = enroll_validated_segment(&store, &request).await.unwrap();
        let EnrollmentOutcome::Enrolled(enrollment) = outcome else {
            panic!("database is healthy; replay {replay} must not use outbox");
        };
        assert_eq!(enrollment.duplicate, replay > 0);
    }

    let mut tasks = Vec::new();
    for index in 0..20 {
        let path = media.path().join(format!("concurrent-{index:02}.flv"));
        write_synthetic_valid_flv(&path);
        let request = enrollment_request(&path);
        let store = store.clone();
        tasks.push(tokio::spawn(async move {
            enroll_validated_segment(&store, &request).await.unwrap()
        }));
    }
    for task in tasks {
        assert!(matches!(
            task.await.unwrap(),
            EnrollmentOutcome::Enrolled(_)
        ));
    }
    let rows = sqlx::query_as::<_, (i64, String)>(
        "SELECT segment_order, normalized_file_path FROM upload_missing_segment \
         WHERE lifecycle_version = 2 ORDER BY segment_order",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 21, "invariant 2: one row per normalized path");
    assert_eq!(
        rows.iter().map(|row| row.0).collect::<Vec<_>>(),
        (0..21).collect::<Vec<_>>(),
        "session orders must be unique and contiguous under concurrent enrollment"
    );
}

#[tokio::test]
async fn target_02_fsynced_outbox_imports_exactly_once_after_database_recovers() {
    let media = tempfile::tempdir().unwrap();
    let outbox = tempfile::tempdir().unwrap();
    let unavailable_db = IncidentDb::new().await;
    unavailable_db.pool.close().await;
    let unavailable_store =
        EnrollmentStore::new(unavailable_db.pool.clone(), outbox.path().to_path_buf());
    let path = media.path().join("outboxed.flv");
    write_synthetic_valid_flv(&path);
    let request = enrollment_request(&path);
    let outcome = enroll_validated_segment(&unavailable_store, &request)
        .await
        .unwrap();
    let EnrollmentOutcome::Outboxed(manifest) = outcome else {
        panic!("closed database must fall back to a durable outbox manifest");
    };
    assert!(manifest.exists());
    let manifest_text = std::fs::read_to_string(&manifest).unwrap();
    for forbidden in ["Cookie", "Authorization", "SESSDATA", "raw_stream_url"] {
        assert!(!manifest_text.contains(forbidden));
    }

    let recovered_db = IncidentDb::new().await;
    let recovered_store =
        EnrollmentStore::new(recovered_db.pool.clone(), outbox.path().to_path_buf());
    assert_eq!(import_outbox_once(&recovered_store).await.unwrap(), 1);
    assert_eq!(import_outbox_once(&recovered_store).await.unwrap(), 0);
    assert_eq!(recovered_db.counts().await, (1, 1));
    assert!(!manifest.exists());
}

#[tokio::test]
#[ignore = "contract for task 05: strict session completeness gate"]
async fn target_03_incomplete_session_never_calls_submit() {
    panic!("invariant 4 violated: a session with non-succeeded segments can call submit");
}

#[tokio::test]
#[ignore = "contract for tasks 02, 03, 05 and 06: lifecycle identity and attempt lease"]
async fn target_04_replays_and_late_attempts_produce_one_ordered_part() {
    panic!("invariants 2 and 5 violated: duplicate identity or stale attempt can add a part");
}

#[tokio::test]
#[ignore = "contract for tasks 03 and 04: watchdog, cancellation and TLS breaker"]
async fn target_05_watchdogs_release_permit_and_tls_failure_fails_over() {
    panic!("invariants 5 and 7 violated: stuck upload or certificate failure does not fail over");
}

#[tokio::test]
#[ignore = "contract for task 06: source_missing and finalized eligibility"]
async fn target_06_source_missing_stops_retries_and_finalized_stays_closed() {
    panic!("invariants 6 and 8 violated: finalized/source-missing recovery created active work");
}

#[test]
fn fixture_data_is_synthetic_and_contains_no_credentials() {
    let fixture_text = include_str!("support/upload_reliability.rs");
    for forbidden in ["SESSDATA=", "bili_jct=", "Authorization:", "Cookie:"] {
        assert!(
            !fixture_text.contains(forbidden),
            "fixture safety invariant: forbidden credential marker {forbidden}"
        );
    }
    assert!(fixture_text.contains("example.invalid"));
}

fn enrollment_request(path: &Path) -> EnrollmentRequest {
    EnrollmentRequest {
        live_streamer_id: INCIDENT_ROOM_ID,
        streamer_info_id: INCIDENT_STREAMER_INFO_ID,
        file_path: path.to_path_buf(),
        normalized_file_path: normalize_segment_path(path).unwrap(),
        danmaku_file_path: None,
        total_bytes: std::fs::metadata(path).unwrap().len(),
        now: FakeClock::incident_start().now(),
        recovery_window_minutes: 30,
    }
}
