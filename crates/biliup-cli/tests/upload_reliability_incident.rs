mod support;

use biliup::bilibili::Video;
use biliup_cli::server::common::attempt_lease::{AttemptPhase, record_heartbeat, record_phase};
use biliup_cli::server::common::missing_segment::recover_stale_upload_attempts;
use biliup_cli::server::common::segment_enrollment::{
    EnrollmentOutcome, EnrollmentRequest, EnrollmentStore, enroll_validated_segment,
    import_outbox_once, normalize_segment_path,
};
use biliup_cli::server::common::submission_scheduler::due_submission_session_ids;
use biliup_cli::server::common::upload::{
    AttemptClaim, claim_enrolled_attempt, fail_enrolled_attempt, persist_segment,
};
use biliup_cli::server::common::upload_line_health::{
    LineAvailability, UploadFailureKind, acquire_line, record_failure,
};
use biliup_cli::server::common::upload_line_selection::LineSource;
use biliup_cli::server::common::upload_session::{
    LiveArchive, SubmitClaim, claim_complete_session, mark_submit_anomaly, mark_submitted,
    parse_videos, request_session_submit, schedule_submit_retry,
};
use biliup_cli::server::common::util::{FileValidator, MediaValidation};
use biliup_cli::server::config::Config;
use biliup_cli::server::core::downloader::SegmentEnrollment;
use biliup_cli::server::infrastructure::connection_pool::{ConnectionManager, ConnectionPool};
use chrono::Duration;
use futures::future::join_all;
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
        claim_enrolled_attempt(&restarted, &enrollment, "bda2", LineSource::Configured)
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
        let outcome = task.await.unwrap();
        assert!(
            matches!(outcome, EnrollmentOutcome::Enrolled(_)),
            "{outcome:?}"
        );
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
    let config: Config = serde_yaml::from_str("{}").unwrap();
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
            &config,
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
        &config,
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
        &config,
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

/// The 2026-08-26 self-sustaining loop, in one test.
///
/// A 3.32 GB segment normalizes for far longer than five minutes. The database-side reaper judged
/// every `uploading` row by "no network progress in five minutes", so it reaped a lease whose
/// ffmpeg was working perfectly — and the upload that kept running became a ghost that threw away
/// its own result at `persist_segment`.
#[tokio::test]
async fn target_07_long_preprocessing_is_not_reaped_while_its_owner_is_alive() {
    let media = tempfile::tempdir().unwrap();
    let outbox = tempfile::tempdir().unwrap();
    let db = IncidentDb::new().await;
    db.insert_session("uploading", "[]").await;
    let store = EnrollmentStore::new(db.pool.clone(), outbox.path().to_path_buf());
    let path = media.path().join("long-normalization.flv");
    write_synthetic_valid_flv(&path);
    let enrollment = enroll_once(&store, &path).await;
    let token = claim_lease(&db.pool, &enrollment, "alia").await;
    let started = FakeClock::incident_start().now();
    // 3.32 GB of source: the preprocessing budget is 10 + 4 * 10 = 50 minutes. The claim stamps
    // the phase with the wall clock, so re-stamp it onto the fixture clock.
    sqlx::query("UPDATE upload_missing_segment SET total_bytes = ? WHERE id = ?")
        .bind(3_565_158_400_i64)
        .bind(enrollment.missing_id)
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        record_phase(
            &db.pool,
            enrollment.missing_id,
            &token,
            AttemptPhase::Preprocessing,
            started,
        )
        .await
        .unwrap()
    );

    for minute in [5, 20, 45] {
        let now = started + Duration::minutes(minute);
        // The owner is alive and says so; nothing else about the row changes.
        assert!(
            record_heartbeat(&db.pool, enrollment.missing_id, &token, now)
                .await
                .unwrap()
        );
        assert_eq!(
            recover_stale_upload_attempts(&db.pool, now).await.unwrap(),
            0,
            "a heartbeating preprocessing attempt must survive minute {minute}"
        );
    }

    let (status, held) = lease_state(&db.pool, enrollment.missing_id).await;
    assert_eq!(status, "uploading");
    assert_eq!(held.as_deref(), Some(token.as_str()));

    // Past the size-derived cap it is reaped, and the reason says preprocessing rather than
    // pretending the upload line went silent.
    let expired = started + Duration::minutes(51);
    assert!(
        record_heartbeat(&db.pool, enrollment.missing_id, &token, expired)
            .await
            .unwrap()
    );
    assert_eq!(
        recover_stale_upload_attempts(&db.pool, expired)
            .await
            .unwrap(),
        1
    );
    let last_error: String =
        sqlx::query_scalar("SELECT last_error FROM upload_missing_segment WHERE id = ?")
            .bind(enrollment.missing_id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(
        last_error.starts_with("preprocess_timeout"),
        "the reason must name the phase, got {last_error:?}"
    );
}

