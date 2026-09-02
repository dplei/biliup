//! One private writer connection and a separate read-only pool. No business database dependencies.
use crate::CaptureKind;
use crate::{Commit, Consumer, Event, EventData, Level, StorageError, now_ms};
use sqlx::{
    Connection, QueryBuilder, Row, Sqlite, SqliteConnection, SqlitePool, Transaction,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

const MIB: u64 = 1024 * 1024;
const DAY: i64 = 86_400_000;
const WRITER_LEASE_MS: i64 = 60_000;
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Clone, Debug)]
pub struct StoreOptions {
    pub path: PathBuf,
    pub max_event_bytes: u64,
    pub max_diagnostic_bytes: u64,
    pub max_rows: u64,
    pub max_wal_bytes: u64,
    pub max_pages: u32,
    pub low_disk_bytes: u64,
    /// Opens without write permission; useful for verifying degradation with an existing archive.
    pub read_only: bool,
}
impl StoreOptions {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            max_event_bytes: 128 * MIB,
            max_diagnostic_bytes: 32 * MIB,
            max_rows: 100_000,
            max_wal_bytes: 16 * MIB,
            max_pages: (192 * MIB / 4096) as u32,
            low_disk_bytes: 256 * MIB,
            read_only: false,
        }
    }
    fn validate(&self) -> Result<(), StorageError> {
        if self.max_event_bytes == 0
            || self.max_event_bytes > 128 * MIB
            || self.max_diagnostic_bytes > 32 * MIB
            || self.max_wal_bytes > 16 * MIB
            || self.max_wal_bytes < 8 * MIB
            || self.max_rows == 0
            || self.max_rows > 100_000
            || self.max_pages == 0
            || self.max_pages > (192 * MIB / 4096) as u32
        {
            return Err(StorageError::new("invalid_store_options"));
        }
        Ok(())
    }
}

fn db_error(e: sqlx::Error) -> StorageError {
    let code = match &e {
        sqlx::Error::Database(db) => match db.code().as_deref() {
            Some("5" | "6" | "261" | "517") => "sqlite_busy",
            Some("8" | "1032") => "sqlite_readonly",
            Some("13") => "sqlite_full",
            _ => "sqlite_error",
        },
        _ => "sqlite_unavailable",
    };
    StorageError::new(code)
}

fn connect_options(path: &Path, read_only: bool) -> SqliteConnectOptions {
    use sqlx::ConnectOptions;
    SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(!read_only)
        .read_only(read_only)
        .busy_timeout(Duration::from_millis(50))
        .foreign_keys(true)
        .disable_statement_logging()
}

