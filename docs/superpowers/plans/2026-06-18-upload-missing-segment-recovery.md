# Upload Missing Segment Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build durable recovery for missed video segments so failed automatic uploads are silently retried before submit, and already-submitted archives can be manually patched with the missing segment at the correct part position.

**Architecture:** Add a durable `upload_missing_segment` queue that records each segment's intended order, local file, streamer/session context, retry state, and eventual target archive. Normal recording uploads continue, but failed segments are queued and a serialized recovery path can upload them later, insert their returned `Video` at the recorded order, and either update the pending `upload_session` before final submit or edit an existing Bilibili archive after final submit. Keep ordering logic pure and heavily tested, and isolate network side effects behind small functions.

**Tech Stack:** Rust 2024 workspace; `biliup-cli` server; SQLite migrations via `sqlx`; ORM via `ormlite`; async runtime via `tokio`; Bilibili upload/edit APIs from `biliup::bilibili`; tests run with `cargo test -p biliup-cli` and targeted module filters.

## Global Constraints

- Do not touch the existing unrelated local change in `BUILD_AND_DEPLOY.md`.
- Do not delete local segment files unless the segment upload result is durable in local DB state.
- Preserve existing behavior that successful segments are accumulated and submitted once at live end.
- Whole-video upload concurrency must be globally serialized for normal automatic upload and recovery upload.
- The missing segment record must include stable intended order so manual recovery can insert the part at the correct position.
- Initial upload-line fallback order is `bda2`, `tx`, `bldsa`, then `AUTO` probe.
- Manual recovery must edit the existing archive when the upload session has already been finalized with an `aid`; it must not create a new submission.
- Tests must be written and observed failing before production code changes.

---

## File Structure

- Create `crates/biliup-cli/migrations/5_add_upload_missing_segment.sql`
  - Creates durable queue table for failed/missing segments and indexes for due recovery.
- Modify `crates/biliup-cli/src/server/infrastructure/models.rs`
  - Adds `UploadMissingSegment` and `InsertUploadMissingSegment` ORM models.
- Create `crates/biliup-cli/src/server/common/missing_segment.rs`
  - Pure ordering helpers, queue state helpers, DB helpers, and unit tests.
- Modify `crates/biliup-cli/src/server/common/mod.rs`
  - Exposes `missing_segment` module.
- Modify `crates/biliup-cli/src/server/common/upload_session.rs`
  - Adds append/insert helpers for `Video` lists and optional finalized-session lookup for manual patching.
- Modify `crates/biliup-cli/src/server/common/upload.rs`
  - Adds global upload semaphore, wraps automatic upload with it, records missing segments on upload failure, runs due silent recovery before live-end submit, and provides manual archive patch helper.
- Modify `crates/biliup-cli/src/server/api/endpoints.rs`
  - Adds endpoints to list missing segments and trigger manual recovery for one missing segment.
- Modify `crates/biliup-cli/src/server/router.rs`
  - Routes the new endpoints.
- Optional front-end work is out of scope for this plan; the API will be enough for the existing UI or a later UI task to call.

---

### Task 1: Pure Ordering And Retry Policy

**Files:**
- Create: `crates/biliup-cli/src/server/common/missing_segment.rs`
- Modify: `crates/biliup-cli/src/server/common/mod.rs`
- Test: `crates/biliup-cli/src/server/common/missing_segment.rs`

**Interfaces:**
- Produces: `pub const FALLBACK_LINES: [RecoveryUploadLine; 4]`
- Produces: `pub enum RecoveryUploadLine { Bda2, Tx, Bldsa, Auto }`
- Produces: `pub fn next_line_index(current: i64) -> i64`
- Produces: `pub fn retry_delay_for_attempt(attempts: i64) -> chrono::Duration`
- Produces: `pub fn insert_video_at_order(videos: &mut Vec<Video>, video: Video, segment_order: i64)`
- Produces: `pub fn normalize_segment_order(existing_count: usize, segment_order: i64) -> usize`

- [ ] **Step 1: Write failing tests for part insertion and line rotation**

Add `crates/biliup-cli/src/server/common/missing_segment.rs` with tests first:

