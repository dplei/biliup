use biliup_observability::{sqlite::*, *};
use sqlx::{Connection, Row};
use std::{
    path::Path,
    time::{Duration, Instant},
};

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}
fn start(path: &Path) -> Runtime {
    let options = StoreOptions::new(path);
    Runtime::start_with_identity(
        "synthetic",
        "test",
        Options {
            enabled: true,
            ..Options::default()
        },
        move |instance_id, process_run_id| {
            SqliteStore::open(options.clone(), instance_id, process_run_id)
        },
    )
    .unwrap()
}
fn wait(emitter: &Emitter, predicate: impl Fn(&Health) -> bool) {
    let at = Instant::now();
    while !predicate(&emitter.health()) {
        assert!(
            at.elapsed() < Duration::from_secs(4),
            "{:?}",
            emitter.health()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn event(emitter: &Emitter, name: &str, task: &str) -> Event {
    let mut d = Draft::new(name, "合成事件");
    d.context = Context(Fields::new().with("task_id", task));
    emitter.create(Level::Info, d).unwrap()
}
fn store(path: &Path, run: &str) -> SqliteStore {
    SqliteStore::open(StoreOptions::new(path), "synthetic", run).unwrap()
}
fn expire(path: &Path, runs: &[&str]) {
    rt().block_on(async {
        let mut conn = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new().filename(path),
        )
        .await
        .unwrap();
        for run in runs {
            sqlx::query("UPDATE log_writer_run SET started_at_ms=?,heartbeat_at_ms=? WHERE process_run_id=?")
                .bind(now_ms() - 61_001)
                .bind(now_ms() - 61_001)
                .bind(run)
                .execute(&mut conn)
                .await
                .unwrap();
        }
    });
}
fn writer_meta(path: &Path) -> (i64, i64) {
    rt().block_on(async {
        let mut conn = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new().filename(path),
        )
        .await
        .unwrap();
        (
            sqlx::query_scalar("SELECT COUNT(*) FROM log_writer_run")
                .fetch_one(&mut conn)
                .await
                .unwrap(),
            sqlx::query_scalar("SELECT unclean_shutdowns FROM log_meta WHERE singleton=1")
                .fetch_one(&mut conn)
                .await
                .unwrap(),
        )
    })
}

#[test]
fn writer_runs_isolate_close_and_reconnect_identity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("writers.sqlite");
    let a = store(&path, "run-a");
    let started = rt().block_on(async {
        let mut conn = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new().filename(&path),
        )
        .await
        .unwrap();
        sqlx::query_scalar::<_, i64>(
            "SELECT started_at_ms FROM log_writer_run WHERE process_run_id='run-a'",
        )
        .fetch_one(&mut conn)
        .await
        .unwrap()
    });
    let mut b = store(&path, "run-b");
    b.close().unwrap();
    let mut reconnected = store(&path, "run-a");
    rt().block_on(async {
        let mut conn = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new().filename(&path),
        )
        .await
        .unwrap();
        let row = sqlx::query(
            "SELECT started_at_ms,closed_at_ms FROM log_writer_run WHERE process_run_id='run-a'",
        )
        .fetch_one(&mut conn)
        .await
        .unwrap();
        assert_eq!(row.get::<i64, _>(0), started);
        assert!(row.get::<Option<i64>, _>(1).is_none());
    });
    reconnected.close().unwrap();
    drop(a);
    assert_eq!(writer_meta(&path), (2, 0));
}

#[test]
fn expired_writer_reaping_is_once_only_and_recovery_stays_visible() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stale.sqlite");
    let a = store(&path, "run-a");
    let mut b = store(&path, "run-b");
    b.close().unwrap();
    expire(&path, &["run-a"]);
    let mut c = store(&path, "run-c");
    assert_eq!(writer_meta(&path), (3, 1));
    c.maintain().unwrap();
    assert_eq!(writer_meta(&path), (3, 1));
    drop(a);

    let mut recovered = store(&path, "run-a");
    rt().block_on(async {
        let mut conn = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new().filename(&path),
        )
        .await
        .unwrap();
        let row = sqlx::query("SELECT heartbeat_at_ms,stale_detected_at_ms FROM log_writer_run WHERE process_run_id='run-a'")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert!(row.get::<i64, _>(0) > now_ms() - 60_000);
        assert!(row.get::<Option<i64>, _>(1).is_some());
    });
    assert_eq!(writer_meta(&path).1, 1);
    recovered.close().unwrap();
    c.close().unwrap();
}

