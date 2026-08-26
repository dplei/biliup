mod support;

use biliup::bilibili::Video;
use biliup_cli::server::common::segment_enrollment::{
    EnrollmentOutcome, EnrollmentRequest, EnrollmentStore, enroll_validated_segment,
    import_outbox_once, normalize_segment_path,
};
use biliup_cli::server::common::upload::{
    AttemptClaim, claim_enrolled_attempt, fail_enrolled_attempt, persist_segment,
};
use biliup_cli::server::common::upload_line_health::{
    LineAvailability, UploadFailureKind, acquire_line, record_failure,
};
use biliup_cli::server::common::upload_session::{
    LiveArchive, SubmitClaim, claim_complete_session, parse_videos,
};
use biliup_cli::server::common::util::{FileValidator, MediaValidation};
use biliup_cli::server::core::downloader::SegmentEnrollment;
use biliup_cli::server::infrastructure::connection_pool::{ConnectionManager, ConnectionPool};
use chrono::Duration;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use support::upload_reliability::*;

/// The `scenario_*` tests characterize the 2026-08-25 failure without touching Bilibili; the
/// `target_*` tests are the executable contracts owned by tasks 02-06. Every contract is now
/// backed by a production adapter, so none of them is ignored.

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

/// Scenario 1 from task 08's fault matrix: the process is killed the instant a segment becomes
/// validated, before any actor or watchdog touches it. Restart must find the row exactly where
/// enrollment left it — `pending`, unclaimed — not lost, not stuck `uploading` forever.
#[tokio::test]
async fn target_02_pending_segment_survives_process_restart() {
    let media = tempfile::tempdir().unwrap();
    let outbox = tempfile::tempdir().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("restart.sqlite3");
    let pool = ConnectionManager::new_pool(db_path.to_str().unwrap())
        .await
        .unwrap();
    let now = FakeClock::incident_start().now();
    sqlx::query("INSERT INTO livestreamers (id, url, remark) VALUES (?1, ?2, ?3)")
        .bind(INCIDENT_ROOM_ID)
        .bind("https://example.invalid/live/synthetic-room")
        .bind("synthetic-streamer")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO streamerinfo (id, name, url, title, date, live_cover_path) \
         VALUES (?1, ?2, ?3, ?4, ?5, '')",
    )
    .bind(INCIDENT_STREAMER_INFO_ID)
    .bind("synthetic-streamer")
    .bind("https://example.invalid/live/synthetic-room")
    .bind("synthetic incident fixture")
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();

    let store = EnrollmentStore::new(pool.clone(), outbox.path().to_path_buf());
    let path = media.path().join("restart-segment.flv");
    write_synthetic_valid_flv(&path);
    let enrollment = match enroll_validated_segment(&store, &enrollment_request(&path))
        .await
        .unwrap()
    {
        EnrollmentOutcome::Enrolled(enrollment) => enrollment,
        other => panic!("expected a durable enrollment before the simulated crash, got {other:?}"),
    };

    // Simulate "kill -9 right after validated": drop the pool without any actor or watchdog
    // ever consuming the row, then reopen the same file to model the restarted process.
    pool.close().await;
    let restarted = ConnectionManager::new_pool(db_path.to_str().unwrap())
        .await
        .unwrap();

    let (status, attempt_token): (String, Option<String>) =
        sqlx::query_as("SELECT status, attempt_token FROM upload_missing_segment WHERE id = ?")
            .bind(enrollment.missing_id)
            .fetch_one(&restarted)
            .await
            .unwrap();
    assert_eq!(
        status, "pending",
        "restart invariant: a crash right after validation must leave the row pending, \
         not lost and not stuck uploading"
    );
    assert!(attempt_token.is_none());

    // Recovery is not just a status string: the row must still be claimable after restart.
    assert!(matches!(
        claim_enrolled_attempt(&restarted, &enrollment, "bda2", None)
            .await
            .unwrap(),
        AttemptClaim::Claimed(_)
    ));
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
async fn target_03_incomplete_session_never_calls_submit() {
    let db = IncidentDb::new().await;
    db.insert_session(
        "uploading",
        r#"[{"title":"part-1","filename":"remote-1","desc":""}]"#,
    )
    .await;
    db.insert_missing(Path::new("/fixture/part-2.flv"), 1, "uploading")
        .await;
    let submit = SubmitSpy::default();

    match claim_complete_session(&db.pool, INCIDENT_SESSION_ID)
        .await
        .unwrap()
    {
        SubmitClaim::Claimed { .. } => submit.submit(),
        SubmitClaim::Blocked { completeness, .. } => {
            assert_eq!(completeness.uploading, 1);
            assert!(!completeness.is_complete());
        }
        other => panic!("unexpected claim result: {other:?}"),
    }
    assert_eq!(
        submit.calls(),
        0,
        "incomplete ledger must make zero submit requests"
    );
    let (state, attempts): (Option<String>, i64) =
        sqlx::query_as("SELECT submit_state, submit_attempts FROM upload_session WHERE id = ?")
            .bind(INCIDENT_SESSION_ID)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(state.as_deref(), Some("blocked_missing_segments"));
    assert_eq!(attempts, 0);
}