pub struct SqliteStore {
    runtime: tokio::runtime::Runtime,
    connection: SqliteConnection,
    options: StoreOptions,
    writer: Writer,
}
#[derive(Debug)]
struct Writer {
    instance_id: String,
    process_run_id: String,
}
impl SqliteStore {
    /// Blocking, intended only inside Runtime's factory or an isolated maintenance process.
    pub fn open(
        options: StoreOptions,
        instance_id: &str,
        process_run_id: &str,
    ) -> Result<Self, StorageError> {
        options.validate()?;
        if !crate::sanitize::identifier(instance_id, 128)
            || !crate::sanitize::identifier(process_run_id, 128)
        {
            return Err(StorageError::new("invalid_identity"));
        }
        let writer = Writer {
            instance_id: instance_id.into(),
            process_run_id: process_run_id.into(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| StorageError::new("runtime_failed"))?;
        let connection = runtime.block_on(async {
            let mut conn = SqliteConnection::connect_with(&connect_options(&options.path, options.read_only))
                .await.map_err(db_error)?;
            let application_id: i64 = sqlx::query_scalar("PRAGMA application_id").fetch_one(&mut conn).await.map_err(db_error)?;
            let tables: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
                .fetch_one(&mut conn).await.map_err(db_error)?;
            // Refuse to migrate an existing business/foreign database even if the caller chose its path.
            if application_id != 0x424f4253 && (application_id != 0 || tables != 0) {
                return Err(StorageError::new("foreign_database"));
            }
            let page_size: i64 = sqlx::query_scalar("PRAGMA page_size").fetch_one(&mut conn).await.map_err(db_error)?;
            let pages: i64 = sqlx::query_scalar("PRAGMA page_count").fetch_one(&mut conn).await.map_err(db_error)?;
            if page_size != 4096 || pages > options.max_pages as i64 { return Err(StorageError::new("physical_budget")); }
            sqlx::raw_sql("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA wal_autocheckpoint=256; PRAGMA journal_size_limit=4194304; PRAGMA cache_size=-2048;")
                .execute(&mut conn).await.map_err(db_error)?;
            sqlx::query(&format!("PRAGMA max_page_count={}", options.max_pages)).execute(&mut conn).await.map_err(db_error)?;
            storage_gate(&mut conn, &options).await?;
            sqlx::query("PRAGMA application_id=1112490579").execute(&mut conn).await.map_err(db_error)?;
            MIGRATOR.run(&mut conn).await.map_err(|_| StorageError::new("migration_failed"))?;
            maintain(&mut conn, &options, &writer, now_ms()).await?;
            Ok::<_, StorageError>(conn)
        })?;
        Ok(Self {
            runtime,
            connection,
            options,
            writer,
        })
    }
    /// Bounded maintenance can also be scheduled by the host when the writer is idle.
    pub fn maintain(&mut self) -> Result<(), StorageError> {
        self.runtime.block_on(maintain(
            &mut self.connection,
            &self.options,
            &self.writer,
            now_ms(),
        ))
    }
    /// VACUUM INTO is a consistent SQLite snapshot including committed WAL contents. The destination
    /// must not exist; backups are explicit, not an automatic side effect of event capture.
    pub fn backup(&mut self, destination: &Path) -> Result<(), StorageError> {
        if destination.exists() {
            return Err(StorageError::new("backup_destination_exists"));
        }
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        if available_bytes(parent).is_none_or(|b| b < 208 * MIB) {
            return Err(StorageError::new("backup_space"));
        }
        let destination = destination
            .to_str()
            .ok_or_else(|| StorageError::new("invalid_backup_path"))?;
        self.runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(2),
                sqlx::query("VACUUM INTO ?")
                    .bind(destination)
                    .execute(&mut self.connection),
            )
            .await
            .map_err(|_| StorageError::new("backup_timeout"))?
            .map_err(db_error)?;
            Ok(())
        })
    }
}
impl Consumer for SqliteStore {
    fn write(&mut self, batch: &[Event]) -> Result<Commit, StorageError> {
        if batch.len() > 64 {
            return Err(StorageError::new("invalid_batch_size"));
        }
        let options = &self.options;
        self.runtime.block_on(async {
            maintain(&mut self.connection, options, &self.writer, now_ms()).await?;
            // Maintenance itself can append WAL frames, so recheck before the event transaction.
            storage_gate(&mut self.connection, options).await?;
            let mut tx = self.connection.begin().await.map_err(db_error)?;
            touch_writer(&mut tx, &self.writer, now_ms()).await?;
            let meta = sqlx::query("SELECT event_bytes, diagnostic_bytes, event_count FROM log_meta WHERE singleton=1")
                .fetch_one(&mut *tx).await.map_err(db_error)?;
            let mut event_bytes = meta.get::<i64,_>(0) as u64;
            let mut diagnostic_bytes = meta.get::<i64,_>(1) as u64;
            let mut count = meta.get::<i64,_>(2) as u64;
            for event in batch {
                let e = &event.data;
                if sqlx::query_scalar::<_,i64>("SELECT id FROM log_event WHERE event_uid=?")
                    .bind(&e.event_uid).fetch_optional(&mut *tx).await.map_err(db_error)?.is_some() { continue; }
                let payload = serde_json::to_string(e).map_err(|_| StorageError::new("invalid_payload"))?;
                let bytes = payload.len() as u64;
                // Stop rather than transiently exceeding any logical budget.
                if count >= options.max_rows || event_bytes + bytes > options.max_event_bytes {
                    return Err(StorageError::new("event_budget"));
                }
                let f = &e.fields;
                sqlx::query("INSERT INTO log_event(event_uid,occurred_at_ms,ingested_at_ms,instance_id,level,category,event_name,capture_kind,message,live_streamer_id,streamer_info_id,upload_session_id,segment_id,missing_id,download_attempt_id,upload_attempt_id,task_id,payload,byte_size) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
                    .bind(&e.event_uid).bind(e.occurred_at_ms).bind(now_ms()).bind(&e.instance_id)
                    .bind(e.level as i64).bind(&e.category).bind(&e.event_name)
                    .bind(match e.capture_kind { crate::CaptureKind::Native => "native", crate::CaptureKind::LegacyBridge => "legacy_bridge" })
                    .bind(&e.message)
                    .bind(f.text("live_streamer_id")).bind(f.text("streamer_info_id")).bind(f.text("upload_session_id"))
                    .bind(f.text("segment_id")).bind(f.text("missing_id")).bind(f.text("download_attempt_id"))
                    .bind(f.text("upload_attempt_id")).bind(f.text("task_id")).bind(payload).bind(bytes as i64)
                    .execute(&mut *tx).await.map_err(db_error)?;
                count += 1; event_bytes += bytes;
                if let Some(diagnostic) = &event.diagnostic {
                    let payload = serde_json::to_string(diagnostic).map_err(|_| StorageError::new("invalid_diagnostic"))?;
                    let bytes = payload.len() as u64;
                    if diagnostic_bytes + bytes > options.max_diagnostic_bytes {
                        return Err(StorageError::new("diagnostic_budget"));
                    }
                    sqlx::query("INSERT INTO log_diagnostic(event_uid,created_at_ms,payload,byte_size) VALUES(?,?,?,?)")
                        .bind(&e.event_uid).bind(now_ms()).bind(payload).bind(bytes as i64)
                        .execute(&mut *tx).await.map_err(db_error)?;
                    diagnostic_bytes += bytes;
                }
            }
            let high_water: i64 = sqlx::query_scalar("SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name='log_event'),0)")
                .fetch_one(&mut *tx).await.map_err(db_error)?;
            tx.commit().await.map_err(db_error)?;
            Ok(Commit { high_water: high_water as u64 })
        })
    }
    fn maintain(&mut self) -> Result<(), StorageError> {
        SqliteStore::maintain(self)
    }
    fn close(&mut self) -> Result<(), StorageError> {
        self.runtime.block_on(async {
            storage_gate(&mut self.connection, &self.options).await?;
            let closed = sqlx::query("UPDATE log_writer_run SET heartbeat_at_ms=MAX(heartbeat_at_ms,?), closed_at_ms=MAX(started_at_ms,?) WHERE process_run_id=? AND closed_at_ms IS NULL")
                .bind(now_ms())
                .bind(now_ms())
                .bind(&self.writer.process_run_id)
                .execute(&mut self.connection)
                .await
                .map_err(db_error)?;
            if closed.rows_affected() != 1 {
                return Err(StorageError::new("writer_run_closed"));
            }
            sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
                .execute(&mut self.connection)
                .await
                .map_err(db_error)?;
            Ok(())
        })
    }
}

async fn storage_gate(
    conn: &mut SqliteConnection,
    options: &StoreOptions,
) -> Result<(), StorageError> {
    let wal = PathBuf::from(format!("{}-wal", options.path.display()));
    if std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0) + 8 * MIB > options.max_wal_bytes {
        let row = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .fetch_one(&mut *conn)
            .await
            .map_err(db_error)?;
        if row.get::<i64, _>(0) != 0 {
            return Err(StorageError::new("wal_pinned"));
        }
    }
    let parent = options
        .path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if available_bytes(parent).is_none_or(|b| b < options.low_disk_bytes) {
        return Err(StorageError::new("low_disk"));
    }
    Ok(())
}