#[test]
fn multiple_expired_writers_and_self_renewal_are_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("multiple.sqlite");
    let a = store(&path, "run-a");
    let b = store(&path, "run-b");
    expire(&path, &["run-a", "run-b"]);
    drop(a);
    drop(b);
    let mut c = store(&path, "run-c");
    assert_eq!(writer_meta(&path), (3, 2));
    c.maintain().unwrap();
    assert_eq!(writer_meta(&path), (3, 2));
    c.close().unwrap();

    let self_path = dir.path().join("self.sqlite");
    let mut current = store(&self_path, "current");
    expire(&self_path, &["current"]);
    current.maintain().unwrap();
    assert_eq!(writer_meta(&self_path), (1, 0));
    current.close().unwrap();
}

#[test]
fn repository_reports_current_writer_health() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("health.sqlite");
    let mut active = store(&path, "active");
    let mut stale = store(&path, "stale");
    let mut closed = store(&path, "closed");
    closed.close().unwrap();
    expire(&path, &["stale"]);

    rt().block_on(async {
        let repo = Repository::open(&path).await.unwrap();
        let page = repo.query(&Query::default()).await.unwrap();
        assert_eq!(page.active_writer_runs, 1);
        assert_eq!(page.unknown_writer_runs, 1);
        repo.close().await;
    });
    active.close().unwrap();
    stale.close().unwrap();
}

#[test]
fn writer_history_cleanup_keeps_recovered_stale_run() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cleanup.sqlite");
    let mut current = store(&path, "current");
    let recovered = store(&path, "recovered");
    let mut closed = store(&path, "closed");
    closed.close().unwrap();
    let stale = store(&path, "stale");
    drop(stale);
    rt().block_on(async {
        let mut conn = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new().filename(&path),
        )
        .await
        .unwrap();
        sqlx::query("UPDATE log_writer_run SET stale_detected_at_ms=? WHERE process_run_id='recovered'")
            .bind(now_ms() - 1)
            .execute(&mut conn)
            .await
            .unwrap();
        let old = now_ms() - 91 * 86_400_000;
        sqlx::query("UPDATE log_writer_run SET started_at_ms=?,heartbeat_at_ms=?,closed_at_ms=? WHERE process_run_id='closed'")
            .bind(old).bind(old).bind(old).execute(&mut conn).await.unwrap();
        sqlx::query("UPDATE log_writer_run SET started_at_ms=?,heartbeat_at_ms=?,stale_detected_at_ms=? WHERE process_run_id='stale'")
            .bind(old).bind(old).bind(old).execute(&mut conn).await.unwrap();
    });
    current.maintain().unwrap();
    rt().block_on(async {
        let mut conn = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new().filename(&path),
        )
        .await
        .unwrap();
        let runs: Vec<String> =
            sqlx::query_scalar("SELECT process_run_id FROM log_writer_run ORDER BY process_run_id")
                .fetch_all(&mut conn)
                .await
                .unwrap();
        assert_eq!(runs, ["current", "recovered"]);
    });
    drop(recovered);
    current.close().unwrap();
}