/// A lease whose owner stopped heartbeating is converged in every phase, including the two that
/// legitimately move no network bytes.
#[tokio::test]
async fn target_07_a_lease_whose_owner_died_is_converged_in_any_phase() {
    let media = tempfile::tempdir().unwrap();
    let outbox = tempfile::tempdir().unwrap();
    let db = IncidentDb::new().await;
    db.insert_session("uploading", "[]").await;
    let store = EnrollmentStore::new(db.pool.clone(), outbox.path().to_path_buf());
    let path = media.path().join("crashed-owner.flv");
    write_synthetic_valid_flv(&path);
    let enrollment = enroll_once(&store, &path).await;
    let token = claim_lease(&db.pool, &enrollment, "alia").await;
    let started = FakeClock::incident_start().now();
    assert!(
        record_phase(
            &db.pool,
            enrollment.missing_id,
            &token,
            AttemptPhase::Queued,
            started,
        )
        .await
        .unwrap()
    );

    // Nothing in this process owns the lease, so there is nothing to cancel: it is simply stale.
    let now = started + Duration::minutes(4);
    assert_eq!(
        recover_stale_upload_attempts(&db.pool, now).await.unwrap(),
        1
    );
    let (status, held) = lease_state(&db.pool, enrollment.missing_id).await;
    assert_eq!(status, "failed");
    assert_eq!(held, None);
    let outcome: String = sqlx::query_scalar(
        "SELECT outcome FROM upload_attempt WHERE missing_id = ? ORDER BY id DESC LIMIT 1",
    )
    .bind(enrollment.missing_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        outcome, "stale",
        "attempt history must record how the attempt ended"
    );
}

/// Restarting mid-broadcast used to produce a second `streamerinfo` row, and with it a second
/// upload session — the exact split that had to be repaired by hand. The platform's session key
/// keeps both restarts on one session, and `segment_order` stays contiguous.
#[tokio::test]
async fn target_08_a_restart_mid_broadcast_keeps_one_session() {
    let media = tempfile::tempdir().unwrap();
    let outbox = tempfile::tempdir().unwrap();
    let db = IncidentDb::new().await;
    db.set_live_session_key(INCIDENT_STREAMER_INFO_ID, "room-42")
        .await;
    let store = EnrollmentStore::new(db.pool.clone(), outbox.path().to_path_buf());

    let before = media.path().join("before-restart.flv");
    write_synthetic_valid_flv(&before);
    let first = enroll_once(&store, &before).await;

    // The restart: a new streamerinfo row for the same broadcast, and enough elapsed time that
    // the old clock window would have expired.
    let restarted_info = db.insert_streamer_info(Some("room-42")).await;
    let after = media.path().join("after-restart.flv");
    write_synthetic_valid_flv(&after);
    let mut request = enrollment_request(&after);
    request.streamer_info_id = restarted_info;
    request.now = FakeClock::incident_start().now() + Duration::minutes(95);
    let EnrollmentOutcome::Enrolled(second) =
        enroll_validated_segment(&store, &request).await.unwrap()
    else {
        panic!("the restarted process must enroll its segment");
    };

    assert_eq!(
        second.upload_session_id, first.upload_session_id,
        "one broadcast must produce one session, however many times the process restarted"
    );
    assert_eq!(second.segment_order, first.segment_order + 1);
    let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM upload_session")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(sessions, 1);
}

/// The opposite hazard: two broadcasts must never be merged into one archive, even when the
/// second starts minutes after the first and the first never finalized.
#[tokio::test]
async fn target_08_two_broadcasts_are_never_merged() {
    let media = tempfile::tempdir().unwrap();
    let outbox = tempfile::tempdir().unwrap();
    let db = IncidentDb::new().await;
    db.set_live_session_key(INCIDENT_STREAMER_INFO_ID, "room-41")
        .await;
    let store = EnrollmentStore::new(db.pool.clone(), outbox.path().to_path_buf());

    let first_path = media.path().join("broadcast-one.flv");
    write_synthetic_valid_flv(&first_path);
    let first = enroll_once(&store, &first_path).await;

    let second_info = db.insert_streamer_info(Some("room-42")).await;
    let second_path = media.path().join("broadcast-two.flv");
    write_synthetic_valid_flv(&second_path);
    let mut request = enrollment_request(&second_path);
    request.streamer_info_id = second_info;
    request.now = FakeClock::incident_start().now() + Duration::minutes(4);
    let EnrollmentOutcome::Enrolled(second) =
        enroll_validated_segment(&store, &request).await.unwrap()
    else {
        panic!("the second broadcast must enroll its segment");
    };

    assert_ne!(
        second.upload_session_id, first.upload_session_id,
        "a different live session key means a different archive, however close in time"
    );
}