async fn maintain(
    conn: &mut SqliteConnection,
    options: &StoreOptions,
    writer: &Writer,
    now: i64,
) -> Result<(), StorageError> {
    storage_gate(conn, options).await?;
    let mut tx = conn.begin().await.map_err(db_error)?;
    touch_writer(&mut tx, writer, now).await?;
    reap_stale_writers(&mut tx, writer, now).await?;
    sqlx::query("DELETE FROM log_writer_run WHERE process_run_id IN (SELECT process_run_id FROM log_writer_run WHERE (closed_at_ms IS NOT NULL AND closed_at_ms < ?) OR (closed_at_ms IS NULL AND stale_detected_at_ms IS NOT NULL AND heartbeat_at_ms < ?) ORDER BY COALESCE(closed_at_ms,heartbeat_at_ms) LIMIT 64)")
        .bind(now-90*DAY).bind(now-90*DAY).execute(&mut *tx).await.map_err(db_error)?;
    sqlx::query("DELETE FROM log_diagnostic WHERE event_uid IN (SELECT event_uid FROM log_diagnostic WHERE created_at_ms < ? ORDER BY created_at_ms LIMIT 64)")
        .bind(now-7*DAY).execute(&mut *tx).await.map_err(db_error)?;
    sqlx::query("DELETE FROM log_event WHERE id IN (SELECT id FROM log_event WHERE (level < 3 AND occurred_at_ms < ?) OR occurred_at_ms < ? ORDER BY id LIMIT 64)")
        .bind(now-30*DAY).bind(now-90*DAY).execute(&mut *tx).await.map_err(db_error)?;
    let meta = sqlx::query(
        "SELECT event_bytes,diagnostic_bytes,event_count FROM log_meta WHERE singleton=1",
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(db_error)?;
    if meta.get::<i64, _>(1) as u64 >= options.max_diagnostic_bytes * 3 / 4 {
        sqlx::query("DELETE FROM log_diagnostic WHERE event_uid IN (SELECT event_uid FROM log_diagnostic ORDER BY created_at_ms LIMIT 64)")
            .execute(&mut *tx).await.map_err(db_error)?;
    }
    if meta.get::<i64, _>(0) as u64 >= options.max_event_bytes * 3 / 4
        || meta.get::<i64, _>(2) as u64 >= options.max_rows * 3 / 4
    {
        // Severity before age: low-value events are removed first; audit is only a projection here.
        sqlx::query("DELETE FROM log_event WHERE id IN (SELECT id FROM log_event ORDER BY level,id LIMIT 64)")
            .execute(&mut *tx).await.map_err(db_error)?;
    }
    tx.commit().await.map_err(db_error)?;
    Ok(())
}

async fn touch_writer(
    tx: &mut Transaction<'_, Sqlite>,
    writer: &Writer,
    now: i64,
) -> Result<(), StorageError> {
    let touched = sqlx::query("INSERT INTO log_writer_run(process_run_id,instance_id,started_at_ms,heartbeat_at_ms) VALUES(?,?,?,?) ON CONFLICT(process_run_id) DO UPDATE SET heartbeat_at_ms=MAX(log_writer_run.heartbeat_at_ms,excluded.heartbeat_at_ms) WHERE log_writer_run.closed_at_ms IS NULL AND log_writer_run.instance_id=excluded.instance_id")
        .bind(&writer.process_run_id)
        .bind(&writer.instance_id)
        .bind(now)
        .bind(now)
        .execute(&mut **tx)
        .await
        .map_err(db_error)?;
    if touched.rows_affected() != 1 {
        return Err(StorageError::new("writer_run_closed"));
    }
    Ok(())
}

async fn reap_stale_writers(
    tx: &mut Transaction<'_, Sqlite>,
    writer: &Writer,
    now: i64,
) -> Result<(), StorageError> {
    let reaped = sqlx::query("UPDATE log_writer_run SET stale_detected_at_ms=? WHERE process_run_id<>? AND closed_at_ms IS NULL AND heartbeat_at_ms<=? AND stale_detected_at_ms IS NULL")
        .bind(now)
        .bind(&writer.process_run_id)
        .bind(now.saturating_sub(WRITER_LEASE_MS))
        .execute(&mut **tx)
        .await
        .map_err(db_error)?
        .rows_affected();
    if reaped > 0 {
        sqlx::query("UPDATE log_meta SET unclean_shutdowns=unclean_shutdowns+? WHERE singleton=1")
            .bind(reaped as i64)
            .execute(&mut **tx)
            .await
            .map_err(db_error)?;
    }
    Ok(())
}

#[cfg(unix)]
pub fn available_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: valid nul-terminated path and writable stat buffer; read only after successful call.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return None;
    }
    let stat = unsafe { stat.assume_init() };
    #[allow(clippy::unnecessary_cast)] // statvfs integer widths differ between Unix platforms.
    let bytes = (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64);
    Some(bytes)
}
#[cfg(not(unix))]
pub fn available_bytes(_: &Path) -> Option<u64> {
    None
}