#[test]
fn legacy_dirty_migration_is_consumed_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("migration.sqlite");
    let mut initial = store(&path, "initial");
    initial.close().unwrap();
    rt().block_on(async {
        let mut conn = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new().filename(&path),
        )
        .await
        .unwrap();
        sqlx::query("DROP TABLE log_writer_run")
            .execute(&mut conn)
            .await
            .unwrap();
        sqlx::query("DELETE FROM _sqlx_migrations WHERE version=3")
            .execute(&mut conn)
            .await
            .unwrap();
        sqlx::query("UPDATE log_meta SET dirty=1,unclean_shutdowns=7 WHERE singleton=1")
            .execute(&mut conn)
            .await
            .unwrap();
    });
    let mut migrated = store(&path, "migrated");
    assert_eq!(writer_meta(&path), (1, 8));
    migrated.close().unwrap();
    let mut reopened = store(&path, "reopened");
    assert_eq!(writer_meta(&path), (2, 8));
    reopened.close().unwrap();
}
#[test]
fn commit_idempotency_restart_query_and_attachment() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.sqlite");
    let mut runtime = start(&path);
    let emitter = runtime.emitter();
    let mut d = Draft::new("processing.command_failed", "外部工具失败");
    let mut c = DiagnosticCapture::new();
    c.push(b"fatal: synthetic\nAuthorization: synthetic-secret\n");
    d.diagnostic = Some(c.finish(Some(2)));
    d.context = Context(Fields::new().with("task_id", "alpha"));
    let e = emitter
        .project(uuid::Uuid::new_v4(), now_ms(), Level::Warn, d)
        .unwrap();
    let uid = e.data().event_uid.clone();
    assert!(emitter.submit(e.clone()));
    assert!(emitter.submit(e));
    for n in 0..30 {
        assert!(emitter.submit(event(
            &emitter,
            "system.started",
            if n % 2 == 0 { "alpha" } else { "beta" }
        )));
    }
    let health = runtime.shutdown(Duration::from_secs(2));
    assert!(health.closed);
    assert_eq!(health.committed_id, 31);
    assert_eq!(health.dropped, [0; 5]);
    rt().block_on(async {
        let repo = Repository::open(&path).await.unwrap();
        let mut cursor = 0;
        let mut count = 0;
        loop {
            let page = repo
                .query(&Query {
                    after_id: cursor,
                    limit: 7,
                    ..Query::default()
                })
                .await
                .unwrap();
            if page.events.is_empty() {
                break;
            }
            for row in page.events {
                assert!(row.id > cursor);
                cursor = row.id;
                count += 1;
            }
        }
        assert_eq!(count, 31);
        let page = repo
            .query(&Query {
                instance_id: Some("synthetic".into()),
                association: Some(("task_id".into(), "alpha".into())),
                limit: 200,
                ..Query::default()
            })
            .await
            .unwrap();
        assert_eq!(page.events.len(), 16);
        assert!(page.events[0].has_diagnostic);
        assert!(!serde_json::to_string(&page).unwrap().contains("fatal:"));
        let diagnostic = repo.diagnostic(&uid).await.unwrap().unwrap();
        assert!(diagnostic.to_string().contains("fatal:"));
        assert!(!diagnostic.to_string().contains("synthetic-secret"));
        assert!(
            repo.query(&Query {
                association: Some(("task_id".into(), "alpha".into())),
                ..Query::default()
            })
            .await
            .is_err()
        );
        repo.close().await;
    });
    let mut restarted = start(&path);
    restarted
        .emitter()
        .submit(event(&restarted.emitter(), "system.started", "restart"));
    assert_eq!(restarted.shutdown(Duration::from_secs(2)).committed_id, 32);
}

#[test]
fn busy_lock_is_bounded_and_recovery_gap_is_visible() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.sqlite");
    let mut runtime = start(&path);
    let emitter = runtime.emitter();
    emitter.submit(event(&emitter, "system.started", "before"));
    wait(&emitter, |h| h.committed_id == 1);
    let rt = rt();
    let mut conn = rt
        .block_on(sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new().filename(&path),
        ))
        .unwrap();
    rt.block_on(sqlx::query("BEGIN IMMEDIATE").execute(&mut conn))
        .unwrap();
    let at = Instant::now();
    emitter.submit(event(&emitter, "system.started", "lost"));
    assert!(at.elapsed() < Duration::from_millis(100));
    wait(&emitter, |h| h.dropped[2] == 1);
    assert!(emitter.health().storage_failures >= 3);
    rt.block_on(sqlx::query("ROLLBACK").execute(&mut conn))
        .unwrap();
    emitter.submit(event(&emitter, "system.started", "after"));
    wait(&emitter, |h| h.recoveries > 0);
    let health = runtime.shutdown(Duration::from_secs(2));
    assert_eq!(health.committed_id, 2);
    assert_eq!(health.dropped[2], 1);
    assert_eq!(writer_meta(&path), (1, 0));
}

