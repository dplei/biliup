//! Read-only query, live continuation and export for the independent event store.
//!
//! This is a second, independent reader: it never touches the business database and never writes
//! to the event store. The old `/v1/ws/logs` file stream stays exactly as it was, so the two
//! sources remain independent evidence during the migration.

use crate::server::errors::{AppError, report_to_response};
use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query as UrlQuery};
use axum::http::{StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use biliup_observability::sqlite::{Query, Repository, StoredEvent};
use biliup_observability::{CaptureKind, Level};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::Mutex;

/// Highest page size any caller can ask for. Bigger requests are clamped, never rejected, so a
/// client that guesses wrong still gets a usable answer.
const MAX_LIMIT: usize = 200;
const DEFAULT_LIMIT: usize = 50;
/// One export is a bounded read, not a backup: it stops and says so rather than pinning WAL.
const EXPORT_MAX_ROWS: usize = 20_000;
const STREAM_POLL: Duration = Duration::from_millis(500);

/// Why the store cannot answer, in the caller's terms. An empty list from a disabled capture is
/// not the same as "nothing happened", so the two are never returned the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Ready,
    Disabled,
    Unavailable,
}

struct Store {
    availability: Availability,
    path: Option<PathBuf>,
    repository: Option<Repository>,
    error: Option<String>,
}

/// One process-wide read-only handle. Opening is lazy because capture is opt-in: a deployment
/// that never turned it on must not pay for a pool, and must still get a clear answer.
async fn store() -> &'static Mutex<Store> {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    let store = STORE.get_or_init(|| {
        Mutex::new(Store {
            availability: Availability::Disabled,
            path: None,
            repository: None,
            error: None,
        })
    });
    let mut guard = store.lock().await;
    if guard.repository.is_some() {
        return store;
    }
    let enabled = matches!(std::env::var("BILIUP_OBSERVABILITY").as_deref(), Ok("1"));
    let path = std::env::var_os("BILIUP_OBSERVABILITY_DB")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    match (enabled, path) {
        (false, _) | (_, None) => {
            guard.availability = Availability::Disabled;
            guard.error = Some("capture is not enabled for this process".to_string());
        }
        (true, Some(path)) => match Repository::open(&path).await {
            Ok(repository) => {
                guard.availability = Availability::Ready;
                guard.repository = Some(repository);
                guard.path = Some(path);
                guard.error = None;
            }
            Err(error) => {
                guard.availability = Availability::Unavailable;
                guard.error = Some(error.to_string());
                guard.path = Some(path);
            }
        },
    }
    drop(guard);
    store
}

#[derive(Debug, Default, Deserialize)]
pub struct ListParams {
    after_id: Option<u64>,
    until_id: Option<u64>,
    since_ms: Option<i64>,
    until_ms: Option<i64>,
    min_level: Option<String>,
    /// Exact levels, comma separated (`INFO,WARN`). Unlike `min_level` this can express "only
    /// warnings and errors" without dragging every INFO along.
    levels: Option<String>,
    category: Option<String>,
    /// Exact categories, comma separated (`recording,upload`).
    categories: Option<String>,
    event_name: Option<String>,
    instance_id: Option<String>,
    /// One allowlisted correlation field, e.g. `segment_id`; requires `instance_id`.
    assoc_key: Option<String>,
    assoc_value: Option<String>,
    /// `native` (default), `legacy_bridge`, or `all`. Bridge diagnostics are never mixed in
    /// unless the caller says so.
    capture_kind: Option<String>,
    keyword: Option<String>,
    limit: Option<usize>,
    /// `asc` (default) reads forward from `after_id`; `desc` answers "the newest first", which is
    /// what a reader opening the page wants. Live continuation and export are always ascending.
    order: Option<String>,
    format: Option<String>,
}

/// A malformed filter is the caller's mistake, not a server failure: 400, and the message names
/// the parameter without echoing anything else back.
fn bad_request(message: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"message": message})),
    )
        .into_response()
}

fn level(value: &str) -> Option<Level> {
    match value.to_ascii_uppercase().as_str() {
        "TRACE" => Some(Level::Trace),
        "DEBUG" => Some(Level::Debug),
        "INFO" => Some(Level::Info),
        "WARN" => Some(Level::Warn),
        "ERROR" => Some(Level::Error),
        _ => None,
    }
}

fn kind(value: Option<&str>) -> Result<Option<CaptureKind>, Response> {
    match value {
        None | Some("native") => Ok(Some(CaptureKind::Native)),
        Some("legacy_bridge") => Ok(Some(CaptureKind::LegacyBridge)),
        Some("all") => Ok(None),
        Some(other) => Err(bad_request(format!("unknown capture_kind {other}"))),
    }
}

/// A comma separated set, bounded and de-duplicated. An empty element is the caller's mistake,
/// not silently dropped: a stray comma would otherwise widen the filter without saying so.
fn set(value: Option<&str>, parameter: &str) -> Result<Vec<String>, Response> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for part in value.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(bad_request(format!("empty value in {parameter}")));
        }
        if !out.iter().any(|kept: &String| kept == part) {
            out.push(part.to_string());
        }
    }
    Ok(out)
}