fn capture_kind_text(kind: CaptureKind) -> &'static str {
    match kind {
        CaptureKind::Native => "native",
        CaptureKind::LegacyBridge => "legacy_bridge",
    }
}

/// Keep user text out of LIKE's own wildcard grammar: a search for `%` is a search for `%`.
fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// The one place the filter grammar lives, so the page and its count can never disagree.
fn push_filters<'a>(
    sql: &mut QueryBuilder<'a, Sqlite>,
    query: &'a Query,
) -> Result<(), StorageError> {
    // Set filters stay bounded here rather than at the caller, so `count` is limited exactly like
    // the page it counts.
    if query.levels.len() > 5
        || query.categories.len() > 16
        || query.categories.iter().any(|value| value.len() > 128)
    {
        return Err(StorageError::new("invalid_query"));
    }
    if let Some(id) = query.until_id {
        sql.push(" AND id <= ").push_bind(id as i64);
    }
    if let Some(ms) = query.since_ms {
        sql.push(" AND occurred_at_ms >= ").push_bind(ms);
    }
    if let Some(ms) = query.until_ms {
        sql.push(" AND occurred_at_ms <= ").push_bind(ms);
    }
    if let Some(level) = query.min_level {
        sql.push(" AND level >= ").push_bind(level as i64);
    }
    for (column, value) in [
        ("category", &query.category),
        ("event_name", &query.event_name),
        ("instance_id", &query.instance_id),
    ] {
        if let Some(value) = value {
            sql.push(" AND ").push(column).push(" = ").push_bind(value);
        }
    }
    if let Some((key, value)) = &query.association {
        if crate::model::field_kind(key) != Some("id") || query.instance_id.is_none() {
            return Err(StorageError::new("invalid_association"));
        }
        sql.push(" AND ").push(key).push(" = ").push_bind(value);
    }
    if !query.levels.is_empty() {
        // An exact set, not a floor: "只看信息" and "警告+错误" are both one query, and neither
        // can be expressed by `level >=`.
        sql.push(" AND level IN (");
        let mut list = sql.separated(", ");
        for level in &query.levels {
            list.push_bind(*level as i64);
        }
        sql.push(")");
    }
    if !query.categories.is_empty() {
        sql.push(" AND category IN (");
        let mut list = sql.separated(", ");
        for category in &query.categories {
            list.push_bind(category.as_str());
        }
        sql.push(")");
    }
    if let Some(kind) = query.capture_kind {
        sql.push(" AND capture_kind = ")
            .push_bind(capture_kind_text(kind));
    }
    if let Some(keyword) = &query.keyword {
        // LIKE with a leading wildcard cannot use an index anywhere, so it is applied to the
        // bounded summary column and never to the payload.
        sql.push(" AND message LIKE ")
            .push_bind(format!("%{}%", escape_like(keyword)))
            .push(" ESCAPE '\\'");
    }
    Ok(())
}