```rust
use biliup::bilibili::Video;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryUploadLine {
    Bda2,
    Tx,
    Bldsa,
    Auto,
}

pub const FALLBACK_LINES: [RecoveryUploadLine; 4] = [
    RecoveryUploadLine::Bda2,
    RecoveryUploadLine::Tx,
    RecoveryUploadLine::Bldsa,
    RecoveryUploadLine::Auto,
];

pub fn next_line_index(_current: i64) -> i64 {
    unimplemented!("implemented in Step 3")
}

pub fn retry_delay_for_attempt(_attempts: i64) -> chrono::Duration {
    unimplemented!("implemented in Step 3")
}

pub fn normalize_segment_order(_existing_count: usize, _segment_order: i64) -> usize {
    unimplemented!("implemented in Step 3")
}

pub fn insert_video_at_order(_videos: &mut Vec<Video>, _video: Video, _segment_order: i64) {
    unimplemented!("implemented in Step 3")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(name: &str) -> Video {
        Video {
            title: Some(name.to_string()),
            filename: name.to_string(),
            desc: String::new(),
        }
    }

    #[test]
    fn inserts_missing_video_at_recorded_zero_based_order() {
        let mut videos = vec![video("p1"), video("p2"), video("p4")];

        insert_video_at_order(&mut videos, video("p3"), 2);

        let names: Vec<_> = videos.into_iter().map(|v| v.filename).collect();
        assert_eq!(names, vec!["p1", "p2", "p3", "p4"]);
    }

    #[test]
    fn appends_when_recorded_order_is_after_current_end() {
        let mut videos = vec![video("p1"), video("p2")];

        insert_video_at_order(&mut videos, video("p9"), 8);

        let names: Vec<_> = videos.into_iter().map(|v| v.filename).collect();
        assert_eq!(names, vec!["p1", "p2", "p9"]);
    }

    #[test]
    fn clamps_negative_order_to_front() {
        assert_eq!(normalize_segment_order(3, -5), 0);
    }

    #[test]
    fn rotates_upload_lines_through_all_fallbacks() {
        assert_eq!(next_line_index(0), 1);
        assert_eq!(next_line_index(1), 2);
        assert_eq!(next_line_index(2), 3);
        assert_eq!(next_line_index(3), 0);
        assert_eq!(next_line_index(7), 0);
    }

    #[test]
    fn retry_delay_starts_at_ten_minutes_and_caps_at_six_hours() {
        assert_eq!(retry_delay_for_attempt(0), chrono::Duration::minutes(10));
        assert_eq!(retry_delay_for_attempt(1), chrono::Duration::minutes(30));
        assert_eq!(retry_delay_for_attempt(2), chrono::Duration::hours(1));
        assert_eq!(retry_delay_for_attempt(3), chrono::Duration::hours(2));
        assert_eq!(retry_delay_for_attempt(9), chrono::Duration::hours(6));
    }
}
```

Modify `crates/biliup-cli/src/server/common/mod.rs`:

```rust
pub mod missing_segment;
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cargo test -p biliup-cli missing_segment -- --nocapture
```

Expected: tests compile and fail with panic messages containing `implemented in Step 3`.

- [ ] **Step 3: Implement minimal pure helpers**

Replace the `unimplemented!` helper bodies in `missing_segment.rs`:

```rust
pub fn next_line_index(current: i64) -> i64 {
    let len = FALLBACK_LINES.len() as i64;
    if current < 0 {
        0
    } else {
        (current + 1) % len
    }
}

pub fn retry_delay_for_attempt(attempts: i64) -> chrono::Duration {
    match attempts {
        i if i <= 0 => chrono::Duration::minutes(10),
        1 => chrono::Duration::minutes(30),
        2 => chrono::Duration::hours(1),
        3 => chrono::Duration::hours(2),
        _ => chrono::Duration::hours(6),
    }
}

pub fn normalize_segment_order(existing_count: usize, segment_order: i64) -> usize {
    if segment_order <= 0 {
        return 0;
    }
    (segment_order as usize).min(existing_count)
}

pub fn insert_video_at_order(videos: &mut Vec<Video>, video: Video, segment_order: i64) {
    let index = normalize_segment_order(videos.len(), segment_order);
    videos.insert(index, video);
}
```

- [ ] **Step 4: Verify tests pass**

Run:

```bash
cargo test -p biliup-cli missing_segment -- --nocapture
```

Expected: all `missing_segment` tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/biliup-cli/src/server/common/mod.rs crates/biliup-cli/src/server/common/missing_segment.rs
git commit -m "test: add missing segment ordering helpers"
```

---

### Task 2: Durable Missing Segment Model And Migration

**Files:**
- Create: `crates/biliup-cli/migrations/5_add_upload_missing_segment.sql`
- Modify: `crates/biliup-cli/src/server/infrastructure/models.rs`
- Modify: `crates/biliup-cli/src/server/common/missing_segment.rs`
- Test: `crates/biliup-cli/src/server/common/missing_segment.rs`

**Interfaces:**
- Consumes: `RecoveryUploadLine`, `next_line_index`, `retry_delay_for_attempt`
- Produces: `UploadMissingSegment` ORM model
- Produces: `InsertUploadMissingSegment` ORM insert model
- Produces: `pub fn mark_retry_failure(row: &mut UploadMissingSegment, error: String, now: DateTime<Utc>)`
- Produces: `pub fn mark_retry_success(row: &mut UploadMissingSegment, now: DateTime<Utc>)`

- [ ] **Step 1: Write failing state-transition tests**

Append to the `#[cfg(test)] mod tests` in `crates/biliup-cli/src/server/common/missing_segment.rs`:

