use crate::{CaptureKind, Draft, Event, EventData, Level, model, sanitize};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
pub struct Options {
    pub enabled: bool,
    pub bridge: bool,
    pub min_level: Level,
    pub queue_count: usize,
    pub queue_bytes: usize,
    pub batch_count: usize,
    pub flush_interval: Duration,
    pub max_attempts: usize,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            enabled: false,
            bridge: false,
            min_level: Level::Info,
            queue_count: 4096,
            queue_bytes: 16 * 1024 * 1024,
            batch_count: 64,
            flush_interval: Duration::from_millis(100),
            max_attempts: 3,
        }
    }
}
impl Options {
    fn validate(&self) -> Result<(), StorageError> {
        if !(4..=4096).contains(&self.queue_count)
            || !(4096..=16 * 1024 * 1024).contains(&self.queue_bytes)
            || !(1..=64).contains(&self.batch_count)
            || self.flush_interval.is_zero()
            || self.flush_interval > Duration::from_millis(100)
            || !(1..=3).contains(&self.max_attempts)
        {
            return Err(StorageError::new("invalid_options"));
        }
        Ok(())
    }
}

/// Codes, never raw SQL/path/error text, are safe for the independent stderr fallback.
#[derive(Debug, thiserror::Error)]
#[error("observability: {code}")]
pub struct StorageError {
    pub code: &'static str,
}
impl StorageError {
    pub fn new(code: &'static str) -> Self {
        Self { code }
    }
}

#[derive(Debug, Default)]
pub struct Commit {
    pub high_water: u64,
}
/// Consumer runs only on a private worker. Implementations must bound their I/O; shutdown does not
/// join an unresponsive consumer. No untrusted callback is invoked under the queue mutex.
pub trait Consumer: Send + 'static {
    fn write(&mut self, batch: &[Event]) -> Result<Commit, StorageError>;
    fn maintain(&mut self) -> Result<(), StorageError> {
        Ok(())
    }
    fn close(&mut self) -> Result<(), StorageError> {
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Health {
    pub queue_depth: usize,
    pub queue_bytes: usize,
    pub peak_queue_bytes: usize,
    pub in_flight: usize,
    pub accepted: u64,
    pub delivered: u64,
    /// TRACE, DEBUG, INFO, WARN, ERROR. Includes overflow, failed batches and shutdown discard.
    pub dropped: [u64; 5],
    pub storage_failures: u64,
    pub recoveries: u64,
    pub committed_id: u64,
    pub last_commit_ms: Option<i64>,
    pub last_error: Option<String>,
    pub state: String,
    pub closed: bool,
    pub shutdown_timed_out: bool,
}
impl Default for Health {
    fn default() -> Self {
        Self {
            queue_depth: 0,
            queue_bytes: 0,
            peak_queue_bytes: 0,
            in_flight: 0,
            accepted: 0,
            delivered: 0,
            dropped: [0; 5],
            storage_failures: 0,
            recoveries: 0,
            committed_id: 0,
            last_commit_ms: None,
            last_error: None,
            state: "starting".into(),
            closed: false,
            shutdown_timed_out: false,
        }
    }
}
struct Queue {
    events: VecDeque<(Event, usize)>,
    health: Health,
    closing: bool,
    deadline: Option<Instant>,
}
struct Shared {
    options: Options,
    queue: Mutex<Queue>,
    wake: Condvar,
    enabled: AtomicBool,
    sequence: AtomicU64,
    instance: String,
    run: String,
    version: String,
}
impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|p| p.into_inner())
    }
}
#[derive(Clone)]
pub struct Emitter {
    shared: Arc<Shared>,
}
impl Emitter {
    pub fn process_run_id(&self) -> &str {
        &self.shared.run
    }
    pub fn instance_id(&self) -> &str {
        &self.shared.instance
    }