#[test]
fn readonly_missing_directory_full_and_low_disk_are_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.sqlite");
    let mut runtime = start(&path);
    let emitter = runtime.emitter();
    emitter.submit(event(&emitter, "system.started", "init"));
    runtime.shutdown(Duration::from_secs(2));
    let mut ro = StoreOptions::new(&path);
    ro.read_only = true;
    assert!(SqliteStore::open(ro, "test", "readonly").is_err());
    let mut missing = start(&dir.path().join("absent/events.sqlite"));
    missing
        .emitter()
        .submit(event(&missing.emitter(), "system.started", "missing"));
    let h = missing.shutdown(Duration::from_secs(2));
    assert!(h.closed);
    assert_eq!(h.dropped[2], 1);
    let mut low = StoreOptions::new(dir.path().join("low.sqlite"));
    low.low_disk_bytes = u64::MAX;
    assert!(matches!(SqliteStore::open(low, "test", "low-disk"),Err(e) if e.code=="low_disk"));
    let mut full = StoreOptions::new(dir.path().join("full.sqlite"));
    full.max_pages = 40;
    let mut store = SqliteStore::open(full, "test", "full").unwrap();
    let mut failure = None;
    for _ in 0..100 {
        let mut d = Draft::new("processing.command_failed", "失败");
        let mut c = DiagnosticCapture::new();
        for _ in 0..20 {
            c.push(format!("{}\n", "x".repeat(800)).as_bytes());
        }
        d.diagnostic = Some(c.finish(Some(1)));
        if let Err(e) = store.write(&[emitter.create(Level::Error, d).unwrap()]) {
            failure = Some(e.code);
            break;
        }
    }
    assert_eq!(failure, Some("sqlite_full"));
}

#[test]
fn retention_budgets_cursors_consistent_backup_restore_and_readonly_repository() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.sqlite");
    let backup = dir.path().join("backup.sqlite");
    let mut idle = start(&dir.path().join("ids.sqlite"));
    let emitter = idle.emitter();
    let mut options = StoreOptions::new(&path);
    options.max_rows = 20;
    let mut store = SqliteStore::open(options, "test", "retention").unwrap();
    let old = now_ms() - 31 * 86_400_000;
    let old_info = emitter
        .project(
            uuid::Uuid::new_v4(),
            old,
            Level::Info,
            Draft::new("system.started", "旧INFO"),
        )
        .unwrap();
    let old_warn = emitter
        .project(
            uuid::Uuid::new_v4(),
            old,
            Level::Warn,
            Draft::new("system.started", "旧WARN"),
        )
        .unwrap();
    store.write(&[old_info, old_warn]).unwrap();
    store.maintain().unwrap();
    for _ in 0..40 {
        store
            .write(&[event(&emitter, "system.started", "bounded")])
            .unwrap();
    }
    store.backup(&backup).unwrap();
    assert!(store.backup(&backup).is_err());
    let rt = rt();
    rt.block_on(async {
        let repo = Repository::open(&path).await.unwrap();
        let restored = Repository::open(&backup).await.unwrap();
        let q = Query {
            after_id: 1,
            limit: 200,
            ..Query::default()
        };
        let page = repo.query(&q).await.unwrap();
        assert!(page.gap);
        assert!(page.events.len() <= 20);
        assert_eq!(
            page.events
                .iter()
                .map(|e| &e.data.event_uid)
                .collect::<Vec<_>>(),
            restored
                .query(&q)
                .await
                .unwrap()
                .events
                .iter()
                .map(|e| &e.data.event_uid)
                .collect::<Vec<_>>()
        );
        let mut conn = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&backup)
                .read_only(true),
        )
        .await
        .unwrap();
        assert!(
            sqlx::query("DELETE FROM log_event")
                .execute(&mut conn)
                .await
                .is_err()
        );
        repo.close().await;
        restored.close().await;
    });
    store.close().unwrap();
    idle.shutdown(Duration::from_secs(2));
}

#[test]
fn wal_pinned_reader_blocks_growth_not_producer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.sqlite");
    let mut ids = start(&dir.path().join("ids.sqlite"));
    let emitter = ids.emitter();
    let mut opts = StoreOptions::new(&path);
    opts.max_wal_bytes = 8 * 1024 * 1024;
    let mut store = SqliteStore::open(opts, "test", "wal").unwrap();
    store
        .write(&[event(&emitter, "system.started", "first")])
        .unwrap();
    let rt = rt();
    let mut reader = rt
        .block_on(sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new().filename(&path),
        ))
        .unwrap();
    rt.block_on(sqlx::query("BEGIN").execute(&mut reader))
        .unwrap();
    rt.block_on(sqlx::query("SELECT * FROM log_event").fetch_all(&mut reader))
        .unwrap();
    let failure = store
        .write(&[event(&emitter, "system.started", "blocked")])
        .unwrap_err();
    assert_eq!(failure.code, "wal_pinned");
    assert!(
        std::fs::metadata(format!("{}-wal", path.display()))
            .unwrap()
            .len()
            < 8 * 1024 * 1024
    );
    rt.block_on(sqlx::query("ROLLBACK").execute(&mut reader))
        .unwrap();
    store
        .write(&[event(&emitter, "system.started", "recovered")])
        .unwrap();
    store.close().unwrap();
    ids.shutdown(Duration::from_secs(2));
}