```rust
use crate::server::infrastructure::models::UploadMissingSegment;
use chrono::TimeZone;

fn missing_row() -> UploadMissingSegment {
    let now = chrono::Utc.with_ymd_and_hms(2026, 6, 18, 12, 0, 0).unwrap();
    UploadMissingSegment {
        id: 1,
        live_streamer_id: 10,
        streamer_info_id: 20,
        upload_session_id: Some(30),
        aid: None,
        file_path: "/opt/p3.flv".to_string(),
        danmaku_file_path: None,
        segment_order: 2,
        status: "pending".to_string(),
        attempts: 0,
        line_index: 0,
        next_retry_at: now,
        last_error: None,
        created_at: now,
        updated_at: now,
    }
}

#[test]
fn failure_transition_rotates_line_and_schedules_retry() {
    let mut row = missing_row();
    let now = chrono::Utc.with_ymd_and_hms(2026, 6, 18, 12, 5, 0).unwrap();

    mark_retry_failure(&mut row, "timeout".to_string(), now);

    assert_eq!(row.status, "failed");
    assert_eq!(row.attempts, 1);
    assert_eq!(row.line_index, 1);
    assert_eq!(row.last_error.as_deref(), Some("timeout"));
    assert_eq!(row.next_retry_at, now + chrono::Duration::minutes(30));
    assert_eq!(row.updated_at, now);
}

#[test]
fn success_transition_marks_row_succeeded_without_changing_order() {
    let mut row = missing_row();
    row.status = "uploading".to_string();
    row.attempts = 2;
    let now = chrono::Utc.with_ymd_and_hms(2026, 6, 18, 12, 5, 0).unwrap();

    mark_retry_success(&mut row, now);

    assert_eq!(row.status, "succeeded");
    assert_eq!(row.attempts, 2);
    assert_eq!(row.segment_order, 2);
    assert_eq!(row.updated_at, now);
}
```

Also add placeholder functions above tests:

```rust
use crate::server::infrastructure::models::UploadMissingSegment;
use chrono::{DateTime, Utc};

pub fn mark_retry_failure(_row: &mut UploadMissingSegment, _error: String, _now: DateTime<Utc>) {
    unimplemented!("implemented in Task 2 Step 4")
}

pub fn mark_retry_success(_row: &mut UploadMissingSegment, _now: DateTime<Utc>) {
    unimplemented!("implemented in Task 2 Step 4")
}
```

- [ ] **Step 2: Add ORM model with fields required by tests**

Modify `crates/biliup-cli/src/server/infrastructure/models.rs` after `UploadSession`:

```rust
/// Missing segment recovery queue.
#[derive(Model, Debug, Clone, Serialize, Deserialize)]
#[ormlite(table = "upload_missing_segment", insert = "InsertUploadMissingSegment")]
pub struct UploadMissingSegment {
    pub id: i64,
    pub live_streamer_id: i64,
    pub streamer_info_id: i64,
    pub upload_session_id: Option<i64>,
    pub aid: Option<i64>,
    pub file_path: String,
    pub danmaku_file_path: Option<String>,
    pub segment_order: i64,
    pub status: String,
    pub attempts: i64,
    pub line_index: i64,
    pub next_retry_at: DateTime<Utc>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
```

- [ ] **Step 3: Run tests to verify transition helpers fail**

Run:

```bash
cargo test -p biliup-cli missing_segment -- --nocapture
```

Expected: new tests fail with panic `implemented in Task 2 Step 4`.

- [ ] **Step 4: Implement state transitions**

Replace placeholders in `missing_segment.rs`:

```rust
pub fn mark_retry_failure(row: &mut UploadMissingSegment, error: String, now: DateTime<Utc>) {
    row.attempts += 1;
    row.line_index = next_line_index(row.line_index);
    row.status = "failed".to_string();
    row.last_error = Some(error);
    row.next_retry_at = now + retry_delay_for_attempt(row.attempts);
    row.updated_at = now;
}

pub fn mark_retry_success(row: &mut UploadMissingSegment, now: DateTime<Utc>) {
    row.status = "succeeded".to_string();
    row.updated_at = now;
}
```

- [ ] **Step 5: Add migration**

Create `crates/biliup-cli/migrations/5_add_upload_missing_segment.sql`:

```sql
-- Durable queue for recorded segments whose upload failed or whose part still needs to be patched into an existing archive.
create table if not exists upload_missing_segment
(
    id INTEGER not null
        constraint pk_upload_missing_segment
            primary key,
    live_streamer_id INTEGER not null,
    streamer_info_id INTEGER not null,
    upload_session_id INTEGER,
    aid INTEGER,
    file_path VARCHAR not null,
    danmaku_file_path VARCHAR,
    segment_order INTEGER not null,
    status VARCHAR not null default 'pending',
    attempts INTEGER not null default 0,
    line_index INTEGER not null default 0,
    next_retry_at DATETIME not null,
    last_error TEXT,
    created_at DATETIME not null,
    updated_at DATETIME not null,
    constraint fk_upload_missing_segment_streamer_info_id_streamerinfo
        foreign key (streamer_info_id) references streamerinfo (id)
        on delete cascade,
    constraint fk_upload_missing_segment_upload_session_id_upload_session
        foreign key (upload_session_id) references upload_session (id)
        on delete set null
);

create unique index if not exists ux_upload_missing_segment_active_file
    on upload_missing_segment (live_streamer_id, file_path)
    where status in ('pending', 'uploading', 'failed');

create index if not exists ix_upload_missing_segment_due
    on upload_missing_segment (status, next_retry_at, updated_at);

create index if not exists ix_upload_missing_segment_session_order
    on upload_missing_segment (upload_session_id, segment_order);
```

- [ ] **Step 6: Verify tests pass**

Run:

```bash
cargo test -p biliup-cli missing_segment -- --nocapture
```

Expected: all `missing_segment` tests pass.

- [ ] **Step 7: Commit**

```bash
git add crates/biliup-cli/migrations/5_add_upload_missing_segment.sql crates/biliup-cli/src/server/infrastructure/models.rs crates/biliup-cli/src/server/common/missing_segment.rs
git commit -m "feat: add missing segment recovery model"
```

---

### Task 3: Persist Missing Segments On Automatic Upload Failure

