use biliup_cli::server::core::downloader::SegmentInfo;
use biliup_cli::server::infrastructure::connection_pool::{ConnectionManager, ConnectionPool};
use chrono::{DateTime, Duration, TimeZone, Utc};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

pub const INCIDENT_ROOM_ID: i64 = 10;
pub const INCIDENT_STREAMER_INFO_ID: i64 = 20;
pub const INCIDENT_SESSION_ID: i64 = 30;

#[derive(Clone, Debug)]
pub struct FakeClock {
    now: Arc<Mutex<DateTime<Utc>>>,
}

impl FakeClock {
    pub fn incident_start() -> Self {
        Self {
            now: Arc::new(Mutex::new(
                Utc.with_ymd_and_hms(2026, 8, 25, 22, 29, 0)
                    .single()
                    .expect("valid fixture timestamp"),
            )),
        }
    }

    pub fn now(&self) -> DateTime<Utc> {
        *self.now.lock().expect("fake clock poisoned")
    }

    pub fn advance(&self, duration: Duration) {
        let mut now = self.now.lock().expect("fake clock poisoned");
        *now += duration;
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum UploadCheckpoint {
    Probe,
    PreUpload,
    Chunk(usize),
    CompletedCallback,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InjectedUploadResult {
    TransportError,
    Http(u16),
    CertificateExpired { host: &'static str },
    PermanentPending,
    DelayedSuccess(Duration),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FakeUploadPoll {
    Paused(UploadCheckpoint),
    Reached(UploadCheckpoint),
    Progress { chunk: usize, uploaded_bytes: u64 },
    Pending,
    Completed,
    Failed(InjectedUploadResult),
}

/// Deterministic uploader used by the incident contracts. It performs no network I/O.
pub struct FakeUploader {
    checkpoints: Vec<UploadCheckpoint>,
    pause_at: HashSet<UploadCheckpoint>,
    failure_at: BTreeMap<usize, InjectedUploadResult>,
    cursor: usize,
    delayed_until: Option<DateTime<Utc>>,
    chunk_bytes: u64,
    clock: FakeClock,
    tls_verification_enabled: Arc<AtomicBool>,
}

impl FakeUploader {
    pub fn new(clock: FakeClock, chunks: usize) -> Self {
        let mut checkpoints = vec![UploadCheckpoint::Probe, UploadCheckpoint::PreUpload];
        checkpoints.extend((0..chunks).map(UploadCheckpoint::Chunk));
        checkpoints.push(UploadCheckpoint::CompletedCallback);
        Self {
            checkpoints,
            pause_at: HashSet::new(),
            failure_at: BTreeMap::new(),
            cursor: 0,
            delayed_until: None,
            chunk_bytes: 8 * 1024 * 1024,
            clock,
            tls_verification_enabled: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn pause_at(mut self, checkpoint: UploadCheckpoint) -> Self {
        self.pause_at.insert(checkpoint);
        self
    }

    pub fn inject_at(mut self, checkpoint: UploadCheckpoint, result: InjectedUploadResult) -> Self {
        let index = self
            .checkpoints
            .iter()
            .position(|candidate| candidate == &checkpoint)
            .expect("injected checkpoint belongs to this upload");
        self.failure_at.insert(index, result);
        self
    }

    pub fn resume(&mut self, checkpoint: &UploadCheckpoint) {
        self.pause_at.remove(checkpoint);
    }

    pub fn tls_verification_enabled(&self) -> bool {
        self.tls_verification_enabled.load(Ordering::SeqCst)
    }

    pub fn poll(&mut self) -> FakeUploadPoll {
        if let Some(until) = self.delayed_until {
            if self.clock.now() < until {
                return FakeUploadPoll::Pending;
            }
            self.delayed_until = None;
            self.cursor = self.cursor.saturating_add(1);
            return FakeUploadPoll::Completed;
        }

        let Some(checkpoint) = self.checkpoints.get(self.cursor).cloned() else {
            return FakeUploadPoll::Completed;
        };
        if self.pause_at.contains(&checkpoint) {
            return FakeUploadPoll::Paused(checkpoint);
        }
        if let Some(result) = self.failure_at.get(&self.cursor).cloned() {
            match result {
                InjectedUploadResult::PermanentPending => return FakeUploadPoll::Pending,
                InjectedUploadResult::DelayedSuccess(delay) => {
                    self.delayed_until = Some(self.clock.now() + delay);
                    return FakeUploadPoll::Pending;
                }
                failure => return FakeUploadPoll::Failed(failure),
            }
        }
        self.cursor += 1;
        match checkpoint {
            UploadCheckpoint::Chunk(chunk) => FakeUploadPoll::Progress {
                chunk,
                uploaded_bytes: (chunk as u64 + 1) * self.chunk_bytes,
            },
            UploadCheckpoint::CompletedCallback => FakeUploadPoll::Completed,
            checkpoint => FakeUploadPoll::Reached(checkpoint),
        }
    }
}

#[derive(Clone, Default)]
pub struct SubmitSpy(Arc<AtomicUsize>);

impl SubmitSpy {
    pub fn submit(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    pub fn calls(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

pub struct IncidentDb {
    _directory: tempfile::TempDir,
    pub pool: ConnectionPool,
}

impl IncidentDb {
    pub async fn new() -> Self {
        let directory = tempfile::tempdir().expect("create incident fixture directory");
        let db_path = directory.path().join("incident.sqlite3");
        let pool = ConnectionManager::new_pool(db_path.to_str().expect("UTF-8 fixture path"))
            .await
            .expect("create migrated incident database");
        let now = FakeClock::incident_start().now();
        sqlx::query("INSERT INTO livestreamers (id, url, remark) VALUES (?1, ?2, ?3)")
            .bind(INCIDENT_ROOM_ID)
            .bind("https://example.invalid/live/synthetic-room")
            .bind("synthetic-streamer")
            .execute(&pool)
            .await
            .expect("seed live streamer");
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
        .expect("seed streamer info");
        Self {
            _directory: directory,
            pool,
        }
    }

    pub async fn insert_session(&self, status: &str, videos_json: &str) {
        let now = FakeClock::incident_start().now();
        sqlx::query(
            "INSERT INTO upload_session \
             (id, live_streamer_id, streamer_info_id, aid, bvid, videos_json, status, created_at, updated_at) \
             VALUES (?1, ?2, ?3, NULL, NULL, ?4, ?5, ?6, ?6)",
        )
        .bind(INCIDENT_SESSION_ID)
        .bind(INCIDENT_ROOM_ID)
        .bind(INCIDENT_STREAMER_INFO_ID)
        .bind(videos_json)
        .bind(status)
        .bind(now)
        .execute(&self.pool)
        .await
        .expect("insert incident upload session");
    }

    pub async fn insert_missing(&self, path: &Path, order: i64, status: &str) {
        let now = FakeClock::incident_start().now();
        sqlx::query(
            "INSERT INTO upload_missing_segment \
             (live_streamer_id, streamer_info_id, upload_session_id, aid, file_path, \
              danmaku_file_path, segment_order, status, attempts, line_index, next_retry_at, \
              last_error, created_at, updated_at) \
             VALUES (?1, ?2, ?3, NULL, ?4, NULL, ?5, ?6, 0, 0, ?7, NULL, ?7, ?7)",
        )
        .bind(INCIDENT_ROOM_ID)
        .bind(INCIDENT_STREAMER_INFO_ID)
        .bind(INCIDENT_SESSION_ID)
        .bind(path.display().to_string())
        .bind(order)
        .bind(status)
        .bind(now)
        .execute(&self.pool)
        .await
        .expect("insert incident lifecycle row");
    }

    pub async fn counts(&self) -> (i64, i64) {
        let sessions = sqlx::query_scalar("SELECT COUNT(*) FROM upload_session")
            .fetch_one(&self.pool)
            .await
            .expect("count sessions");
        let missing = sqlx::query_scalar("SELECT COUNT(*) FROM upload_missing_segment")
            .fetch_one(&self.pool)
            .await
            .expect("count lifecycle rows");
        (sessions, missing)
    }
}

pub fn incident_segments(root: &Path) -> Vec<SegmentInfo> {
    ["22:29:32", "22:59:56", "23:30:19"]
        .into_iter()
        .enumerate()
        .map(|(index, time)| {
            SegmentInfo::new(
                root.join(format!("synthetic-streamer 2026-08-25 {time}.flv")),
                None,
                None,
                index,
            )
        })
        .collect()
}

pub fn write_synthetic_valid_flv(path: &Path) {
    let payload_size = 1024usize;
    let mut bytes = vec![
        b'F', b'L', b'V', 1, 5, 0, 0, 0, 9, 0, 0, 0, 0, // header + PreviousTagSize0
        9, 0, 4, 0, 0x1b, 0x77, 0x40, 0, 0, 0, 0, // video tag, 30-minute timestamp
    ];
    bytes.extend_from_slice(&[0x17, 1, 0, 0, 0, 0x65]);
    bytes.resize(bytes.len() + payload_size - 6, 0);
    bytes.extend_from_slice(&((11 + payload_size) as u32).to_be_bytes());
    std::fs::write(path, bytes).expect("write synthetic FLV fixture");
}

pub fn distinct_source_identity(path: &Path) -> PathBuf {
    path.components().collect()
}