#[test]
fn kill_child() {
    let Some(path) = std::env::var_os("OBS_KILL_CHILD") else {
        return;
    };
    let runtime = start(Path::new(&path));
    let emitter = runtime.emitter();
    emitter.submit(event(&emitter, "system.started", "committed"));
    wait(&emitter, |h| h.committed_id == 1);
    // Keep a real write lock held: the next event cannot commit before the parent kills us.
    let rt = rt();
    let mut lock = rt
        .block_on(sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new().filename(&path),
        ))
        .unwrap();
    rt.block_on(sqlx::query("BEGIN IMMEDIATE").execute(&mut lock))
        .unwrap();
    emitter.submit(event(&emitter, "system.started", "uncommitted"));
    println!("COMMITTED");
    use std::io::Write;
    std::io::stdout().flush().unwrap();
    loop {
        std::thread::park_timeout(Duration::from_secs(1));
    }
}
#[test]
fn force_kill_retains_commit_and_reports_unclean_window() {
    use std::{
        io::{BufRead, BufReader},
        process::{Command, Stdio},
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("kill.sqlite");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "kill_child", "--nocapture"])
        .env("OBS_KILL_CHILD", &path)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if line.unwrap().contains("COMMITTED") {
                let _ = tx.send(());
                break;
            }
        }
    });
    let ready = rx.recv_timeout(Duration::from_secs(5));
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(ready.is_ok());
    rt().block_on(async {
        let mut conn = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new().filename(&path),
        )
        .await
        .unwrap();
        let old = now_ms() - 61_001;
        sqlx::query("UPDATE log_writer_run SET started_at_ms=?,heartbeat_at_ms=? WHERE closed_at_ms IS NULL")
            .bind(old)
            .bind(old)
            .execute(&mut conn)
            .await
            .unwrap();
    });
    let mut restart = start(&path);
    let emitter = restart.emitter();
    emitter.submit(event(&emitter, "system.started", "restart"));
    wait(&emitter, |h| h.committed_id == 2);
    rt().block_on(async {
        let repo = Repository::open(&path).await.unwrap();
        let page = repo
            .query(&Query {
                limit: 200,
                ..Query::default()
            })
            .await
            .unwrap();
        assert_eq!(page.events.len(), 2);
        assert!(page.unclean_shutdowns >= 1);
        repo.close().await;
    });
    restart.shutdown(Duration::from_secs(2));
}

#[test]
fn foreign_database_is_not_migrated_or_switched_to_wal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("business.sqlite");
    let rt = rt();
    rt.block_on(async {
        let mut c = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true),
        )
        .await
        .unwrap();
        sqlx::query("CREATE TABLE business(id INTEGER)")
            .execute(&mut c)
            .await
            .unwrap();
        c.close().await.unwrap();
    });
    assert!(
        matches!(SqliteStore::open(StoreOptions::new(&path), "test", "foreign"),Err(e) if e.code=="foreign_database")
    );
    rt.block_on(async {
        let mut c = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new().filename(&path),
        )
        .await
        .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, String>("PRAGMA journal_mode")
                .fetch_one(&mut c)
                .await
                .unwrap(),
            "delete"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM sqlite_master WHERE name='log_event'"
            )
            .fetch_one(&mut c)
            .await
            .unwrap(),
            0
        );
    });
}