**Files:**
- Modify: `crates/biliup-cli/src/server/common/missing_segment.rs`
- Modify: `crates/biliup-cli/src/server/common/upload.rs`
- Test: `crates/biliup-cli/src/server/common/missing_segment.rs`

**Interfaces:**
- Consumes: `InsertUploadMissingSegment`, `UploadMissingSegment`
- Produces: `pub fn next_segment_order(successful_count: usize, missing_before_or_at_end: usize) -> i64`
- Produces: `pub async fn enqueue_missing_segment(...) -> AppResult<()>`
- Produces: upload pipeline calls `enqueue_missing_segment` when `upload_single_file` fails.

- [ ] **Step 1: Write failing test for segment order calculation**

Append to `missing_segment.rs` tests:

```rust
#[test]
fn next_segment_order_counts_successes_and_prior_missing_segments() {
    assert_eq!(next_segment_order(0, 0), 0);
    assert_eq!(next_segment_order(2, 0), 2);
    assert_eq!(next_segment_order(2, 1), 3);
}
```

Add placeholder above tests:

```rust
pub fn next_segment_order(_successful_count: usize, _missing_before_or_at_end: usize) -> i64 {
    unimplemented!("implemented in Task 3 Step 3")
}
```

- [ ] **Step 2: Run test to verify failure**

Run:

```bash
cargo test -p biliup-cli next_segment_order -- --nocapture
```

Expected: fails with `implemented in Task 3 Step 3`.

- [ ] **Step 3: Implement order calculation**

Replace placeholder:

```rust
pub fn next_segment_order(successful_count: usize, missing_before_or_at_end: usize) -> i64 {
    (successful_count + missing_before_or_at_end) as i64
}
```

- [ ] **Step 4: Add DB enqueue helper**

Add imports to `missing_segment.rs`:

```rust
use crate::server::errors::{AppError, AppResult};
use crate::server::infrastructure::connection_pool::ConnectionPool;
use crate::server::infrastructure::models::InsertUploadMissingSegment;
use error_stack::ResultExt;
use ormlite::Insert;
use std::path::Path;
```

Add helper:

```rust
pub async fn enqueue_missing_segment(
    pool: &ConnectionPool,
    live_streamer_id: i64,
    streamer_info_id: i64,
    upload_session_id: Option<i64>,
    aid: Option<i64>,
    file_path: &Path,
    danmaku_file_path: Option<&Path>,
    segment_order: i64,
    error: String,
    now: DateTime<Utc>,
) -> AppResult<()> {
    let item = InsertUploadMissingSegment {
        live_streamer_id,
        streamer_info_id,
        upload_session_id,
        aid,
        file_path: file_path.display().to_string(),
        danmaku_file_path: danmaku_file_path.map(|p| p.display().to_string()),
        segment_order,
        status: "failed".to_string(),
        attempts: 1,
        line_index: 1,
        next_retry_at: now + retry_delay_for_attempt(1),
        last_error: Some(error),
        created_at: now,
        updated_at: now,
    };

    let sql = r#"
        INSERT INTO upload_missing_segment
            (live_streamer_id, streamer_info_id, upload_session_id, aid, file_path, danmaku_file_path,
             segment_order, status, attempts, line_index, next_retry_at, last_error, created_at, updated_at)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
        ON CONFLICT(live_streamer_id, file_path) WHERE status IN ('pending', 'uploading', 'failed')
        DO UPDATE SET
            upload_session_id = excluded.upload_session_id,
            aid = COALESCE(upload_missing_segment.aid, excluded.aid),
            segment_order = excluded.segment_order,
            status = 'failed',
            attempts = upload_missing_segment.attempts + 1,
            line_index = excluded.line_index,
            next_retry_at = excluded.next_retry_at,
            last_error = excluded.last_error,
            updated_at = excluded.updated_at
    "#;

    sqlx::query(sql)
        .bind(item.live_streamer_id)
        .bind(item.streamer_info_id)
        .bind(item.upload_session_id)
        .bind(item.aid)
        .bind(item.file_path)
        .bind(item.danmaku_file_path)
        .bind(item.segment_order)
        .bind(item.status)
        .bind(item.attempts)
        .bind(item.line_index)
        .bind(item.next_retry_at)
        .bind(item.last_error)
        .bind(item.created_at)
        .bind(item.updated_at)
        .execute(pool)
        .await
        .change_context(AppError::Unknown)?;

    Ok(())
}
```

- [ ] **Step 5: Wire automatic upload failure into queue**

In `crates/biliup-cli/src/server/common/upload.rs`, add imports:

```rust
use crate::server::common::missing_segment::{enqueue_missing_segment, next_segment_order};
```

In `pipeline_upload_videos`, before the loop add:

```rust
let mut missing_count = 0usize;
```

Inside the `Err(e)` arm for `upload_single_file`, replace the current log-only behavior with:

```rust
let err = format!("{e:?}");
let segment_order = next_segment_order(archive.videos.len(), missing_count);
missing_count += 1;
error!(file = ?upload_path, segment_order, "upload_single_file failed, queueing missing segment: {:?}", e);
if let Err(queue_err) = enqueue_missing_segment(
    ctx.pool(),
    ctx.worker_id(),
    ctx.id(),
    archive.session_row_id,
    archive.aid.map(|aid| aid as i64),
    &upload_path,
    event.danmaku_file_path.as_deref(),
    segment_order,
    err,
    chrono::Utc::now(),
)
.await
{
    error!(file = ?upload_path, "failed to enqueue missing segment: {:?}", queue_err);
}
```

