//! Opt-in P2 integration. Configuration is read at entry, never hot-reloads the subscriber.
use crate::{
    CaptureLayer, Emitter, Health, Level, Options, Runtime,
    sqlite::{SqliteStore, StoreOptions},
};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex, Weak},
    time::Duration,
};
use tracing::{Dispatch, Subscriber};

#[derive(Clone, Debug)]
pub struct Config {
    pub path: PathBuf,
    pub instance: String,
    pub level: Level,
}
impl Config {
    pub fn from_env() -> Result<Option<Self>, &'static str> {
        match std::env::var("BILIUP_OBSERVABILITY").as_deref() {
            Err(_) | Ok("0") | Ok("off") => return Ok(None),
            Ok("1") => (),
            _ => return Err("invalid_enable_flag"),
        }
        let path = std::env::var_os("BILIUP_OBSERVABILITY_DB")
            .filter(|v| !v.is_empty())
            .ok_or("explicit_database_required")?;
        let instance = std::env::var("BILIUP_OBSERVABILITY_INSTANCE")
            .map_err(|_| "explicit_instance_required")?;
        let level = match std::env::var("BILIUP_OBSERVABILITY_LEVEL").as_deref() {
            Err(_) | Ok("info") => Level::Info,
            Ok("trace") => Level::Trace,
            Ok("debug") => Level::Debug,
            Ok("warn") => Level::Warn,
            Ok("error") => Level::Error,
            _ => return Err("invalid_capture_level"),
        };
        Ok(Some(Self {
            path: path.into(),
            instance,
            level,
        }))
    }
}
struct Shared {
    runtime: Mutex<Runtime>,
    emitter: Emitter,
    key: PathBuf,
    instance: String,
    level: Level,
}
struct Entry {
    active: Weak<Shared>,
    last: Option<serde_json::Value>,
}
static RUNS: LazyLock<Mutex<BTreeMap<PathBuf, Entry>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));
impl Drop for Shared {
    fn drop(&mut self) {
        let health = self
            .runtime
            .get_mut()
            .unwrap()
            .shutdown(Duration::from_secs(2));
        let frame = health_frame(&self.emitter, health);
        eprintln!(
            "observability_health={}",
            serde_json::json!({"schema_version":1,"legacy_file_health":"unknown","runs":[frame.clone()]})
        );
        if let Some(entry) = RUNS.lock().unwrap().get_mut(&self.key) {
            entry.last = Some(frame);
        }
    }
}
/// Keep this guard until business work and the old nonblocking file guard have drained.
pub struct Shadow(Option<Arc<Shared>>);
impl Shadow {
    pub fn from_env(version: &str) -> Self {
        match Config::from_env() {
            Ok(None) => Self(None),
            Ok(Some(config)) => Self::start(config, version).unwrap_or_else(|code| {
                eprintln!("observability: {code}; legacy output retained");
                Self(None)
            }),
            Err(code) => {
                eprintln!("observability: {code}; legacy output retained");
                Self(None)
            }
        }
    }
    pub fn start(config: Config, version: &str) -> Result<Self, &'static str> {
        // Resolve parent aliases without creating directories or touching the business DB.
        let parent = config
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        let key = parent
            .canonicalize()
            .unwrap_or_else(|_| parent.to_path_buf())
            .join(config.path.file_name().ok_or("invalid_database_path")?);
        let mut runs = RUNS.lock().unwrap();
        if let Some(entry) = runs.get(&key) {
            if let Some(active) = entry.active.upgrade() {
                if active.instance != config.instance || active.level != config.level {
                    return Err("active_capture_config_conflict");
                }
                return Ok(Self(Some(active)));
            }
            if entry.last.is_none() {
                return Err("capture_closing_retry");
            }
        }
        // Bound retained health entries for hosts invoking many independent paths.
        if runs.len() >= 64 {
            runs.retain(|_, e| e.active.strong_count() > 0);
        }
        if runs.len() >= 64 {
            return Err("too_many_capture_databases");
        }
        let options = StoreOptions::new(&key);
        let runtime = Runtime::start(
            &config.instance,
            version,
            Options {
                enabled: true,
                bridge: true,
                min_level: config.level,
                ..Options::default()
            },
            move || SqliteStore::open(options.clone()),
        )
        .map_err(|e| e.code)?;
        let shared = Arc::new(Shared {
            emitter: runtime.emitter(),
            runtime: Mutex::new(runtime),
            key: key.clone(),
            instance: config.instance,
            level: config.level,
        });
        runs.insert(
            key,
            Entry {
                active: Arc::downgrade(&shared),
                last: None,
            },
        );
        Ok(Self(Some(shared)))
    }
    pub fn layer(&self) -> Option<CaptureLayer> {
        self.0
            .as_ref()
            .map(|s| CaptureLayer::new(s.emitter.clone()))
    }
    pub fn health(&self) -> Option<Health> {
        self.0.as_ref().map(|s| s.emitter.health())
    }
    /// Preserve an arbitrary embedding host subscriber for helpers that never owned a formatter.
    /// Bridge fields are captured directly; absent inherited span fields remain unknown.
    pub fn inherited_dispatch(&self) -> Dispatch {
        let host = tracing::dispatcher::get_default(Clone::clone);
        if host.downcast_ref::<Inherited>().is_some()
            || host.downcast_ref::<CaptureLayer>().is_some()
        {
            return host;
        }
        match &self.0 {
            Some(s) => Dispatch::new(Inherited {
                host,
                emitter: s.emitter.clone(),
            }),
            None => host,
        }
    }
}
fn health_frame(emitter: &Emitter, health: Health) -> serde_json::Value {
    let mut value = serde_json::to_value(health).expect("health serialization");
    value["process_run_id"] = emitter.process_run_id().into();
    value["instance_id"] = emitter.instance_id().into();
    value
}

