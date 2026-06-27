# Missing Upload Recovery Controls Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add safe missing-upload deletion, retry for stuck `uploading` rows, stricter automatic upload-line probing, and extra small-segment diagnostics.

**Architecture:** Keep the existing missing-upload table and synchronous recovery flow. Add small helpers around row state transitions and file cleanup, expose two focused API endpoints, and update the existing Missing Recovery page controls. Change upload-line probing so probe failures are observable and no longer fall back silently to the default line.

**Tech Stack:** Rust 2024, axum, sqlx/ormlite, tokio, reqwest, Next.js 14, React 18, SWR, Semi UI.

---

## File Structure

- Modify `crates/biliup-cli/src/server/common/missing_segment.rs`
  - Add pure helpers for delete eligibility, retry reset, and idempotent local-file cleanup.
  - Add unit tests for state transitions and file cleanup.
- Modify `crates/biliup-cli/src/server/common/upload.rs`
  - Add `retry_missing_segment` wrapper for `uploading` rows.
  - Reuse existing `manual_recover_missing_segment`.
- Modify `crates/biliup-cli/src/server/api/endpoints.rs`
  - Add `delete_missing_upload` and `retry_missing_upload` handlers.
- Modify `crates/biliup-cli/src/server/router.rs`
  - Add `DELETE /v1/uploads/missing/{id}` and `POST /v1/uploads/missing/{id}/retry`.
- Modify `crates/biliup/src/uploader/line.rs`
  - Make `Probe::probe` skip failed lines, log each result, and fail when every line fails.
  - Add testable selection helpers.
- Modify `crates/biliup-cli/src/server/common/upload.rs`
  - Stop using `Probe::probe(...).unwrap_or_default()` in automatic server upload selection.
- Modify `crates/biliup-cli/src/uploader.rs`
  - Stop using `Probe::probe(...).unwrap_or_default()` in CLI upload selection.
- Modify `crates/biliup/src/downloader/httpflv.rs`, `crates/biliup-cli/src/server/core/downloader/stream_gears.rs`, and `crates/biliup-cli/src/server/common/download.rs`
  - Add diagnostic log fields for small-segment investigation without changing control flow.
- Modify `app/(app)/missing/page.tsx`
  - Add delete and retry controls.
  - Preserve existing user-local styling changes in that file.

---

### Task 1: Missing Segment State Helpers

**Files:**
- Modify: `crates/biliup-cli/src/server/common/missing_segment.rs`

- [ ] **Step 1: Write failing tests for delete and retry state rules**

Append these tests inside the existing `#[cfg(test)] mod tests` in `missing_segment.rs`:

```rust
#[test]
fn delete_is_allowed_only_for_unrecovered_rows() {
    assert!(can_delete_missing_segment("pending"));
    assert!(can_delete_missing_segment("failed"));
    assert!(!can_delete_missing_segment("uploading"));
    assert!(!can_delete_missing_segment("succeeded"));
}

#[test]
fn retry_reset_turns_uploading_into_due_failed_row() {
    let mut row = missing_row();
    row.status = "uploading".to_string();
    row.attempts = 7;
    row.next_retry_at = chrono::Utc.with_ymd_and_hms(2026, 6, 18, 13, 0, 0).unwrap();
    let now = chrono::Utc.with_ymd_and_hms(2026, 6, 18, 12, 5, 0).unwrap();

    reset_for_manual_retry(&mut row, now);

    assert_eq!(row.status, "failed");
    assert_eq!(row.attempts, 7);
    assert_eq!(
        row.last_error.as_deref(),
        Some("manual retry requested from uploading state")
    );
    assert_eq!(row.next_retry_at, now);
    assert_eq!(row.updated_at, now);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:

```bash
cargo test -p biliup-cli missing_segment -- --nocapture
```

Expected: FAIL with missing functions `can_delete_missing_segment` and `reset_for_manual_retry`.

- [ ] **Step 3: Implement the state helpers**

Add near the other pure helpers in `missing_segment.rs`:

```rust
pub fn can_delete_missing_segment(status: &str) -> bool {
    matches!(status, "pending" | "failed")
}