- [ ] **Step 6: Verify tests pass**

Run:

```bash
cargo test -p biliup-cli missing_segment -- --nocapture
```

Expected: all missing segment tests pass.

- [ ] **Step 7: Commit**

```bash
git add crates/biliup-cli/src/server/common/missing_segment.rs crates/biliup-cli/src/server/common/upload.rs
git commit -m "feat: queue failed segment uploads"
```

---

### Task 4: Insert Missing Videos Into Pending Sessions Before Final Submit

**Files:**
- Modify: `crates/biliup-cli/src/server/common/upload_session.rs`
- Modify: `crates/biliup-cli/src/server/common/missing_segment.rs`
- Test: `crates/biliup-cli/src/server/common/upload_session.rs`

**Interfaces:**
- Consumes: `insert_video_at_order`
- Produces: `pub fn videos_with_inserted_segment(videos_json: &str, video: Video, segment_order: i64) -> AppResult<Vec<Video>>`
- Produces: `pub async fn insert_session_video_at_order(pool, session_row_id, video, segment_order) -> AppResult<Vec<Video>>`

- [ ] **Step 1: Write failing tests for session JSON insertion**

Append to `upload_session.rs` tests:

```rust
use biliup::bilibili::Video;

fn video(name: &str) -> Video {
    Video {
        title: Some(name.to_string()),
        filename: name.to_string(),
        desc: String::new(),
    }
}

#[test]
fn inserts_video_into_session_json_at_recorded_order() {
    let videos = vec![video("p1"), video("p2"), video("p4")];
    let json = serde_json::to_string(&videos).unwrap();

    let result = videos_with_inserted_segment(&json, video("p3"), 2).unwrap();

    let names: Vec<_> = result.into_iter().map(|v| v.filename).collect();
    assert_eq!(names, vec!["p1", "p2", "p3", "p4"]);
}
```

Add placeholder above tests:

```rust
pub fn videos_with_inserted_segment(
    _videos_json: &str,
    _video: Video,
    _segment_order: i64,
) -> AppResult<Vec<Video>> {
    unimplemented!("implemented in Task 4 Step 3")
}
```

- [ ] **Step 2: Run test to verify failure**

Run:

```bash
cargo test -p biliup-cli inserts_video_into_session_json_at_recorded_order -- --nocapture
```

Expected: fails with `implemented in Task 4 Step 3`.

- [ ] **Step 3: Implement JSON insertion helper**

Add import to `upload_session.rs`:

```rust
use crate::server::common::missing_segment::insert_video_at_order;
```

Replace placeholder:

```rust
pub fn videos_with_inserted_segment(
    videos_json: &str,
    video: Video,
    segment_order: i64,
) -> AppResult<Vec<Video>> {
    let mut videos: Vec<Video> = serde_json::from_str(videos_json).unwrap_or_default();
    insert_video_at_order(&mut videos, video, segment_order);
    Ok(videos)
}
```

- [ ] **Step 4: Add DB update helper**

Add to `upload_session.rs`:

```rust
pub async fn insert_session_video_at_order(
    pool: &ConnectionPool,
    session_row_id: i64,
    video: Video,
    segment_order: i64,
) -> AppResult<Vec<Video>> {
    let mut updated = Vec::new();
    mutate_session(pool, session_row_id, |row| {
        updated = videos_with_inserted_segment(&row.videos_json, video, segment_order)?;
        row.videos_json = serde_json::to_string(&updated).change_context(AppError::Unknown)?;
        Ok(())
    })
    .await?;
    Ok(updated)
}
```

- [ ] **Step 5: Verify tests pass**

Run:

```bash
cargo test -p biliup-cli upload_session -- --nocapture
```

Expected: upload_session tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/biliup-cli/src/server/common/upload_session.rs
git commit -m "feat: insert recovered parts into upload sessions"
```

---

### Task 5: Serialized Silent Recovery Before Live-End Submit

**Files:**
- Modify: `crates/biliup-cli/src/server/common/missing_segment.rs`
- Modify: `crates/biliup-cli/src/server/common/upload.rs`
- Test: `crates/biliup-cli/src/server/common/missing_segment.rs`

**Interfaces:**
- Consumes: `insert_session_video_at_order`
- Produces: `pub fn recovery_line(line: RecoveryUploadLine, client: &reqwest::Client) -> impl Future<Output = AppResult<Line>>`
- Produces: `pub async fn due_missing_segments_for_session(pool, upload_session_id, now) -> AppResult<Vec<UploadMissingSegment>>`
- Produces: `pub async fn recover_due_missing_segments(...) -> AppResult<Vec<Video>>`
- Produces: global upload semaphore used by normal upload and recovery upload.

- [ ] **Step 1: Write failing test for due-status predicate**

In `missing_segment.rs`, add pure helper placeholder:

```rust
pub fn is_due_for_silent_recovery(status: &str, next_retry_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    unimplemented!("implemented in Task 5 Step 3")
}
```

Append tests:

```rust
#[test]
fn silent_recovery_only_picks_pending_or_failed_rows_that_are_due() {
    let now = chrono::Utc.with_ymd_and_hms(2026, 6, 18, 12, 0, 0).unwrap();
    assert!(is_due_for_silent_recovery("pending", now, now));
    assert!(is_due_for_silent_recovery("failed", now - chrono::Duration::minutes(1), now));
    assert!(!is_due_for_silent_recovery("failed", now + chrono::Duration::minutes(1), now));
    assert!(!is_due_for_silent_recovery("uploading", now, now));
    assert!(!is_due_for_silent_recovery("succeeded", now, now));
}
```

- [ ] **Step 2: Run test to verify failure**

Run:

```bash
cargo test -p biliup-cli silent_recovery_only_picks -- --nocapture
```

Expected: fails with `implemented in Task 5 Step 3`.

- [ ] **Step 3: Implement due predicate**

Replace placeholder:

```rust
pub fn is_due_for_silent_recovery(status: &str, next_retry_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    matches!(status, "pending" | "failed") && next_retry_at <= now
}
```

- [ ] **Step 4: Add global upload semaphore**

In `upload.rs`, add imports:

```rust
use std::sync::OnceLock;
use tokio::sync::{Semaphore, OwnedSemaphorePermit};
```

Add near `UploadContext`:

```rust
static GLOBAL_UPLOAD_SEMAPHORE: OnceLock<std::sync::Arc<Semaphore>> = OnceLock::new();