    pub fn enabled(&self, level: Level) -> bool {
        self.shared.enabled.load(Ordering::Relaxed) && level >= self.shared.options.min_level
    }
    pub fn set_enabled(&self, enabled: bool) {
        self.shared.enabled.store(enabled, Ordering::Relaxed);
    }
    pub fn health(&self) -> Health {
        self.shared.lock().health.clone()
    }
    pub(crate) fn reject(&self, level: Level) {
        self.shared.lock().health.dropped[level.index()] += 1;
    }
    pub fn emit_with(&self, level: Level, build: impl FnOnce() -> Draft) -> bool {
        if !self.enabled(level) {
            return false;
        }
        match self.create(level, build()) {
            Ok(e) => self.submit(e),
            Err(_) => {
                self.shared.lock().health.dropped[level.index()] += 1;
                false
            }
        }
    }
    pub fn create(&self, level: Level, draft: Draft) -> Result<Event, StorageError> {
        self.create_inner(
            level,
            draft,
            CaptureKind::Native,
            "biliup::event",
            None,
            None,
        )
    }
    /// Durable outbox projection: caller persists the UID and original occurrence time, and retries
    /// the same event. This is not an atomic business+log commit.
    pub fn project(
        &self,
        uid: uuid::Uuid,
        occurred_at_ms: i64,
        level: Level,
        draft: Draft,
    ) -> Result<Event, StorageError> {
        self.create_inner(
            level,
            draft,
            CaptureKind::Native,
            "biliup::event",
            Some(uid),
            Some(occurred_at_ms),
        )
    }
    pub(crate) fn bridge_enabled(&self) -> bool {
        self.shared.options.bridge
    }
    pub(crate) fn create_inner(
        &self,
        level: Level,
        draft: Draft,
        kind: CaptureKind,
        target: &str,
        uid: Option<uuid::Uuid>,
        occurred: Option<i64>,
    ) -> Result<Event, StorageError> {
        if !model::valid_name(&draft.name) {
            return Err(StorageError::new("invalid_event_name"));
        }
        let mut fields = draft.context.0;
        fields.merge(&draft.fields);
        fields.insert("message", draft.message.into());
        let message = fields.text("message").unwrap_or_default().to_owned();
        fields.values.remove("message");
        fields.values.remove("event_name");
        let category = draft.name.split('.').next().unwrap().to_owned();
        Ok(Event {
            data: EventData {
                event_uid: uid.unwrap_or_else(uuid::Uuid::new_v4).to_string(),
                schema_version: 1,
                instance_id: self.shared.instance.clone(),
                process_run_id: self.shared.run.clone(),
                app_version: self.shared.version.clone(),
                occurred_at_ms: occurred.unwrap_or_else(model::now_ms),
                sequence: self.shared.sequence.fetch_add(1, Ordering::Relaxed),
                level,
                category,
                event_name: draft.name,
                message,
                target: sanitize::clean(target, 128).0,
                capture_kind: kind,
                fields,
            },
            diagnostic: draft.diagnostic,
        })
    }
    pub fn submit(&self, event: Event) -> bool {
        let level = event.data.level;
        if !self.enabled(level) {
            return false;
        }
        // All strings/maps were bounded before this serialization. Account for both object and JSON
        // storage plus allocator/tree overhead, not merely the number of events.
        let bytes = serde_json::to_vec(&event.data)
            .map(|v| v.len())
            .unwrap_or(usize::MAX);
        let diagnostic = event
            .diagnostic
            .as_ref()
            .map(|d| serde_json::to_vec(d).unwrap().len())
            .unwrap_or(0);
        let charge = bytes
            .saturating_add(diagnostic)
            .saturating_mul(3)
            .saturating_add(4096);
        let mut q = self.shared.lock();
        let high = level >= Level::Warn;
        let count_limit = self.shared.options.queue_count * if high { 4 } else { 3 } / 4;
        let byte_limit = self.shared.options.queue_bytes * if high { 4 } else { 3 } / 4;
        if q.closing
            || bytes > 32768
            || diagnostic > 16384
            || q.events.len() >= count_limit
            || q.health.queue_bytes.saturating_add(charge) > byte_limit
        {
            q.health.dropped[level.index()] += 1;
            return false;
        }
        q.events.push_back((event, charge));
        q.health.accepted += 1;
        q.health.queue_depth = q.events.len();
        q.health.queue_bytes += charge;
        q.health.peak_queue_bytes = q.health.peak_queue_bytes.max(q.health.queue_bytes);
        drop(q);
        self.shared.wake.notify_one();
        true
    }
}