/// New storage health only. Old file delivery is explicitly unknown; it cannot be inferred here.
pub fn health_snapshot() -> serde_json::Value {
    let (active, last): (Vec<_>, Vec<_>) = {
        let runs = RUNS.lock().unwrap();
        (
            runs.values().filter_map(|e| e.active.upgrade()).collect(),
            runs.values().filter_map(|e| e.last.clone()).collect(),
        )
    };
    let states: Vec<_> = active
        .iter()
        .map(|s| health_frame(&s.emitter, s.emitter.health()))
        .chain(last)
        .collect();
    serde_json::json!({"schema_version":1,"capture_config_version":"shadow-v1","legacy_file_health":"unknown","runs":states})
}

/// Retrieve only this invocation's collector. Never search global runs or initialize storage
/// from a business callback: overlapping embedded calls may use different databases.
pub fn current_emitter() -> Option<Emitter> {
    tracing::dispatcher::get_default(|dispatch| {
        dispatch.downcast_ref::<CaptureLayer>().map(CaptureLayer::emitter)
            .or_else(|| dispatch.downcast_ref::<Inherited>().map(|s| s.emitter.clone()))
    })
}

thread_local! { static DISPATCH_GUARD: RefCell<Option<tracing::dispatcher::DefaultGuard>> = const { RefCell::new(None) }; }
/// Scope root futures, worker threads and blocking workers to one entry's dispatch. No second global
/// subscriber, no guard held over an await, and no changes to the embedding host's global state.
pub fn block_on_inherited<F: std::future::Future>(
    dispatch: Dispatch,
    current_thread: bool,
    future: F,
) -> std::io::Result<F::Output> {
    let workers = dispatch.downcast_ref::<CaptureLayer>().is_some()
        || dispatch.downcast_ref::<Inherited>().is_some();
    block_on_impl(dispatch, current_thread, workers, future)
}

pub fn block_on<F: std::future::Future>(
    dispatch: Dispatch,
    current_thread: bool,
    future: F,
) -> std::io::Result<F::Output> {
    block_on_impl(dispatch, current_thread, true, future)
}

fn block_on_impl<F: std::future::Future>(
    dispatch: Dispatch,
    current_thread: bool,
    workers: bool,
    future: F,
) -> std::io::Result<F::Output> {
    let mut builder = if current_thread {
        tokio::runtime::Builder::new_current_thread()
    } else {
        tokio::runtime::Builder::new_multi_thread()
    };
    let worker_dispatch = dispatch.clone();
    let runtime = builder
        .enable_all()
        .on_thread_start(move || {
            if !workers {
                return;
            }
            DISPATCH_GUARD.with(|slot| {
                *slot.borrow_mut() = Some(tracing::dispatcher::set_default(&worker_dispatch))
            });
        })
        .on_thread_stop(|| {
            DISPATCH_GUARD.with(|slot| slot.borrow_mut().take());
        })
        .build()?;
    Ok(tracing::dispatcher::with_default(&dispatch, || {
        runtime.block_on(future)
    }))
}
struct Inherited {
    host: Dispatch,
    emitter: Emitter,
}
impl Subscriber for Inherited {
    fn enabled(&self, m: &tracing::Metadata<'_>) -> bool {
        self.host.enabled(m) || self.emitter.enabled(Level::from_tracing(m.level()))
    }
    fn new_span(&self, a: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        self.host.new_span(a)
    }
    fn record(&self, id: &tracing::span::Id, r: &tracing::span::Record<'_>) {
        self.host.record(id, r);
    }
    fn record_follows_from(&self, id: &tracing::span::Id, follows: &tracing::span::Id) {
        self.host.record_follows_from(id, follows);
    }
    fn event(&self, event: &tracing::Event<'_>) {
        if crate::legacy_output(event.metadata()) && self.host.enabled(event.metadata()) {
            self.host.event(event);
        }
        crate::capture::capture_event(&self.emitter, event, crate::Fields::new());
    }
    fn enter(&self, id: &tracing::span::Id) {
        self.host.enter(id);
    }
    fn exit(&self, id: &tracing::span::Id) {
        self.host.exit(id);
    }
    fn clone_span(&self, id: &tracing::span::Id) -> tracing::span::Id {
        self.host.clone_span(id)
    }
    fn try_close(&self, id: tracing::span::Id) -> bool {
        self.host.try_close(id)
    }
    fn current_span(&self) -> tracing_core::span::Current {
        self.host.current_span()
    }
}