pub fn reset_for_manual_retry(row: &mut UploadMissingSegment, now: DateTime<Utc>) {
    row.status = "failed".to_string();
    row.last_error = Some("manual retry requested from uploading state".to_string());
    row.next_retry_at = now;
    row.updated_at = now;
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run:

```bash
cargo test -p biliup-cli missing_segment -- --nocapture
```

Expected: PASS for the existing missing-segment tests and the two new tests.

- [ ] **Step 5: Commit**

```bash
git add crates/biliup-cli/src/server/common/missing_segment.rs
git commit -m "feat: add missing upload state helpers"
```

---

### Task 2: Delete Missing Upload Backend

**Files:**
- Modify: `crates/biliup-cli/src/server/common/missing_segment.rs`
- Modify: `crates/biliup-cli/src/server/api/endpoints.rs`
- Modify: `crates/biliup-cli/src/server/router.rs`

- [ ] **Step 1: Write failing tests for local file cleanup**

Add async tests inside the existing `#[cfg(test)] mod tests` in `missing_segment.rs`:

```rust
#[tokio::test]
async fn remove_missing_segment_files_deletes_video_and_danmaku() {
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("part.flv");
    let danmaku = dir.path().join("part.xml");
    tokio::fs::write(&video, b"video").await.unwrap();
    tokio::fs::write(&danmaku, b"danmaku").await.unwrap();

    remove_missing_segment_files(&video, Some(&danmaku)).await.unwrap();

    assert!(!video.exists());
    assert!(!danmaku.exists());
}

#[tokio::test]
async fn remove_missing_segment_files_ignores_missing_files() {
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("missing.flv");
    let danmaku = dir.path().join("missing.xml");

    remove_missing_segment_files(&video, Some(&danmaku)).await.unwrap();

    assert!(!video.exists());
    assert!(!danmaku.exists());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cargo test -p biliup-cli remove_missing_segment_files -- --nocapture
```

Expected: FAIL with missing function `remove_missing_segment_files`.

- [ ] **Step 3: Implement idempotent cleanup**

Add import at the top of `missing_segment.rs`:

```rust
use tracing::info;
```

Add helper:

```rust
pub async fn remove_missing_segment_files(
    file_path: &Path,
    danmaku_file_path: Option<&Path>,
) -> AppResult<()> {
    async fn remove_one(path: &Path) -> AppResult<()> {
        match tokio::fs::remove_file(path).await {
            Ok(()) => {
                info!(file = %path.display(), "deleted missing upload local file");
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                info!(file = %path.display(), "missing upload local file already absent");
                Ok(())
            }
            Err(e) => Err(e).change_context(AppError::Unknown),
        }
    }

    remove_one(file_path).await?;
    if let Some(path) = danmaku_file_path {
        remove_one(path).await?;
    }
    Ok(())
}
```

- [ ] **Step 4: Run cleanup tests**

Run:

```bash
cargo test -p biliup-cli remove_missing_segment_files -- --nocapture
```

Expected: PASS.

- [ ] **Step 5: Add backend delete handler**

In `endpoints.rs`, extend imports:

```rust
use crate::server::common::missing_segment::{
    can_delete_missing_segment, remove_missing_segment_files,
};
```

Add handler after `recover_missing_upload`:

```rust
pub async fn delete_missing_upload(
    State(service_register): State<ServiceRegister>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Json<serde_json::Value>, Response> {
    let row = UploadMissingSegment::select()
        .where_("id = ?")
        .bind(id)
        .fetch_one(&service_register.pool)
        .await
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;

    if !can_delete_missing_segment(&row.status) {
        return Err((
            StatusCode::CONFLICT,
            format!("missing upload status '{}' cannot be deleted", row.status),
        )
            .into_response());
    }

    let file_path = PathBuf::from(&row.file_path);
    let danmaku_path = row.danmaku_file_path.as_deref().map(PathBuf::from);
    remove_missing_segment_files(&file_path, danmaku_path.as_deref())
        .await
        .map_err(report_to_response)?;

    sqlx::query("DELETE FROM upload_missing_segment WHERE id = ?")
        .bind(id)
        .execute(&service_register.pool)
        .await
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;

    Ok(Json(serde_json::json!({ "ok": true })))
}
```

- [ ] **Step 6: Wire the route**

In `router.rs`, add `delete_missing_upload` to the `use crate::server::api::endpoints::{...}` import list.

Change the existing route:

```rust
.route("/v1/uploads/missing", get(get_missing_uploads))
```

Add directly below it:

```rust
.route(
    "/v1/uploads/missing/{id}",
    delete(delete_missing_upload),
)
```

- [ ] **Step 7: Run backend checks**

Run:

```bash
cargo test -p biliup-cli missing_segment -- --nocapture
cargo check -p biliup-cli
```

Expected: both PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/biliup-cli/src/server/common/missing_segment.rs crates/biliup-cli/src/server/api/endpoints.rs crates/biliup-cli/src/server/router.rs
git commit -m "feat: delete missing upload records"
```

---

### Task 3: Retry Stuck Uploading Rows

**Files:**
- Modify: `crates/biliup-cli/src/server/common/upload.rs`
- Modify: `crates/biliup-cli/src/server/api/endpoints.rs`
- Modify: `crates/biliup-cli/src/server/router.rs`

- [ ] **Step 1: Add retry function skeleton and compile-failing endpoint**

In `upload.rs`, add a public wrapper after `manual_recover_missing_segment`:

```rust
pub async fn retry_missing_segment(
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

    if row.status == "succeeded" {
        return Ok(());
    }

    if row.status == "uploading" {
        let now = chrono::Utc::now();
        reset_for_manual_retry(&mut row, now);
        row.update_all_fields(pool)
            .await
            .change_context(AppError::Unknown)?;
    }

    manual_recover_missing_segment(config, pool, missing_id).await
}
```

This should not compile yet because `reset_for_manual_retry` is not imported.

- [ ] **Step 2: Run cargo check to verify expected failure**

Run:

```bash
cargo check -p biliup-cli
```

Expected: FAIL with missing import `reset_for_manual_retry`.

- [ ] **Step 3: Import the helper and add endpoint**

In `upload.rs`, extend the existing `missing_segment` import:

```rust
use crate::server::common::missing_segment::{
    due_missing_segments_for_session, enqueue_missing_segment, mark_retry_failure,
    mark_retry_success, next_segment_order, patch_studio_videos, reset_for_manual_retry,
    upload_line_for_recovery,
};
```

In `endpoints.rs`, extend the upload import:

```rust
use crate::server::common::upload::{
    build_studio, manual_recover_missing_segment, retry_missing_segment, submit_to_bilibili,
    upload,
};
```

Add handler after `recover_missing_upload`:

```rust
pub async fn retry_missing_upload(
    State(service_register): State<ServiceRegister>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Json<serde_json::Value>, Response> {
    let config = service_register.config.read().unwrap().clone();
    retry_missing_segment(&config, &service_register.pool, id)
        .await
        .change_context(AppError::Unknown)
        .map_err(report_to_response)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}
```

In `router.rs`, import `retry_missing_upload` and add:

```rust
.route(
    "/v1/uploads/missing/{id}/retry",
    post(retry_missing_upload),
)
```

- [ ] **Step 4: Run backend checks**

Run:

```bash
cargo check -p biliup-cli
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/biliup-cli/src/server/common/upload.rs crates/biliup-cli/src/server/api/endpoints.rs crates/biliup-cli/src/server/router.rs
git commit -m "feat: retry stuck missing uploads"
```

---

### Task 4: Strict Automatic Upload-Line Probe

**Files:**
- Modify: `crates/biliup/src/uploader/line.rs`
- Modify: `crates/biliup-cli/src/server/common/upload.rs`
- Modify: `crates/biliup-cli/src/uploader.rs`

- [ ] **Step 1: Write failing pure tests for line selection**

In `crates/biliup/src/uploader/line.rs`, add this test module at the end of the file:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn line_with_cost(query: &str, cost: u128) -> Line {
        Line {
            os: Uploader::Upos,
            probe_url: format!("//{query}.example.com/OK"),
            query: query.to_string(),
            cost,
        }
    }

    #[test]
    fn choose_fastest_successful_line_ignores_failures() {
        let candidates = vec![
            (line_with_cost("slow", 300), true),
            (line_with_cost("down", 10), false),
            (line_with_cost("fast", 20), true),
        ];

        let selected = choose_fastest_successful_line(candidates).unwrap();

        assert_eq!(selected.query, "fast");
        assert_eq!(selected.cost, 20);
    }

    #[test]
    fn choose_fastest_successful_line_fails_when_all_fail() {
        let candidates = vec![
            (line_with_cost("down-1", 10), false),
            (line_with_cost("down-2", 20), false),
        ];

        let err = choose_fastest_successful_line(candidates).unwrap_err();

        assert!(err.to_string().contains("no upload line probe succeeded"));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cargo test -p biliup choose_fastest_successful_line -- --nocapture
```

Expected: FAIL with missing function `choose_fastest_successful_line`.

- [ ] **Step 3: Add selection helper and stricter probe**

In `line.rs`, change tracing import:

```rust
use tracing::{info, warn};
```

Add helper above `impl Probe`:

```rust
pub fn choose_fastest_successful_line<I>(candidates: I) -> Result<Line>
where
    I: IntoIterator<Item = (Line, bool)>,
{
    candidates
        .into_iter()
        .filter_map(|(line, ok)| ok.then_some(line))
        .min_by_key(|line| line.cost)
        .ok_or_else(|| Custom("no upload line probe succeeded".to_string()))
}
```

Replace `Probe::probe` with:

```rust
pub async fn probe(client: &reqwest::Client) -> Result<Line> {
    let res: Self = client
        .get("https://member.bilibili.com/preupload?r=probe")
        .send()
        .await?
        .json()
        .await?;

    let mut candidates = Vec::new();
    for mut line in res.lines {
        let url = format!("https:{}", line.probe_url);
        let instant = Instant::now();
        let ping_result = Probe::ping(&res.probe, &url, client).send().await;
        match ping_result {
            Ok(resp) if resp.status().is_success() => {
                line.cost = instant.elapsed().as_millis();
                info!(query = %line.query, cost = line.cost, "upload line probe succeeded");
                candidates.push((line, true));
            }
            Ok(resp) => {
                let status = resp.status();
                warn!(query = %line.query, %status, "upload line probe returned non-success status");
                candidates.push((line, false));
            }
            Err(err) => {
                warn!(query = %line.query, error = %err, "upload line probe failed");
                candidates.push((line, false));
            }
        }
    }

    choose_fastest_successful_line(candidates)
}
```

- [ ] **Step 4: Replace silent fallbacks**

In `crates/biliup-cli/src/server/common/upload.rs`, change `get_upload_line` default branch:

```rust
_ => Probe::probe(client).await.change_context(AppError::Unknown)?,
```

In the same file, inside `recover_due_missing_segments`, change:

```rust
line: Probe::probe(&upload_context.client.client)
    .await
    .change_context(AppError::Unknown)?,
```

In `crates/biliup-cli/src/uploader.rs`, replace both occurrences of:

```rust
_ => Probe::probe(&client.client).await.unwrap_or_default(),
```

with:

```rust
_ => Probe::probe(&client.client).await?,
```

- [ ] **Step 5: Run probe and CLI checks**

Run:

```bash
cargo test -p biliup choose_fastest_successful_line -- --nocapture
cargo check -p biliup-cli
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/biliup/src/uploader/line.rs crates/biliup-cli/src/server/common/upload.rs crates/biliup-cli/src/uploader.rs
git commit -m "fix: fail automatic line selection when probes fail"
```

---

### Task 5: Missing Recovery Frontend Controls

**Files:**
- Modify: `app/(app)/missing/page.tsx`
- Modify: `app/lib/api-streamer.ts` only if the existing `requestDelete` helper does not fit the call shape.

- [ ] **Step 1: Add API actions and state**

In `app/(app)/missing/page.tsx`, change imports:

```tsx
import { IconDeleteStroked, IconRefresh, IconSendStroked } from '@douyinfe/semi-icons'
import { fetcher, requestDelete, sendRequest } from '../../lib/api-streamer'
```

Add state next to `recoveringId`:

```tsx
const [retryingId, setRetryingId] = useState<number | null>(null)
const [deletingId, setDeletingId] = useState<number | null>(null)
```

Add handlers after `handleRecover`:

```tsx
const handleRetry = async (id: number) => {
  setRetryingId(id)
  try {
    await sendRequest(`/v1/uploads/missing/${id}/retry`, { arg: {} })
    Toast.success('已重新发起补投')
    await mutate()
  } catch (e: any) {
    Toast.error(`重新补投失败：${e?.message ?? e}`)
  } finally {
    setRetryingId(null)
  }
}

const handleDelete = async (id: number) => {
  setDeletingId(id)
  try {
    await requestDelete('/v1/uploads/missing', { arg: id })
    Toast.success('已删除缺失记录和本地文件')
    await mutate()
  } catch (e: any) {
    Toast.error(`删除失败：${e?.message ?? e}`)
  } finally {
    setDeletingId(null)
  }
}
```

- [ ] **Step 2: Replace operation-column render**

Replace the operation column `render` body with:

```tsx
render: (_: unknown, record: MissingSegment) => {
  if (record.status === 'succeeded') return '—'

  if (record.status === 'uploading') {
    return (
      <Popconfirm
        title="重新补投这一段？"
        content="将重新上传该分段。旧的卡住请求不一定会被取消，目标是尽快把分 P 补进 B 站。"
        okText="重新补投"
        onConfirm={() => handleRetry(record.id)}
      >
        <Button
          theme="borderless"
          icon={<IconSendStroked />}
          loading={retryingId === record.id}
        >
          重新补投
        </Button>
      </Popconfirm>
    )
  }

  return (
    <div style={{ display: 'flex', gap: 4 }}>
      <Popconfirm
        title="补传这一段？"
        content="将重新上传该分段，并按原分 P 位置补进对应稿件（已投稿）或待提交会话。"
        okText="补传"
        onConfirm={() => handleRecover(record.id)}
      >
        <Button
          theme="borderless"
          icon={<IconSendStroked />}
          loading={recoveringId === record.id}
        >
          补传
        </Button>
      </Popconfirm>
      <Popconfirm
        title="删除这条缺失记录？"
        content="将删除缺失补传记录，并同时删除对应本地视频文件和弹幕文件。此操作不会补投到 B 站。"
        okText="删除"
        okButtonProps={{ type: 'danger' }}
        onConfirm={() => handleDelete(record.id)}
      >
        <Button
          theme="borderless"
          type="danger"
          icon={<IconDeleteStroked />}
          loading={deletingId === record.id}
        />
      </Popconfirm>
    </div>
  )
}
```

- [ ] **Step 3: Build the frontend**

Run:

```bash
pnpm lint
pnpm build
```

Expected: both PASS. If `pnpm lint` fails because `next lint` is removed or unavailable in the installed Next version, record the exact failure and continue with `pnpm build`.

- [ ] **Step 4: Commit**

```bash
git add app/'(app)'/missing/page.tsx
git commit -m "feat: add missing upload retry and delete controls"
```

---

### Task 6: Small Segment Diagnostic Logs

**Files:**
- Modify: `crates/biliup/src/downloader/httpflv.rs`
- Modify: `crates/biliup-cli/src/server/core/downloader/stream_gears.rs`
- Modify: `crates/biliup-cli/src/server/common/download.rs`

- [ ] **Step 1: Add diagnostic logging without changing return types**

In `httpflv.rs`, change the timeout branch inside `Connection::read_frame`:

```rust
match timeout(Duration::from_secs(30), self.resp.chunk()).await {
    Ok(Ok(Some(chunk))) => {
        self.buffer.put(chunk);
    }
    Ok(Ok(None)) => {
        warn!(
            buffered = self.buffer.len(),
            "httpflv chunk stream ended before requested frame was complete"
        );
        return Ok(self.buffer.split().freeze());
    }
    Ok(Err(err)) => {
        warn!(error = %err, buffered = self.buffer.len(), "httpflv chunk read failed");
        return Err(err.into());
    }
    Err(err) => {
        warn!(error = %err, buffered = self.buffer.len(), "httpflv chunk read timed out");
        return Err(err.into());
    }
}
```

This changes timeout from a warning emitted by `parse_flv` to a more specific warning at the read boundary, but it preserves the existing outer behavior of ending this download attempt.

- [ ] **Step 2: Add stream URL host logging**

In `stream_gears.rs`, before `info!("Downloading {}...", url);`, add:

```rust
let stream_host = url::Url::parse(&url)
    .ok()
    .and_then(|u| u.host_str().map(ToString::to_string))
    .unwrap_or_default();
info!(stream_host = %stream_host, "selected stream url host");
```

- [ ] **Step 3: Add status-check timing logs**

In `download.rs`, before `match plugin.check_stream(...)`, add:

```rust
let check_started = std::time::Instant::now();
let check_result = plugin.check_stream(live_request(ctx.worker())).await;
let check_elapsed = check_started.elapsed();
```

Then change the `match` target to:

```rust
match check_result {
```

In the `Live` arm, add `check_elapsed = ?check_elapsed` to the existing `info!`:

```rust
info!(url = url, check_elapsed = ?check_elapsed, "Stream is still live, continuing same session");
```

In the `Offline` arm, add `check_elapsed = ?check_elapsed` to the `info!` calls.

In the `Err(e)` arm, add `check_elapsed = ?check_elapsed` to both warning logs.

- [ ] **Step 4: Run backend checks**

Run:

```bash
cargo check -p biliup-cli
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/biliup/src/downloader/httpflv.rs crates/biliup-cli/src/server/core/downloader/stream_gears.rs crates/biliup-cli/src/server/common/download.rs
git commit -m "chore: add small segment diagnostics"
```

---

### Task 7: Final Verification

**Files:**
- Verify all modified files from Tasks 1-6.

- [ ] **Step 1: Run Rust tests**

Run:

```bash
cargo test -p biliup-cli missing_segment -- --nocapture
cargo test -p biliup choose_fastest_successful_line -- --nocapture
cargo check -p biliup-cli
```

Expected: all PASS.

- [ ] **Step 2: Run frontend verification**

Run:

```bash
pnpm build
```

Expected: PASS.

- [ ] **Step 3: Inspect git status**

Run:

```bash
git status --short
```

Expected: no unintended unstaged files except any user-owned changes that existed before implementation.

- [ ] **Step 4: Summarize remaining operational notes**

Include in the final response:

- `uploading` rows can now be retried.
- `pending/failed` rows can be deleted with local files.
- automatic line selection fails clearly when all probes fail.
- small-segment behavior is unchanged; logs are richer for the next incident.