impl ListParams {
    fn newest_first(&self) -> Result<bool, Response> {
        match self.order.as_deref() {
            None | Some("asc") => Ok(false),
            Some("desc") => Ok(true),
            Some(other) => Err(bad_request(format!("unknown order {other}"))),
        }
    }

    fn to_query(&self, limit: usize, newest_first: bool) -> Result<Query, Response> {
        let association = match (&self.assoc_key, &self.assoc_value) {
            (Some(key), Some(value)) => Some((key.clone(), value.clone())),
            (None, None) => None,
            _ => {
                return Err(bad_request(
                    "assoc_key and assoc_value must be given together".to_string(),
                ));
            }
        };
        let min_level = match &self.min_level {
            Some(value) => {
                Some(level(value).ok_or_else(|| bad_request(format!("unknown level {value}")))?)
            }
            None => None,
        };
        let mut levels = Vec::new();
        for value in set(self.levels.as_deref(), "levels")? {
            levels
                .push(level(&value).ok_or_else(|| bad_request(format!("unknown level {value}")))?);
        }
        let categories = set(self.categories.as_deref(), "categories")?;
        Ok(Query {
            after_id: self.after_id.unwrap_or(0),
            until_id: self.until_id,
            since_ms: self.since_ms,
            until_ms: self.until_ms,
            min_level,
            levels,
            category: self.category.clone(),
            categories,
            event_name: self.event_name.clone(),
            instance_id: self.instance_id.clone(),
            association,
            capture_kind: kind(self.capture_kind.as_deref())?,
            keyword: self.keyword.clone(),
            newest_first,
            limit,
        })
    }
}

#[derive(Serialize)]
pub struct ListResponse {
    version: &'static str,
    availability: Availability,
    /// Which kinds this answer covers. A caller reading only native events is told so, so an
    /// empty list is never mistaken for "no problems anywhere".
    coverage: &'static str,
    events: Vec<StoredEvent>,
    /// Cursor for the next page when reading forward; absent when this page reached the end.
    next_after_id: Option<u64>,
    /// Cursor for the next (older) page when reading newest-first; pass it back as `until_id`.
    next_until_id: Option<u64>,
    /// Rows matching the whole filtered range, not just this page.
    total: u64,
    /// Events deleted by retention below this id; a cursor older than it cannot be continued.
    pruned_through: u64,
    /// The requested cursor fell inside the pruned range: this answer has a hole in it.
    gap: bool,
    unclean_shutdowns: u64,
    health: serde_json::Value,
    error: Option<String>,
}

fn unavailable(availability: Availability, error: Option<String>) -> ListResponse {
    ListResponse {
        version: "log-events-v1",
        availability,
        coverage: "none",
        events: Vec::new(),
        next_after_id: None,
        next_until_id: None,
        total: 0,
        pruned_through: 0,
        gap: false,
        unclean_shutdowns: 0,
        health: biliup_observability::shadow::health_snapshot(),
        error,
    }
}

pub async fn list_log_events(
    UrlQuery(params): UrlQuery<ListParams>,
) -> Result<Json<ListResponse>, Response> {
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let query = params.to_query(limit, params.newest_first()?)?;
    let coverage = match query.capture_kind {
        Some(CaptureKind::Native) => "native",
        Some(CaptureKind::LegacyBridge) => "legacy_bridge",
        None => "native_and_legacy_bridge",
    };
    let guard = store().await.lock().await;
    let Some(repository) = guard.repository.as_ref() else {
        return Ok(Json(unavailable(guard.availability, guard.error.clone())));
    };
    let page = repository
        .query(&query)
        .await
        .map_err(|error| report_to_response(AppError::Custom(error.to_string())))?;
    let total = repository
        .count(&query)
        .await
        .map_err(|error| report_to_response(AppError::Custom(error.to_string())))?;
    // A full page means there may be more; which end to continue from depends on the direction.
    // Reading newest-first, the oldest row on this page is the largest id the next page may hold.
    let full = page.events.len() == limit;
    let (next_after_id, next_until_id) = match (full, query.newest_first) {
        (false, _) => (None, None),
        (true, false) => (page.events.last().map(|event| event.id), None),
        (true, true) => (
            None,
            page.events
                .last()
                .and_then(|event| event.id.checked_sub(1))
                .filter(|id| *id > 0),
        ),
    };
    Ok(Json(ListResponse {
        version: "log-events-v1",
        availability: Availability::Ready,
        coverage,
        next_after_id,
        next_until_id,
        total,
        pruned_through: page.pruned_through,
        gap: page.gap,
        unclean_shutdowns: page.unclean_shutdowns,
        health: biliup_observability::shadow::health_snapshot(),
        error: None,
        events: page.events,
    }))
}