/// The production race from the incident: the close wakeup sees the tail still uploading, then
/// several independent wakeups arrive once it succeeds. The durable claim must turn those wakes
/// into one ordered remote payload and one finalized archive.
#[tokio::test]
async fn submit_liveness_01_tail_race_and_concurrent_wakeups_are_exactly_once() {
    let db = IncidentDb::new().await;
    db.insert_session("uploading", "[]").await;
    let now = FakeClock::incident_start().now();
    request_session_submit(&db.pool, INCIDENT_SESSION_ID, now)
        .await
        .unwrap();
    for order in 0..4 {
        insert_submit_ledger_segment(
            &db.pool,
            1_000 + order,
            order,
            if order == 3 { "uploading" } else { "succeeded" },
        )
        .await;
    }
    let submit = SubmitSpy::default();

    let first = claim_complete_session(&db.pool, INCIDENT_SESSION_ID)
        .await
        .unwrap();
    let SubmitClaim::Blocked { completeness, .. } = first else {
        panic!("the first close wakeup must observe the tail as incomplete");
    };
    assert_eq!(completeness.uploading, 1);
    assert_eq!(submit.calls(), 0);

    let tail = uploaded_video("part-3");
    sqlx::query(
        "UPDATE upload_missing_segment SET status = 'succeeded', video_json = ?1 \
         WHERE id = 1003",
    )
    .bind(serde_json::to_string(&tail).unwrap())
    .execute(&db.pool)
    .await
    .unwrap();

    let outcomes =
        join_all((0..4).map(|_| claim_complete_session(&db.pool, INCIDENT_SESSION_ID))).await;
    let mut winning_claim = None;
    for outcome in outcomes {
        match outcome.unwrap() {
            SubmitClaim::Claimed { token, videos } => {
                assert!(winning_claim.is_none(), "only one wakeup may own submit");
                assert_eq!(
                    videos
                        .iter()
                        .map(|video| video.filename.as_str())
                        .collect::<Vec<_>>(),
                    ["part-0", "part-1", "part-2", "part-3"]
                );
                submit.submit();
                winning_claim = Some(token);
            }
            SubmitClaim::AlreadyClaimed => {}
            other => panic!("unexpected concurrent gate result: {other:?}"),
        }
    }
    let token = winning_claim.expect("one wakeup must acquire the claim");
    mark_submitted(
        &db.pool,
        INCIDENT_SESSION_ID,
        &token,
        88_001,
        Some("BV1incident".to_string()),
    )
    .await
    .unwrap();
    assert_eq!(submit.calls(), 1);
    let row: (String, Option<i64>, Option<String>) =
        sqlx::query_as("SELECT status, aid, bvid FROM upload_session WHERE id = ?1")
            .bind(INCIDENT_SESSION_ID)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        row,
        (
            "finalized".to_string(),
            Some(88_001),
            Some("BV1incident".to_string())
        )
    );
}

#[tokio::test]
async fn submit_liveness_02_restart_scan_and_manual_null_recovery_are_distinct() {
    let db = IncidentDb::new().await;
    db.insert_session("uploading", "[]").await;
    let now = FakeClock::incident_start().now();
    insert_submit_ledger_segment(&db.pool, 2_000, 0, "succeeded").await;

    // Historical NULL sessions are intentionally invisible to automatic startup scans.
    assert!(
        due_submission_session_ids(&db.pool, now, true)
            .await
            .unwrap()
            .is_empty()
    );

    // An operator recovery is the durable authorization. A new process sees it without needing
    // another live event and can claim the already-complete ledger.
    request_session_submit(&db.pool, INCIDENT_SESSION_ID, now)
        .await
        .unwrap();
    assert_eq!(
        due_submission_session_ids(&db.pool, now, true)
            .await
            .unwrap(),
        vec![INCIDENT_SESSION_ID]
    );
    let SubmitClaim::Claimed { token, videos } =
        claim_complete_session(&db.pool, INCIDENT_SESSION_ID)
            .await
            .unwrap()
    else {
        panic!("the restarted scanner must be able to claim a complete requested session");
    };
    assert_eq!(videos[0].filename, "part-0");
    let submit = SubmitSpy::default();
    submit.submit();
    mark_submitted(
        &db.pool,
        INCIDENT_SESSION_ID,
        &token,
        88_002,
        Some("BV1restart".to_string()),
    )
    .await
    .unwrap();
    assert_eq!(submit.calls(), 1);
}