async fn acquire_global_upload_permit() -> OwnedSemaphorePermit {
    GLOBAL_UPLOAD_SEMAPHORE
        .get_or_init(|| std::sync::Arc::new(Semaphore::new(1)))
        .clone()
        .acquire_owned()
        .await
        .expect("global upload semaphore should not be closed")
}
```

Wrap `upload_single_file` body at call sites instead of inside the function. In `pipeline_upload_videos`, before calling `upload_single_file`:

```rust
let _permit = acquire_global_upload_permit().await;
match upload_single_file(&upload_path, upload_context).await {
```

- [ ] **Step 5: Add line mapping helper**

In `missing_segment.rs`, add:

```rust
use crate::UploadLine;

pub fn upload_line_for_recovery(index: i64) -> Option<UploadLine> {
    match FALLBACK_LINES[index.rem_euclid(FALLBACK_LINES.len() as i64) as usize] {
        RecoveryUploadLine::Bda2 => Some(UploadLine::Bda2),
        RecoveryUploadLine::Tx => Some(UploadLine::Tx),
        RecoveryUploadLine::Bldsa => Some(UploadLine::Bldsa),
        RecoveryUploadLine::Auto => None,
    }
}
```

- [ ] **Step 6: Add DB query for due rows**

In `missing_segment.rs`, add:

```rust
use ormlite::Model;

pub async fn due_missing_segments_for_session(
    pool: &ConnectionPool,
    upload_session_id: i64,
    now: DateTime<Utc>,
) -> AppResult<Vec<UploadMissingSegment>> {
    UploadMissingSegment::select()
        .where_("upload_session_id = ? AND status IN ('pending', 'failed') AND next_retry_at <= ?")
        .bind(upload_session_id)
        .bind(now)
        .order_by("segment_order ASC, id ASC")
        .fetch_all(pool)
        .await
        .change_context(AppError::Unknown)
}
```

- [ ] **Step 7: Add recovery upload helper**

In `upload.rs`, add imports:

```rust
use crate::server::common::missing_segment::{
    due_missing_segments_for_session, mark_retry_failure, mark_retry_success, upload_line_for_recovery,
};
use crate::server::common::upload_session::insert_session_video_at_order;
use ormlite::Model;
```

Add helper after `upload_single_file`:

```rust
async fn recover_due_missing_segments(
    upload_context: &UploadContext,
    ctx: &Context,
    session_row_id: i64,
    archive: &mut LiveArchive,
) -> AppResult<()> {
    let now = chrono::Utc::now();
    let rows = due_missing_segments_for_session(ctx.pool(), session_row_id, now).await?;
    for mut row in rows {
        row.status = "uploading".to_string();
        row.updated_at = chrono::Utc::now();
        row.update_all_fields(ctx.pool()).await.change_context(AppError::Unknown)?;

        let selected_line = upload_line_for_recovery(row.line_index);
        let recovery_context = if let Some(line) = selected_line {
            UploadContext {
                bilibili: upload_context.bilibili.clone(),
                line: get_upload_line(&upload_context.client.client, &format!("{:?}", line).to_lowercase()).await?,
                threads: upload_context.threads,
                client: upload_context.client.clone(),
            }
        } else {
            UploadContext {
                bilibili: upload_context.bilibili.clone(),
                line: Probe::probe(&upload_context.client.client).await.unwrap_or_default(),
                threads: upload_context.threads,
                client: upload_context.client.clone(),
            }
        };

        let path = PathBuf::from(&row.file_path);
        let result = {
            let _permit = acquire_global_upload_permit().await;
            upload_single_file(&path, &recovery_context).await
        };

        match result {
            Ok(video) => {
                let updated = insert_session_video_at_order(ctx.pool(), session_row_id, video, row.segment_order).await?;
                archive.videos = updated;
                mark_retry_success(&mut row, chrono::Utc::now());
                row.update_all_fields(ctx.pool()).await.change_context(AppError::Unknown)?;
                if let Err(e) = execute_postprocessor(vec![path], ctx).await {
                    error!(row_id = row.id, "postprocessor failed after missing segment recovery: {:?}", e);
                }
            }
            Err(e) => {
                mark_retry_failure(&mut row, format!("{e:?}"), chrono::Utc::now());
                row.update_all_fields(ctx.pool()).await.change_context(AppError::Unknown)?;
            }
        }
    }
    Ok(())
}
```

Important implementation note for Step 7: `BiliBili` must be cloneable for this exact code. If it is not `Clone`, change `UploadContext.bilibili` to `std::sync::Arc<BiliBili>` and update existing uses by dereferencing `&upload_context.bilibili`. Keep that change local to `upload.rs`.

- [ ] **Step 8: Call silent recovery before final submit**

In `process_with_upload`, before `submit_session(...)` inside the `if let Some(archive)` block, make `archive` mutable:

```rust
if let Some(mut archive) = archive
```

Then before `submit_session`:

```rust
if let Err(e) = recover_due_missing_segments(&upload_context, ctx, row_id, &mut archive).await {
    error!(?e, row_id, "静默补传缺失分段失败，继续提交已成功分段");
}
```

- [ ] **Step 9: Verify targeted tests pass**

Run:

```bash
cargo test -p biliup-cli missing_segment upload_session -- --nocapture
```

Expected: missing segment and upload session tests pass.

- [ ] **Step 10: Commit**

```bash
git add crates/biliup-cli/src/server/common/missing_segment.rs crates/biliup-cli/src/server/common/upload.rs
git commit -m "feat: silently recover missing segments before submit"
```

---

### Task 6: Manual Recovery Edits Existing Archive At Correct Part Position

**Files:**
- Modify: `crates/biliup-cli/src/server/common/missing_segment.rs`
- Modify: `crates/biliup-cli/src/server/common/upload.rs`
- Modify: `crates/biliup-cli/src/server/api/endpoints.rs`
- Modify: `crates/biliup-cli/src/server/router.rs`
- Test: `crates/biliup-cli/src/server/common/missing_segment.rs`

**Interfaces:**
- Consumes: `bilibili.studio_data(&Vid::Aid(aid), None)` and `bilibili.edit_by_app(&studio, None)`
- Produces: `pub fn patch_studio_videos(studio: &mut Studio, video: Video, segment_order: i64)`
- Produces: `pub async fn manual_recover_missing_segment(...) -> AppResult<()>`
- Produces: `POST /v1/uploads/missing/{id}/recover`
- Produces: `GET /v1/uploads/missing`

- [ ] **Step 1: Write failing test for studio patch ordering**

In `missing_segment.rs`, add imports:

```rust
use biliup::bilibili::Studio;
```

Add placeholder:

```rust
pub fn patch_studio_videos(_studio: &mut Studio, _video: Video, _segment_order: i64) {
    unimplemented!("implemented in Task 6 Step 3")
}
```

Append test:

```rust
#[test]
fn patch_studio_inserts_video_at_recorded_order() {
    let mut studio = Studio::builder()
        .title("archive".to_string())
        .videos(vec![video("p1"), video("p2"), video("p4")])
        .build();

    patch_studio_videos(&mut studio, video("p3"), 2);

    let names: Vec<_> = studio.videos.into_iter().map(|v| v.filename).collect();
    assert_eq!(names, vec!["p1", "p2", "p3", "p4"]);
}
```

- [ ] **Step 2: Run test to verify failure**

Run:

```bash
cargo test -p biliup-cli patch_studio_inserts_video_at_recorded_order -- --nocapture
```

Expected: fails with `implemented in Task 6 Step 3`.

- [ ] **Step 3: Implement studio patch helper**

Replace placeholder:

```rust
pub fn patch_studio_videos(studio: &mut Studio, video: Video, segment_order: i64) {
    insert_video_at_order(&mut studio.videos, video, segment_order);
}
```

- [ ] **Step 4: Add manual recovery function**

In `upload.rs`, add import:

```rust
use biliup::bilibili::Vid;
use crate::server::common::missing_segment::patch_studio_videos;
```

Add function:

```rust
pub async fn manual_recover_missing_segment(
    config: &Config,
    pool: &ConnectionPool,
    missing_id: i64,
) -> AppResult<()> {
    let mut row = UploadMissingSegment::select()
        .where_("id = ?")
        .bind(missing_id)
        .fetch_one(pool)
        .await
        .change_context(AppError::Unknown)?;

    let streamer_info = get_streamer_info(pool, row.streamer_info_id).await?;
    let upload_config = crate::server::infrastructure::models::upload_streamer::UploadStreamer::select()
        .where_("id = (SELECT upload_streamers_id FROM livestreamers WHERE id = ?)")
        .bind(row.live_streamer_id)
        .fetch_one(pool)
        .await
        .change_context(AppError::Unknown)?;

    let upload_context = initialize_upload_context(config, &StatelessClient::default(), &upload_config).await?;
    let path = PathBuf::from(&row.file_path);
    let video = {
        let _permit = acquire_global_upload_permit().await;
        upload_single_file(&path, &upload_context).await?
    };

    if let Some(session_id) = row.upload_session_id {
        let session = crate::server::infrastructure::models::UploadSession::select()
            .where_("id = ?")
            .bind(session_id)
            .fetch_one(pool)
            .await
            .change_context(AppError::Unknown)?;
        if let Some(aid) = session.aid.or(row.aid) {
            let bilibili = &upload_context.bilibili;
            let mut studio = bilibili
                .studio_data(&Vid::Aid(aid as u64), None)
                .await
                .change_context(AppError::Unknown)?;
            patch_studio_videos(&mut studio, video.clone(), row.segment_order);
            bilibili
                .edit_by_app(&studio, None)
                .await
                .change_context(AppError::Unknown)?;
        } else {
            insert_session_video_at_order(pool, session_id, video.clone(), row.segment_order).await?;
        }
    } else if let Some(aid) = row.aid {
        let bilibili = &upload_context.bilibili;
        let mut studio = bilibili
            .studio_data(&Vid::Aid(aid as u64), None)
            .await
            .change_context(AppError::Unknown)?;
        patch_studio_videos(&mut studio, video.clone(), row.segment_order);
        bilibili
            .edit_by_app(&studio, None)
            .await
            .change_context(AppError::Unknown)?;
    } else {
        return Err(AppError::Custom("missing segment has neither upload_session_id nor aid".to_string()).into());
    }

    mark_retry_success(&mut row, chrono::Utc::now());
    row.aid = row.aid.or_else(|| row.upload_session_id.and_then(|_| None));
    row.update_all_fields(pool).await.change_context(AppError::Unknown)?;

    Ok(())
}
```

Implementation note: the `streamer_info` local is read to prove the row still points at valid stream metadata. If Rust warns about unused variable, replace it with `_streamer_info`.

- [ ] **Step 5: Add list and manual recover endpoints**

In `endpoints.rs`, add imports:

```rust
use crate::server::common::upload::manual_recover_missing_segment;
use crate::server::infrastructure::models::UploadMissingSegment;
use ormlite::Model;
```

Add endpoint functions:

```rust
pub async fn get_missing_uploads(
    State(service_register): State<ServiceRegister>,
) -> Result<Json<Vec<UploadMissingSegment>>, Response> {
    let rows = UploadMissingSegment::select()
        .where_("status IN ('pending', 'failed', 'uploading')")
        .order_by("created_at DESC")
        .fetch_all(&service_register.connection_pool)
        .await
        .map_err(AppError::from)?;
    Ok(Json(rows))
}

pub async fn recover_missing_upload(
    State(service_register): State<ServiceRegister>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Json<serde_json::Value>, Response> {
    let config = service_register.config.read().unwrap().clone();
    manual_recover_missing_segment(&config, &service_register.connection_pool, id)
        .await
        .map_err(AppError::from)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}
```

Adjust field access names if `ServiceRegister` uses accessor methods rather than public fields. Check `crates/biliup-cli/src/server/infrastructure/service_register.rs` and use the exact public API.

- [ ] **Step 6: Wire routes**

In `router.rs`, add endpoint imports:

```rust
get_missing_uploads, recover_missing_upload,
```

Add routes before `/v1/uploads`:

```rust
.route("/v1/uploads/missing", get(get_missing_uploads))
.route("/v1/uploads/missing/{id}/recover", post(recover_missing_upload))
```

- [ ] **Step 7: Verify tests pass**

Run:

```bash
cargo test -p biliup-cli missing_segment -- --nocapture
```

Expected: missing segment tests pass.

- [ ] **Step 8: Commit**

```bash
git add crates/biliup-cli/src/server/common/missing_segment.rs crates/biliup-cli/src/server/common/upload.rs crates/biliup-cli/src/server/api/endpoints.rs crates/biliup-cli/src/server/router.rs
git commit -m "feat: manually patch missing segments into archives"
```

---

### Task 7: Final Verification And Formatting

**Files:**
- Modify only files touched by prior tasks if formatting requires it.

**Interfaces:**
- Consumes all prior tasks.
- Produces verified implementation.

- [ ] **Step 1: Format Rust code**

Run:

```bash
cargo fmt
```

Expected: exits 0.

- [ ] **Step 2: Run targeted tests**

Run:

```bash
cargo test -p biliup-cli missing_segment upload_session -- --nocapture
```

Expected: tests pass.

- [ ] **Step 3: Run broader crate tests**

Run:

```bash
cargo test -p biliup-cli
```

Expected: tests pass. If unrelated pre-existing tests fail, capture exact failures and stop for review.

- [ ] **Step 4: Inspect git status**

Run:

```bash
git status --short
```

Expected: only files from this plan plus the pre-existing unrelated `BUILD_AND_DEPLOY.md` show changes. Do not revert or stage `BUILD_AND_DEPLOY.md`.

- [ ] **Step 5: Commit final formatting if needed**

If `cargo fmt` changed files after the previous task commits:

```bash
git add crates/biliup-cli

git commit -m "chore: format missing segment recovery"
```

Do not add `BUILD_AND_DEPLOY.md`.

---

## Self-Review

- Spec coverage: The plan covers durable missing segment registration, silent serialized retry before final submit, upload-line rotation, manual recovery into existing archives, correct part insertion by `segment_order`, and preserving local files until durable success.
- Placeholder scan: The plan intentionally uses temporary `unimplemented!` only in failing-test steps, each with a matching implementation step in the same task. No open-ended implementation placeholders remain.
- Type consistency: `UploadMissingSegment`, `RecoveryUploadLine`, `insert_video_at_order`, `insert_session_video_at_order`, `patch_studio_videos`, and endpoint names are consistently referenced. The plan calls out places where exact public fields may need to be checked before implementation.