#[derive(Debug, Default)]
pub struct Query {
    pub after_id: u64,
    pub until_id: Option<u64>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub min_level: Option<Level>,
    pub category: Option<String>,
    /// An exact set of levels; empty means "no set filter". Combined with `min_level` by AND, so a
    /// caller uses one or the other.
    pub levels: Vec<Level>,
    /// An exact set of categories; empty means "no set filter". Same AND rule against `category`.
    pub categories: Vec<String>,
    pub event_name: Option<String>,
    pub instance_id: Option<String>,
    /// One exact allowlisted correlation field; instance_id is mandatory when it is used.
    pub association: Option<(String, String)>,
    /// `None` means both kinds. Callers that show business events ask for `Native` explicitly;
    /// bridge diagnostics are never mixed in silently.
    pub capture_kind: Option<CaptureKind>,
    /// Case-insensitive substring of the summary only. It is a bounded scan over the filtered
    /// range, not an index lookup, and is cut off by the same VM deadline as everything else.
    pub keyword: Option<String>,
    /// Return the newest rows of the range first. Paging older then moves `until_id` down instead
    /// of `after_id` up; the live cursor is still the largest id seen, in either direction.
    pub newest_first: bool,
    pub limit: usize,
}
#[derive(Debug, serde::Serialize)]
pub struct StoredEvent {
    pub id: u64,
    pub ingested_at_ms: i64,
    pub data: EventData,
    pub has_diagnostic: bool,
}
#[derive(Debug, serde::Serialize)]
pub struct Page {
    pub events: Vec<StoredEvent>,
    pub pruned_through: u64,
    pub gap: bool,
    pub unclean_shutdowns: u64,
    pub active_writer_runs: u64,
    pub unknown_writer_runs: u64,
}
#[derive(Clone)]
pub struct Repository {
    pool: SqlitePool,
}
impl Repository {
    pub async fn open(path: &Path) -> Result<Self, StorageError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_millis(100))
            .after_release(|conn, _| {
                Box::pin(async move {
                    conn.lock_handle().await?.remove_progress_handler();
                    Ok(true)
                })
            })
            .connect_with(
                connect_options(path, true)
                    .pragma("query_only", "ON")
                    .pragma("cache_size", "-2048"),
            )
            .await
            .map_err(db_error)?;
        Ok(Self { pool })
    }
    pub async fn close(&self) {
        self.pool.close().await;
    }
    pub async fn query(&self, query: &Query) -> Result<Page, StorageError> {
        tokio::time::timeout(Duration::from_millis(250), self.query_inner(query))
            .await
            .map_err(|_| StorageError::new("query_timeout"))?
    }
    async fn query_inner(&self, query: &Query) -> Result<Page, StorageError> {
        if [&query.instance_id, &query.event_name, &query.category]
            .into_iter()
            .flatten()
            .any(|s| s.len() > 128)
            || query
                .association
                .as_ref()
                .is_some_and(|(key, value)| key.len() > 32 || value.len() > 128)
        {
            return Err(StorageError::new("invalid_query"));
        }
        if query.after_id > i64::MAX as u64 || query.until_id.is_some_and(|v| v > i64::MAX as u64) {
            return Err(StorageError::new("invalid_cursor"));
        }
        let mut sql = QueryBuilder::<Sqlite>::new(
            "SELECT id,ingested_at_ms,payload,EXISTS(SELECT 1 FROM log_diagnostic d WHERE d.event_uid=e.event_uid) AS has_diagnostic FROM log_event e WHERE id > ",
        );
        sql.push_bind(query.after_id as i64);
        push_filters(&mut sql, query)?;
        sql.push(if query.newest_first {
            " ORDER BY id DESC LIMIT "
        } else {
            " ORDER BY id LIMIT "
        })
        .push_bind(query.limit.clamp(1, 200) as i64);
        // One bounded snapshot for both the page and retention marker.
        let mut conn = self.pool.acquire().await.map_err(db_error)?;
        // Bound the SQLite VM itself, not only the waiting Rust future: a cancelled query must not
        // leave a read transaction pinning WAL indefinitely in SQLx's worker thread.
        let deadline = std::time::Instant::now() + Duration::from_millis(200);
        conn.lock_handle()
            .await
            .map_err(db_error)?
            .set_progress_handler(1000, move || std::time::Instant::now() < deadline);
        let mut tx = conn.begin().await.map_err(db_error)?;
        let lease_deadline = now_ms().saturating_sub(WRITER_LEASE_MS);
        let meta = sqlx::query("SELECT pruned_through,unclean_shutdowns,(SELECT COUNT(*) FROM log_writer_run WHERE closed_at_ms IS NULL AND heartbeat_at_ms>?),(SELECT COUNT(*) FROM log_writer_run WHERE closed_at_ms IS NULL AND heartbeat_at_ms<=?) FROM log_meta WHERE singleton=1")
            .bind(lease_deadline)
            .bind(lease_deadline)
            .fetch_one(&mut *tx)
            .await
            .map_err(db_error)?;
        let rows = sql.build().fetch_all(&mut *tx).await.map_err(db_error)?;
        tx.commit().await.map_err(db_error)?;
        let pruned_through = meta.get::<i64, _>(0) as u64;
        let events = rows
            .into_iter()
            .map(|row| {
                Ok(StoredEvent {
                    id: row.get::<i64, _>("id") as u64,
                    ingested_at_ms: row.get("ingested_at_ms"),
                    data: serde_json::from_str(row.get("payload"))
                        .map_err(|_| StorageError::new("invalid_stored_event"))?,
                    has_diagnostic: row.get("has_diagnostic"),
                })
            })
            .collect::<Result<_, StorageError>>()?;
        Ok(Page {
            events,
            pruned_through,
            gap: query.after_id > 0 && query.after_id < pruned_through,
            unclean_shutdowns: meta.get::<i64, _>(1) as u64,
            active_writer_runs: meta.get::<i64, _>(2) as u64,
            unknown_writer_runs: meta.get::<i64, _>(3) as u64,
        })
    }
    /// How many rows the whole filtered range holds, independent of the page. Counting is bounded
    /// by the same VM deadline: a range too large to count says so instead of blocking.
    pub async fn count(&self, query: &Query) -> Result<u64, StorageError> {
        tokio::time::timeout(Duration::from_millis(250), self.count_inner(query))
            .await
            .map_err(|_| StorageError::new("query_timeout"))?
    }

    async fn count_inner(&self, query: &Query) -> Result<u64, StorageError> {
        let mut sql = QueryBuilder::<Sqlite>::new("SELECT COUNT(*) FROM log_event e WHERE id > ");
        sql.push_bind(query.after_id as i64);
        push_filters(&mut sql, query)?;
        let mut conn = self.pool.acquire().await.map_err(db_error)?;
        let deadline = std::time::Instant::now() + Duration::from_millis(200);
        conn.lock_handle()
            .await
            .map_err(db_error)?
            .set_progress_handler(1000, move || std::time::Instant::now() < deadline);
        let total: i64 = sql
            .build_query_scalar()
            .fetch_one(&mut *conn)
            .await
            .map_err(db_error)?;
        Ok(total.max(0) as u64)
    }

    pub async fn diagnostic(
        &self,
        event_uid: &str,
    ) -> Result<Option<serde_json::Value>, StorageError> {
        if uuid::Uuid::parse_str(event_uid).is_err() {
            return Err(StorageError::new("invalid_event_uid"));
        }
        let payload: Option<String> = tokio::time::timeout(
            Duration::from_millis(250),
            sqlx::query_scalar("SELECT payload FROM log_diagnostic WHERE event_uid=?")
                .bind(event_uid)
                .fetch_optional(&self.pool),
        )
        .await
        .map_err(|_| StorageError::new("query_timeout"))?
        .map_err(db_error)?;
        payload
            .map(|p| serde_json::from_str(&p).map_err(|_| StorageError::new("invalid_diagnostic")))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn read_vm_deadline_releases_connection_for_next_query() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.sqlite");
        let mut store = SqliteStore::open(StoreOptions::new(&path), "test", "read-vm").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let repo=Repository::open(&path).await.unwrap();
            let mut conn=repo.pool.acquire().await.unwrap();
            let at=std::time::Instant::now();let deadline=at+Duration::from_millis(30);
            conn.lock_handle().await.unwrap().set_progress_handler(1000,move||std::time::Instant::now()<deadline);
            let result=sqlx::query("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000000000) SELECT SUM(x) FROM n")
                .fetch_one(&mut *conn).await;
            assert!(result.is_err());assert!(at.elapsed()<Duration::from_millis(250));
            drop(conn);
            assert!(repo.query(&Query::default()).await.unwrap().events.is_empty());
            let mut conn=repo.pool.acquire().await.unwrap();
            assert!(sqlx::query("DELETE FROM log_event").execute(&mut *conn).await.is_err());
            drop(conn);repo.close().await;
        });
        store.close().unwrap();
    }
}