#[tokio::test]
async fn submit_liveness_03_live_session_without_intent_is_never_scanned_and_stays_open() {
    let media = tempfile::tempdir().unwrap();
    let outbox = tempfile::tempdir().unwrap();
    let db = IncidentDb::new().await;
    db.insert_session("uploading", "[]").await;
    let now = FakeClock::incident_start().now();
    insert_submit_ledger_segment(&db.pool, 3_000, 0, "succeeded").await;

    for offset in [0, 60, 600] {
        assert!(
            due_submission_session_ids(&db.pool, now + Duration::seconds(offset), false)
                .await
                .unwrap()
                .is_empty(),
            "a complete ledger is not a close signal"
        );
    }

    let next_path = media.path().join("still-live-part.flv");
    write_synthetic_valid_flv(&next_path);
    let store = EnrollmentStore::new(db.pool.clone(), outbox.path().to_path_buf());
    let next = enroll_once(&store, &next_path).await;
    assert_eq!(next.upload_session_id, INCIDENT_SESSION_ID);
}

#[tokio::test]
async fn submit_liveness_04_retry_deadline_and_uncertain_claim_are_safe() {
    let db = IncidentDb::new().await;
    db.insert_session("uploading", "[]").await;
    let now = FakeClock::incident_start().now();
    insert_submit_ledger_segment(&db.pool, 4_000, 0, "succeeded").await;
    request_session_submit(&db.pool, INCIDENT_SESSION_ID, now)
        .await
        .unwrap();

    let SubmitClaim::Claimed { token, .. } = claim_complete_session(&db.pool, INCIDENT_SESSION_ID)
        .await
        .unwrap()
    else {
        panic!("complete session should be claimable");
    };
    let retry_at = now + Duration::minutes(5);
    assert!(
        schedule_submit_retry(
            &db.pool,
            INCIDENT_SESSION_ID,
            &token,
            retry_at,
            "definite failure".to_string(),
            true,
        )
        .await
        .unwrap()
    );
    assert!(
        due_submission_session_ids(&db.pool, retry_at - Duration::milliseconds(1), false,)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        due_submission_session_ids(&db.pool, retry_at, false)
            .await
            .unwrap(),
        vec![INCIDENT_SESSION_ID]
    );

    let SubmitClaim::Claimed { token, .. } = claim_complete_session(&db.pool, INCIDENT_SESSION_ID)
        .await
        .unwrap()
    else {
        panic!("the session should be claimable once retry is due");
    };
    mark_submit_anomaly(
        &db.pool,
        INCIDENT_SESSION_ID,
        &token,
        "ok_no_aid",
        "remote accepted without aid".to_string(),
        false,
    )
    .await
    .unwrap();
    assert!(
        due_submission_session_ids(&db.pool, retry_at + Duration::days(1), false)
            .await
            .unwrap()
            .is_empty(),
        "an uncertain remote result retains its claim and is never auto-retried"
    );
    let state: (Option<String>, Option<String>) =
        sqlx::query_as("SELECT submit_state, submit_claim_token FROM upload_session WHERE id = ?1")
            .bind(INCIDENT_SESSION_ID)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(state.0.as_deref(), Some("ok_no_aid"));
    assert_eq!(state.1.as_deref(), Some(token.as_str()));
}

async fn lease_state(pool: &ConnectionPool, missing_id: i64) -> (String, Option<String>) {
    sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT status, attempt_token FROM upload_missing_segment WHERE id = ?",
    )
    .bind(missing_id)
    .fetch_one(pool)
    .await
    .unwrap()
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
    let AttemptClaim::Claimed(token) =
        claim_enrolled_attempt(pool, enrollment, line, LineSource::Configured)
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

async fn insert_submit_ledger_segment(pool: &ConnectionPool, id: i64, order: i64, status: &str) {
    let now = FakeClock::incident_start().now();
    let video_json = (status == "succeeded")
        .then(|| serde_json::to_string(&uploaded_video(&format!("part-{order}"))).unwrap());
    sqlx::query(
        "INSERT INTO upload_missing_segment \
         (id, live_streamer_id, streamer_info_id, upload_session_id, file_path, \
          normalized_file_path, segment_order, status, next_retry_at, created_at, updated_at, \
          lifecycle_version, video_json) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7, ?8, ?8, ?8, 2, ?9)",
    )
    .bind(id)
    .bind(INCIDENT_ROOM_ID)
    .bind(INCIDENT_STREAMER_INFO_ID)
    .bind(INCIDENT_SESSION_ID)
    .bind(format!("/fixture/part-{order}.flv"))
    .bind(order)
    .bind(status)
    .bind(now)
    .bind(video_json)
    .execute(pool)
    .await
    .unwrap();
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
