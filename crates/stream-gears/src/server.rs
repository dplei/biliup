use biliup::uploader::util::SubmitOption;
use biliup_cli::cli::{Cli, Commands};
use biliup_cli::downloader::generate_json;
use biliup_cli::server::common::lifecycle_backfill::run_lifecycle_backfill;
use biliup_cli::server::config::Config;
use biliup_cli::server::errors::AppResult;
use biliup_cli::server::infrastructure::connection_pool::ConnectionManager;
use biliup_cli::uploader::{
    append, comments, list, login, renew, reply, show, upload_by_command, upload_by_config,
};
use biliup_observability::{
    legacy_output,
    shadow::{self, Shadow},
};
use clap::Parser;
use pyo3::prelude::PyAnyMethods;
use pyo3::prelude::PyDictMethods;
use pyo3::types::PyDict;
use pyo3::{Bound, PyAny, PyResult, Python};
use pyo3::{pyclass, pyfunction, pymethods};
use pythonize::pythonize;
use std::ops::Deref;
use std::sync::{Arc, LazyLock, RwLock};
use time::macros::format_description;
use tracing::info;
use tracing_appender::rolling::Rotation;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{EnvFilter, reload};

#[pyclass]
#[derive(Debug, Clone)]
struct OnceConfig {
    // 用 PyObject 存，方便保持任意 Python 对象
    map: Config,
}

#[pymethods]
impl OnceConfig {
    /// 获取：config.get("k", default=None)
    /// - 若 key 存在，返回保存的对象
    /// - 若不存在，返回 default（默认 None）
    #[pyo3(signature = (key, default=None))]
    fn get<'py>(
        &self,
        py: Python<'py>,
        key: &str,
        default: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let guard = &self.map;
        // serde_json::to_value(guard.deref())
        if let Some(bound) = pythonize(py, guard)?
            .extract::<Bound<PyDict>>()?
            .get_item(key)?
        {
            if bound.is_none()
                && let Some(d) = default
            {
                return Ok(d);
            }
            // 尝试转换为字典并过滤
            return match bound.cast::<PyDict>() {
                Ok(dict) => {
                    let filtered = PyDict::new(py);
                    dict.iter()
                        .filter(|(_, v)| !v.is_none())
                        .try_for_each(|(k, v)| filtered.set_item(k, v))?;
                    Ok(filtered.into_any())
                }
                Err(_) => Ok(bound), // 不是字典，直接返回
            };
        };
        let Some(default) = default else {
            return Err(pyo3::exceptions::PyAttributeError::new_err(format!(
                "object has no attribute '{key}'"
            )));
        };
        Ok(default)
    }
}

#[pyclass]
pub struct ConfigState {
    // 用 PyObject 存，方便保持任意 Python 对象
    map: Arc<RwLock<Config>>,
}

#[pymethods]
impl ConfigState {
    /// 获取：config.get("k", default=None)
    /// - 若 key 存在，返回保存的对象
    /// - 若不存在，返回 default（默认 None）
    #[pyo3(signature = (key, default=None))]
    fn get<'py>(
        &self,
        py: Python<'py>,
        key: &str,
        default: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let guard = self.map.read().unwrap();
        // serde_json::to_value(guard.deref())
        if let Some(bound) = pythonize(py, guard.deref())?
            .extract::<Bound<PyDict>>()?
            .get_item(key)?
        {
            if bound.is_none()
                && let Some(d) = default
            {
                return Ok(d);
            }
            // 尝试转换为字典并过滤
            return match bound.cast::<PyDict>() {
                Ok(dict) => {
                    let filtered = PyDict::new(py);
                    dict.iter()
                        .filter(|(_, v)| !v.is_none())
                        .try_for_each(|(k, v)| filtered.set_item(k, v))?;
                    Ok(filtered.into_any())
                }
                Err(_) => Ok(bound), // 不是字典，直接返回
            };
        };
        let Some(default) = default else {
            return Err(pyo3::exceptions::PyAttributeError::new_err(format!(
                "object has no attribute '{key}'"
            )));
        };
        Ok(default)
    }
}

#[pyfunction]
pub fn config_bindings() -> PyResult<ConfigState> {
    let state = ConfigState {
        map: cfg_arc().clone(),
    };
    // pythonize(py, &config)
    Ok(state)
}

// 进程级全局单例（安全）：OnceLock + Arc + RwLock
pub static CONFIG: LazyLock<Arc<RwLock<Config>>> = LazyLock::new(|| {
    Arc::new(RwLock::new(
        Config::builder().streamers(Default::default()).build(),
    ))
});

fn cfg_arc() -> &'static Arc<RwLock<Config>> {
    &CONFIG
}