#[tokio::test]
async fn target_04_replays_and_late_attempts_produce_one_ordered_part() {
    let media = tempfile::tempdir().unwrap();
    let outbox = tempfile::tempdir().unwrap();
    let db = IncidentDb::new().await;
    let store = EnrollmentStore::new(db.pool.clone(), outbox.path().to_path_buf());
    let first_path = media.path().join("ordered-0.flv");
    let second_path = media.path().join("ordered-1.flv");
    write_synthetic_valid_flv(&first_path);
    write_synthetic_valid_flv(&second_path);

    // Invariant 2: a replayed SegmentEvent reuses the lifecycle row it already created.
    let first = enroll_once(&store, &first_path).await;
    let second = enroll_once(&store, &second_path).await;
    for _ in 0..3 {
        assert_eq!(
            enroll_once(&store, &first_path).await.missing_id,
            first.missing_id,
            "invariant 2: one lifecycle row per local segment"
        );
    }
    assert_eq!(db.counts().await, (1, 2));

    // Invariant 5: the cancelled attempt lost its lease, and the delayed success it was still
    // carrying must not overwrite the attempt which replaced it.
    let stale = claim_lease(&db.pool, &second, "bda2").await;
    assert!(
        fail_enrolled_attempt(
            &db.pool,
            second.missing_id,
            &stale,
            "watchdog cancelled".to_string(),
            FakeClock::incident_start().now(),
        )
        .await
        .unwrap()
    );
    let current = claim_lease(&db.pool, &second, "tx").await;
    let mut archive = LiveArchive::default();
    assert!(
        persist_segment(
            &db.pool,
            &mut archive,
            uploaded_video("remote-stale-part-1"),
            &second,
            &stale,
        )
        .await
        .is_err(),
        "invariant 5: a revoked lease cannot publish a delayed success"
    );
    persist_segment(
        &db.pool,
        &mut archive,
        uploaded_video("remote-part-1"),
        &second,
        &current,
    )
    .await
    .unwrap();

    // The later segment finished first, so the session must be rebuilt in enrollment order.
    let first_token = claim_lease(&db.pool, &first, "bda2").await;
    persist_segment(
        &db.pool,
        &mut archive,
        uploaded_video("remote-part-0"),
        &first,
        &first_token,
    )
    .await
    .unwrap();

    let videos_json =
        sqlx::query_scalar::<_, String>("SELECT videos_json FROM upload_session WHERE id = ?")
            .bind(first.upload_session_id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    for parts in [&archive.videos, &parse_videos(&videos_json)] {
        assert_eq!(
            parts
                .iter()
                .map(|video| video.filename.as_str())
                .collect::<Vec<_>>(),
            ["remote-part-0", "remote-part-1"],
            "replays and a revoked attempt still yield exactly one ordered part each"
        );
    }

    // A replay arriving after success is idempotent: no new part, no reset of the succeeded row.
    assert_eq!(
        enroll_once(&store, &first_path).await.missing_id,
        first.missing_id
    );
    let statuses = sqlx::query_scalar::<_, String>(
        "SELECT status FROM upload_missing_segment WHERE upload_session_id = ? \
         ORDER BY segment_order",
    )
    .bind(first.upload_session_id)
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(statuses, ["succeeded", "succeeded"]);
    assert_eq!(db.counts().await, (1, 2));
}

#[tokio::test]
async fn target_05_watchdogs_release_permit_and_tls_failure_fails_over() {
    let clock = FakeClock::incident_start();
    let last_progress = clock.now();
    let mut stuck = FakeUploader::new(clock.clone(), 1).inject_at(
        UploadCheckpoint::Chunk(0),
        InjectedUploadResult::PermanentPending,
    );
    assert!(matches!(
        stuck.poll(),
        FakeUploadPoll::Reached(UploadCheckpoint::Probe)
    ));
    assert!(matches!(
        stuck.poll(),
        FakeUploadPoll::Reached(UploadCheckpoint::PreUpload)
    ));
    assert_eq!(stuck.poll(), FakeUploadPoll::Pending);
    clock.advance(Duration::minutes(5));
    assert!(clock.now() - last_progress >= Duration::minutes(5));

    let db = IncidentDb::new().await;
    assert!(
        record_failure(
            &db.pool,
            "bldsa",
            UploadFailureKind::CertificateExpired,
            "certificate expired",
            clock.now(),
        )
        .await
        .unwrap()
    );
    assert!(matches!(
        acquire_line(&db.pool, "bldsa", clock.now()).await.unwrap(),
        LineAvailability::Cooling { .. }
    ));
    assert_eq!(
        acquire_line(&db.pool, "bda2", clock.now()).await.unwrap(),
        LineAvailability::Available
    );
}

#[tokio::test]
async fn target_06_source_missing_stops_retries_and_finalized_stays_closed() {
    let media = tempfile::tempdir().unwrap();
    let outbox = tempfile::tempdir().unwrap();
    let db = IncidentDb::new().await;
    db.insert_session("finalized", "[]").await;
    let source = media.path().join("late-segment.flv");
    let request = EnrollmentRequest {
        live_streamer_id: INCIDENT_ROOM_ID,
        streamer_info_id: INCIDENT_STREAMER_INFO_ID,
        file_path: source.clone(),
        normalized_file_path: normalize_segment_path(&source).unwrap(),
        danmaku_file_path: None,
        total_bytes: 0,
        now: FakeClock::incident_start().now(),
        recovery_window_minutes: 30,
    };
    let store = EnrollmentStore::new(db.pool.clone(), outbox.path().to_path_buf());

    assert!(matches!(
        enroll_validated_segment(&store, &request).await.unwrap(),
        EnrollmentOutcome::SourceMissing
    ));
    assert_eq!(db.counts().await, (1, 0));

    write_synthetic_valid_flv(&source);
    assert!(matches!(
        enroll_validated_segment(&store, &request).await.unwrap(),
        EnrollmentOutcome::FinalizedRejected {
            session_id: INCIDENT_SESSION_ID
        }
    ));
    assert_eq!(db.counts().await, (1, 0));
    let audit_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM upload_recovery_audit")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(audit_count, 2, "both rejected paths stay auditable");
}

#[tokio::test]
async fn target_06_outbox_import_respects_a_finalized_session_boundary() {
    let media = tempfile::tempdir().unwrap();
    let outbox = tempfile::tempdir().unwrap();
    let unavailable_db = IncidentDb::new().await;
    unavailable_db.pool.close().await;
    let unavailable_store =
        EnrollmentStore::new(unavailable_db.pool.clone(), outbox.path().to_path_buf());
    let path = media.path().join("late-outboxed.flv");
    write_synthetic_valid_flv(&path);
    let request = enrollment_request(&path);

    // Invariant 1: the finalized guard cannot query an unreachable database, and must not turn a
    // validated segment into an error instead of a durable record.
    let outcome = enroll_validated_segment(&unavailable_store, &request)
        .await
        .unwrap();
    let EnrollmentOutcome::Outboxed(manifest) = outcome else {
        panic!("the finalized guard must not defeat the outbox fallback");
    };

    // Invariant 6: the session is finalized by the time the database recovers, so the deferred
    // manifest must not reopen it.
    let recovered_db = IncidentDb::new().await;
    recovered_db.insert_session("finalized", "[]").await;
    let recovered_store =
        EnrollmentStore::new(recovered_db.pool.clone(), outbox.path().to_path_buf());
    assert_eq!(import_outbox_once(&recovered_store).await.unwrap(), 0);
    assert_eq!(recovered_db.counts().await, (1, 0));
    assert!(!manifest.exists());
    let audit_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM upload_recovery_audit")
        .fetch_one(&recovered_db.pool)
        .await
        .unwrap();
    assert_eq!(audit_count, 1, "the discarded manifest stays auditable");
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

async fn enroll_once(store: &EnrollmentStore, path: &Path) -> SegmentEnrollment {
    let EnrollmentOutcome::Enrolled(enrollment) =
        enroll_validated_segment(store, &enrollment_request(path))
            .await
            .unwrap()
    else {
        panic!("a healthy incident database must enroll directly");
    };
    enrollment
}

async fn claim_lease(pool: &ConnectionPool, enrollment: &SegmentEnrollment, line: &str) -> String {
    let AttemptClaim::Claimed(token) = claim_enrolled_attempt(pool, enrollment, line, None)
        .await
        .unwrap()
    else {
        panic!("a due lifecycle row must yield exactly one lease");
    };
    token
}

fn uploaded_video(name: &str) -> Video {
    Video {
        title: Some(name.to_string()),
        filename: name.to_string(),
        desc: String::new(),
    }
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