#[test]
fn attachment_expiry_budget_rollback_and_page_clamps() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.sqlite");
    let mut ids = start(&dir.path().join("ids.sqlite"));
    let emitter = ids.emitter();
    let mut options = StoreOptions::new(&path);
    options.max_diagnostic_bytes = 1024;
    let mut store = SqliteStore::open(options, "test", "attachment").unwrap();
    let mut c = DiagnosticCapture::new();
    c.push(b"fatal: synthetic\n");
    let mut d = Draft::new("processing.command_failed", "失败");
    d.diagnostic = Some(c.finish(Some(1)));
    let e = emitter.create(Level::Error, d).unwrap();
    let uid = e.data().event_uid.clone();
    store.write(&[e]).unwrap();
    let rt = rt();
    rt.block_on(async {
        let mut conn = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new().filename(&path),
        )
        .await
        .unwrap();
        sqlx::query("UPDATE log_diagnostic SET created_at_ms=?")
            .bind(now_ms() - 8 * 86_400_000)
            .execute(&mut conn)
            .await
            .unwrap();
    });
    store.maintain().unwrap();
    let mut c = DiagnosticCapture::new();
    for _ in 0..100 {
        c.push(b"synthetic diagnostic line\n");
    }
    let mut d = Draft::new("processing.command_failed", "大附件");
    d.diagnostic = Some(c.finish(Some(1)));
    assert_eq!(
        store
            .write(&[emitter.create(Level::Error, d).unwrap()])
            .unwrap_err()
            .code,
        "diagnostic_budget"
    );
    rt.block_on(async {
        let repo = Repository::open(&path).await.unwrap();
        assert!(repo.diagnostic(&uid).await.unwrap().is_none());
        let q = Query {
            limit: usize::MAX,
            ..Query::default()
        };
        let page = repo.query(&q).await.unwrap();
        assert_eq!(page.events.len(), 1);
        assert!(!page.events[0].has_diagnostic);
        assert!(
            repo.query(&Query {
                after_id: u64::MAX,
                ..Query::default()
            })
            .await
            .is_err()
        );
        repo.close().await;
    });
    store.close().unwrap();
    ids.shutdown(Duration::from_secs(2));
}

/// Native and bridge live in the same table and must never be mixed by accident: the default is
/// whatever the caller asks for, the count agrees with the page, and a keyword is text, not a
/// LIKE pattern.
#[test]
fn capture_kind_keyword_and_count_agree_with_the_page() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.sqlite");
    let mut runtime = start(&path);
    let emitter = runtime.emitter();
    for (name, message) in [
        ("recording.started", "开始录制 100% 进度"),
        ("upload.failed", "分段上传失败"),
        ("upload.completed", "分段上传完成"),
    ] {
        let mut draft = Draft::new(name, message);
        draft.context = Context(Fields::new().with("task_id", "alpha"));
        let event = emitter.create(Level::Info, draft).unwrap();
        assert!(emitter.submit(event));
    }
    wait(&emitter, |h| h.delivered == 3);
    runtime.shutdown(Duration::from_secs(2));

    rt().block_on(async {
        let repo = Repository::open(&path).await.unwrap();
        let native = Query {
            capture_kind: Some(CaptureKind::Native),
            limit: 50,
            ..Query::default()
        };
        let page = repo.query(&native).await.unwrap();
        assert_eq!(page.events.len(), 3, "everything emitted here is native");
        assert_eq!(repo.count(&native).await.unwrap(), 3);

        let bridge = Query {
            capture_kind: Some(CaptureKind::LegacyBridge),
            limit: 50,
            ..Query::default()
        };
        assert!(repo.query(&bridge).await.unwrap().events.is_empty());
        assert_eq!(repo.count(&bridge).await.unwrap(), 0);

        // The count covers the whole filtered range, not just the page.
        let one = Query {
            capture_kind: Some(CaptureKind::Native),
            limit: 1,
            ..Query::default()
        };
        assert_eq!(repo.query(&one).await.unwrap().events.len(), 1);
        assert_eq!(repo.count(&one).await.unwrap(), 3);

        let keyword = |value: &str| Query {
            keyword: Some(value.to_string()),
            limit: 50,
            ..Query::default()
        };
        assert_eq!(repo.query(&keyword("上传")).await.unwrap().events.len(), 2);
        assert_eq!(repo.count(&keyword("上传")).await.unwrap(), 2);
        // A percent sign is searched for literally; it does not match everything.
        assert_eq!(repo.query(&keyword("100%")).await.unwrap().events.len(), 1);
        assert_eq!(repo.query(&keyword("%")).await.unwrap().events.len(), 1);
        assert!(repo.query(&keyword("_")).await.unwrap().events.is_empty());
    });
}