pub(crate) fn _main(args: &[String]) -> AppResult<()> {
    let cli = match Cli::try_parse_from(args) {
        Ok(res) => res,
        Err(e) => e.exit(),
    };

    let _shadow = Shadow::from_env(env!("CARGO_PKG_VERSION"));
    let local_time = tracing_subscriber::fmt::time::LocalTime::new(format_description!(
        "[year]-[month]-[day] [hour]:[minute]:[second]"
    ));

    let console_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_timer(local_time.clone())
        .with_file(true) // 打印文件名
        .with_line_number(true)
        .with_thread_ids(true);

    // 主程序日志写到工作目录(容器内 /opt)，文件 ds_update.<日期>.log（前缀须与 web
    // 「实时日志-主程序运行日志」一致，ws.rs 会解析 ds_update.* 取最新一个）。
    // 此前误配成 download.log(builder 重复调用，prefix 被第二次覆盖)，导致网页一直「不存在」。
    // 按天滚动 + 只留最近 7 天(max_log_files)，避免日志在小磁盘上无限增长。
    let file_appender = tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("ds_update")
        .filename_suffix("log")
        .max_log_files(7)
        .build("")
        .expect("initializing rolling file appender failed");

    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let (filter_layer, reload_handle) = reload::Layer::new(filter);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_timer(local_time)
        .with_target(true)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .with_ansi(false);

    let subscriber = tracing_subscriber::registry()
        .with(
            console_layer
                .and_then(file_layer)
                .with_filter(tracing_subscriber::filter::filter_fn(legacy_output))
                .with_filter(filter_layer),
        )
        .with(_shadow.layer().map(|layer| layer.filtered()));

    // One scoped subscriber per invocation, including spawned and blocking workers.
    // Repeated wheel calls never replace or duplicate an embedding host's subscriber.
    let command = biliup_cli::observe::lifecycle::command_name(&cli.command);
    shadow::block_on(tracing::Dispatch::new(subscriber), false, async move {
        info!("Tracing initialized with daily rotation");

        let work = async {
            match cli.command {
                Commands::Login => login(cli.user_cookie, cli.proxy.as_deref()).await?,
                Commands::Renew => {
                    renew(cli.user_cookie, cli.proxy.as_deref()).await?;
                }
                Commands::Upload {
                    video_path,
                    config: None,
                    line,
                    limit,
                    studio,
                    submit,
                } => {
                    upload_by_command(
                        studio,
                        cli.user_cookie,
                        video_path,
                        line,
                        limit,
                        submit.unwrap_or(SubmitOption::App),
                        cli.proxy.as_deref(),
                    )
                    .await?
                }
                Commands::Upload {
                    video_path: _,
                    config: Some(config),
                    submit,
                    ..
                } => {
                    upload_by_config(config, cli.user_cookie, submit, cli.proxy.as_deref()).await?;
                }
                Commands::Append {
                    video_path,
                    vid,
                    line,
                    limit,
                    studio: _,
                    submit,
                    replace,
                    execute,
                } => {
                    append(
                        cli.user_cookie,
                        vid,
                        video_path,
                        line,
                        limit,
                        submit.unwrap_or(SubmitOption::App),
                        replace,
                        execute,
                        cli.proxy.as_deref(),
                    )
                    .await?
                }
                Commands::Show { vid } => show(cli.user_cookie, vid, cli.proxy.as_deref()).await?,
                Commands::Comments { vid, sort, pn, ps } => {
                    comments(cli.user_cookie, vid, sort, pn, ps, cli.proxy.as_deref()).await?
                }
                Commands::Reply {
                    vid,
                    rpid,
                    message,
                    execute,
                } => {
                    reply(
                        cli.user_cookie,
                        vid,
                        rpid,
                        message,
                        execute,
                        cli.proxy.as_deref(),
                    )
                    .await?
                }
                Commands::DumpFlv { file_name } => generate_json(file_name)?,
                Commands::Download {
                    url,
                    output,
                    split_size,
                    split_time,
                    stall_timeout,
                } => {
                    biliup_cli::downloader::download(
                        &url,
                        output,
                        split_size,
                        split_time,
                        stall_timeout,
                    )
                    .await?
                }
                Commands::Server {
                    bind,
                    port,
                    auth,
                    config,
                } => {
                    biliup_cli::run((&bind, port), auth, reload_handle, config).await?;
                }
                // 与 biliup-cli 的 main.rs 保持一致。生产走的是 python wheel → stream_gears
                // 这条链路（不是 main.rs），子命令只加在那边的话，这里就编不过——
                // 而 `cargo check -p biliup-cli` 看不到本 crate，问题会一直藏到 Docker 构建才炸。
                Commands::CoverPreview {
                    text,
                    background,
                    output,
                    dim,
                    blur,
                    background_only,
                } => {
                    biliup_cli::cover_preview::cover_preview(
                        &text,
                        background,
                        &output,
                        dim,
                        blur,
                        background_only,
                    )?;
                    let what = if background_only {
                        "背景图"
                    } else {
                        "封面预览"
                    };
                    println!("已生成{what}：{}", output.display());
                }
                Commands::List {
                    is_pubing,
                    pubed,
                    not_pubed,
                    from_page,
                    max_pages,
                } => {
                    list(
                        cli.user_cookie,
                        is_pubing,
                        pubed,
                        not_pubed,
                        cli.proxy.as_deref(),
                        from_page,
                        max_pages,
                    )
                    .await?
                }
                Commands::BackfillLifecycle { database, dry_run } => {
                    let pool = ConnectionManager::new_pool(&database).await?;
                    let summary = run_lifecycle_backfill(&pool, dry_run).await?;
                    info!(
                        processed_sessions = summary.processed_sessions,
                        migrated_rows = summary.migrated_rows,
                        synthetic_rows = summary.synthetic_rows,
                        conflict_rows = summary.conflict_rows,
                        dry_run,
                        "lifecycle backfill finished"
                    );
                }
            };

            Ok(())
        };
        biliup_cli::observe::lifecycle::run(
            biliup_cli::observe::lifecycle::WHEEL_CLI,
            command,
            work,
        )
        .await
    })
    .map_err(|_| {
        biliup_cli::server::errors::AppError::Custom("runtime initialization failed".into())
    })?
}