pub struct Runtime {
    emitter: Emitter,
    worker: Option<thread::JoinHandle<()>>,
}
impl Runtime {
    /// Starts immediately; factory (including migrations) runs on the private worker.
    pub fn start<C: Consumer>(
        instance: &str,
        version: &str,
        options: Options,
        mut factory: impl FnMut() -> Result<C, StorageError> + Send + 'static,
    ) -> Result<Self, StorageError> {
        options.validate()?;
        if !sanitize::identifier(instance, 128) || !sanitize::identifier(version, 64) {
            return Err(StorageError::new("invalid_identity"));
        }
        let shared = Arc::new(Shared {
            enabled: AtomicBool::new(options.enabled),
            options,
            queue: Mutex::new(Queue {
                events: VecDeque::new(),
                health: Health::default(),
                closing: false,
                deadline: None,
            }),
            wake: Condvar::new(),
            sequence: AtomicU64::new(1),
            instance: instance.into(),
            run: uuid::Uuid::new_v4().to_string(),
            version: version.into(),
        });
        let worker_shared = shared.clone();
        let worker = thread::Builder::new()
            .name("observability-writer".into())
            .spawn(move || {
                tracing::subscriber::with_default(
                    tracing::subscriber::NoSubscriber::default(),
                    || {
                        worker_loop(&worker_shared, &mut factory);
                    },
                );
                let mut q = worker_shared.lock();
                q.health.closed = true;
                q.health.state = "closed".into();
                drop(q);
                worker_shared.wake.notify_all();
            })
            .map_err(|_| StorageError::new("worker_spawn_failed"))?;
        Ok(Self {
            emitter: Emitter { shared },
            worker: Some(worker),
        })
    }
    pub fn emitter(&self) -> Emitter {
        self.emitter.clone()
    }
    pub fn health(&self) -> Health {
        self.emitter.health()
    }
    pub fn shutdown(&mut self, timeout: Duration) -> Health {
        self.emitter.set_enabled(false);
        let shared = &self.emitter.shared;
        let deadline = Instant::now() + timeout;
        let mut q = shared.lock();
        q.closing = true;
        q.deadline = Some(deadline);
        shared.wake.notify_all();
        while !q.health.closed && Instant::now() < deadline {
            let (next, _) = shared
                .wake
                .wait_timeout(q, deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|p| p.into_inner());
            q = next;
        }
        if !q.health.closed {
            q.health.shutdown_timed_out = true;
            discard_queue(&mut q);
        }
        let health = q.health.clone();
        drop(q);
        if health.closed
            && let Some(worker) = self.worker.take()
        {
            let _ = worker.join();
        }
        health
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        if self.worker.is_some() {
            self.shutdown(Duration::ZERO);
        }
    }
}
fn discard_queue(q: &mut Queue) {
    while let Some((event, _)) = q.events.pop_front() {
        q.health.dropped[event.data.level.index()] += 1;
    }
    q.health.queue_depth = 0;
    q.health.queue_bytes = 0;
}
fn worker_loop<C: Consumer>(
    shared: &Shared,
    factory: &mut impl FnMut() -> Result<C, StorageError>,
) {
    // Initialization/migrations happen even before the first event, off the caller's thread.
    let mut consumer = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(&mut *factory)) {
        Ok(Ok(c)) => {
            shared.lock().health.state = "healthy".into();
            Some(c)
        }
        _ => {
            let mut q = shared.lock();
            q.health.state = "degraded".into();
            q.health.storage_failures += 1;
            q.health.last_error = Some("startup_failed".into());
            None
        }
    };
    let mut last_warning = None;
    let mut last_maintenance = Instant::now();
    loop {
        let batch = {
            let mut q = shared.lock();
            while q.events.is_empty() && !q.closing {
                let (next, _) = shared
                    .wake
                    .wait_timeout(q, shared.options.flush_interval)
                    .unwrap_or_else(|p| p.into_inner());
                q = next;
                if last_maintenance.elapsed() >= Duration::from_secs(1) {
                    break;
                }
            }
            if q.deadline.is_some_and(|d| Instant::now() >= d) {
                discard_queue(&mut q);
            }
            if q.closing && q.events.is_empty() {
                break;
            }
            // Collect at most one flush interval, even if producers wake us for each individual row.
            let flush_at = Instant::now() + shared.options.flush_interval;
            while !q.events.is_empty()
                && q.events.len() < shared.options.batch_count
                && !q.closing
                && Instant::now() < flush_at
            {
                let (next, _) = shared
                    .wake
                    .wait_timeout(q, flush_at.saturating_duration_since(Instant::now()))
                    .unwrap_or_else(|p| p.into_inner());
                q = next;
            }
            let mut batch = Vec::with_capacity(shared.options.batch_count);
            while batch.len() < shared.options.batch_count {
                let Some((event, charge)) = q.events.pop_front() else {
                    break;
                };
                q.health.queue_bytes -= charge;
                batch.push(event);
            }
            q.health.queue_depth = q.events.len();
            q.health.in_flight = batch.len();
            batch
        };
        if batch.is_empty() {
            if let Some(c) = &mut consumer {
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| c.maintain()));
                if !matches!(result, Ok(Ok(()))) {
                    let mut q = shared.lock();
                    q.health.storage_failures += 1;
                    q.health.state = "degraded".into();
                    q.health.last_error = Some("maintenance_failed".into());
                }
            }
            last_maintenance = Instant::now();
            continue;
        }
        let mut committed = false;
        for _ in 0..shared.options.max_attempts {
            if shared.lock().deadline.is_some_and(|d| Instant::now() >= d) {
                break;
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if consumer.is_none() {
                    consumer = Some(factory()?);
                }
                consumer.as_mut().unwrap().write(&batch)
            }))
            .unwrap_or_else(|_| Err(StorageError::new("consumer_panicked")));
            match result {
                Ok(commit) => {
                    let mut q = shared.lock();
                    if q.health.state == "degraded" {
                        q.health.recoveries += 1;
                    }
                    q.health.state = "healthy".into();
                    q.health.last_error = None;
                    q.health.delivered += batch.len() as u64;
                    q.health.committed_id = q.health.committed_id.max(commit.high_water);
                    q.health.last_commit_ms = Some(model::now_ms());
                    committed = true;
                    break;
                }
                Err(error) => {
                    consumer = None;
                    let mut q = shared.lock();
                    q.health.storage_failures += 1;
                    q.health.state = "degraded".into();
                    q.health.last_error = Some(sanitize::clean(error.code, 64).0);
                    drop(q);
                    if last_warning.is_none_or(|t: Instant| t.elapsed() >= Duration::from_secs(30))
                    {
                        // Do not print untrusted consumer error codes or use tracing here.
                        eprintln!("observability: storage unavailable; inspect health snapshot");
                        last_warning = Some(Instant::now());
                    }
                }
            }
        }
        let mut q = shared.lock();
        q.health.in_flight = 0;
        if !committed {
            for e in &batch {
                q.health.dropped[e.data.level.index()] += 1;
            }
        }
    }
    if let Some(mut consumer) = consumer
        && !matches!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| consumer.close())),
            Ok(Ok(()))
        )
    {
        shared.lock().health.storage_failures += 1;
    }
}