pub async fn get_log_event_diagnostic(Path(event_uid): Path<String>) -> Result<Response, Response> {
    let guard = store().await.lock().await;
    let Some(repository) = guard.repository.as_ref() else {
        return Ok((StatusCode::SERVICE_UNAVAILABLE, "event store unavailable").into_response());
    };
    match repository
        .diagnostic(&event_uid)
        .await
        .map_err(|error| report_to_response(AppError::Custom(error.to_string())))?
    {
        Some(payload) => Ok(Json(payload).into_response()),
        None => Ok((StatusCode::NOT_FOUND, "no diagnostic for this event").into_response()),
    }
}

/// Live continuation over already committed events only.
///
/// The client keeps the last id it saw and reconnects with it, so the history query and the
/// subscription share one cursor and cannot race: nothing is published that a list query would
/// not also return. A cursor older than retention is reported as a gap instead of silently
/// skipping the missing range.
pub async fn stream_log_events(
    UrlQuery(params): UrlQuery<ListParams>,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, Response> {
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    // Live continuation only ever moves forward, whichever way the reader is paging history.
    let mut query = params.to_query(limit, false)?;
    let stream = async_stream::stream! {
        let mut announced_gap = false;
        loop {
            let guard = store().await.lock().await;
            let Some(repository) = guard.repository.as_ref() else {
                yield Ok(SseEvent::default().event("unavailable").data(
                    serde_json::json!({"availability": guard.availability}).to_string(),
                ));
                drop(guard);
                tokio::time::sleep(STREAM_POLL).await;
                continue;
            };
            match repository.query(&query).await {
                Ok(page) => {
                    if page.gap && !announced_gap {
                        announced_gap = true;
                        yield Ok(SseEvent::default().event("gap").data(
                            serde_json::json!({"pruned_through": page.pruned_through}).to_string(),
                        ));
                    }
                    for event in &page.events {
                        query.after_id = event.id;
                        let payload = serde_json::to_string(event)
                            .unwrap_or_else(|_| "{}".to_string());
                        // The id doubles as the SSE id, so a reconnect resumes exactly here.
                        yield Ok(SseEvent::default()
                            .id(event.id.to_string())
                            .event("log-event")
                            .data(payload));
                    }
                }
                Err(error) => {
                    yield Ok(SseEvent::default()
                        .event("error")
                        .data(serde_json::json!({"error": error.to_string()}).to_string()));
                }
            }
            drop(guard);
            tokio::time::sleep(STREAM_POLL).await;
        }
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// Export what the current query selects, streamed, without ever writing a file on the server.
pub async fn export_log_events(
    UrlQuery(params): UrlQuery<ListParams>,
) -> Result<Response, Response> {
    let csv = matches!(params.format.as_deref(), Some("csv"));
    let mut query = params.to_query(MAX_LIMIT, false)?;
    let guard = store().await.lock().await;
    let Some(repository) = guard.repository.as_ref() else {
        return Ok((StatusCode::SERVICE_UNAVAILABLE, "event store unavailable").into_response());
    };
    let mut body = Vec::new();
    if csv {
        body.extend_from_slice(
            b"id,occurred_at_ms,level,category,event_name,capture_kind,outcome,reason_code,message\n",
        );
    }
    let mut written = 0usize;
    loop {
        let page = repository
            .query(&query)
            .await
            .map_err(|error| report_to_response(AppError::Custom(error.to_string())))?;
        if page.events.is_empty() {
            break;
        }
        for event in &page.events {
            query.after_id = event.id;
            if csv {
                body.extend_from_slice(csv_row(event).as_bytes());
            } else {
                body.extend_from_slice(
                    serde_json::to_string(event)
                        .unwrap_or_else(|_| "{}".to_string())
                        .as_bytes(),
                );
                body.push(b'\n');
            }
            written += 1;
        }
        if written >= EXPORT_MAX_ROWS {
            // Saying the export is truncated is part of the export; a silent cut would be read
            // as "there was nothing more".
            let note = serde_json::json!({"truncated": true, "rows": written,
                                          "next_after_id": query.after_id});
            body.extend_from_slice(note.to_string().as_bytes());
            body.push(b'\n');
            break;
        }
    }
    let content_type = if csv {
        "text/csv"
    } else {
        "application/x-ndjson"
    };
    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            (
                header::CONTENT_DISPOSITION,
                if csv {
                    "attachment; filename=\"log-events.csv\""
                } else {
                    "attachment; filename=\"log-events.jsonl\""
                },
            ),
        ],
        Body::from(body),
    )
        .into_response())
}

/// CSV is a convenience view, so every field is quoted and escaped; the JSONL export stays the
/// lossless one.
fn csv_row(event: &StoredEvent) -> String {
    let data = &event.data;
    let field = |key: &str| {
        data.fields
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let quote = |value: &str| format!("\"{}\"", value.replace('"', "\"\""));
    format!(
        "{},{},{},{},{},{},{},{},{}\n",
        event.id,
        data.occurred_at_ms,
        quote(&format!("{:?}", data.level).to_uppercase()),
        quote(&data.category),
        quote(&data.event_name),
        quote(match data.capture_kind {
            CaptureKind::Native => "native",
            CaptureKind::LegacyBridge => "legacy_bridge",
        }),
        quote(&field("outcome")),
        quote(&field("reason_code")),
        quote(&data.message),
    )
}
