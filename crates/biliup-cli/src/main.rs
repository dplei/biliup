use time::macros::format_description;

use biliup::uploader::util::SubmitOption;
use biliup_cli::cli::{Cli, Commands, expand_path};
use biliup_cli::cover_preview::cover_preview;
use biliup_cli::downloader::{download, generate_json};
use biliup_cli::uploader::{
    append, comments, list, login, renew, reply, show, upload_by_command, upload_by_config,
};

use clap::Parser;

use biliup_cli::server::common::lifecycle_backfill::run_lifecycle_backfill;
use biliup_cli::server::errors::AppResult;
use biliup_cli::server::infrastructure::connection_pool::ConnectionManager;
use biliup_observability::{legacy_output, shadow::Shadow};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, reload};

#[tokio::main]
async fn main() -> AppResult<()> {
    // a builder for `FmtSubscriber`.
    // let subscriber = FmtSubscriber::builder()
    //     // all spans/events with a level higher than TRACE (e.g, debug, info, warn, etc.)
    //     // will be written to stdout.
    //     .with_max_level(Level::INFO)
    //     // completes the builder.
    //     .finish();

    // tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");
    let cli = Cli::parse();

    // use of deprecated function `time::util::local_offset::set_soundness`: no longer needed; TZ is refreshed manually
    // unsafe {
    //     time::util::local_offset::set_soundness(time::util::local_offset::Soundness::Unsound);
    // }

    let timer = tracing_subscriber::fmt::time::LocalTime::new(format_description!(
        "[year]-[month]-[day] [hour]:[minute]:[second]"
    ));

    let console_filter = tracing_subscriber::EnvFilter::new(&cli.rust_log);
    // let (file_filter_layer, file_reload_handle) = reload::Layer::new(file_filter);
    let (console_filter_layer, console_reload_handle) = reload::Layer::new(console_filter);
    let _shadow = Shadow::from_env(env!("CARGO_PKG_VERSION"));
    let subscriber = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_timer(timer)
                .with_filter(tracing_subscriber::filter::filter_fn(legacy_output))
                .with_filter(console_filter_layer),
        )
        .with(_shadow.layer().map(|layer| layer.filtered()));
    // A standalone binary owns one global subscriber; a conflicting host must not panic.
    if subscriber.try_init().is_err() {
        eprintln!("observability: subscriber_already_installed; retaining host subscriber");
    }

    let user_cookie = expand_path(cli.user_cookie);

    // The whole command is one run of this process; its result is the run's result.
    let command = biliup_cli::observe::lifecycle::command_name(&cli.command);
    let work = async {
        match cli.command {
            Commands::Login => login(user_cookie, cli.proxy.as_deref()).await?,
            Commands::Renew => {
                renew(user_cookie, cli.proxy.as_deref()).await?;
            }
            Commands::Upload {
                video_path,
                config: None,
                line,
                limit,
                studio,
                submit,
            } => {
                let video_path: Vec<_> = video_path.into_iter().map(expand_path).collect();
                upload_by_command(
                    studio,
                    user_cookie,
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
                let config = expand_path(config);
                upload_by_config(config, user_cookie, submit, cli.proxy.as_deref()).await?;
            }
            Commands::Append {
                video_path,
                vid,
                line,
                limit,
                studio: _,
                submit,
            } => {
                let video_path: Vec<_> = video_path.into_iter().map(expand_path).collect();
                append(
                    user_cookie,
                    vid,
                    video_path,
                    line,
                    limit,
                    submit.unwrap_or(SubmitOption::App),
                    cli.proxy.as_deref(),
                )
                .await?
            }
            Commands::Show { vid } => show(user_cookie, vid, cli.proxy.as_deref()).await?,
            Commands::Comments { vid, sort, pn, ps } => {
                comments(user_cookie, vid, sort, pn, ps, cli.proxy.as_deref()).await?
            }
            Commands::Reply {
                vid,
                rpid,
                message,
                execute,
            } => {
                reply(
                    user_cookie,
                    vid,
                    rpid,
                    message,
                    execute,
                    cli.proxy.as_deref(),
                )
                .await?
            }
            Commands::DumpFlv { file_name } => {
                let file_name = expand_path(file_name);
                generate_json(file_name)?
            }
            Commands::Download {
                url,
                output,
                split_size,
                split_time,
                stall_timeout,
            } => download(&url, output, split_size, split_time, stall_timeout).await?,
            Commands::Server {
                bind,
                port,
                auth,
                config,
            } => biliup_cli::run((&bind, port), auth, console_reload_handle, config).await?,
            Commands::CoverPreview {
                text,
                background,
                output,
                dim,
                blur,
                background_only,
            } => {
                cover_preview(&text, background, &output, dim, blur, background_only)?;
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
                    user_cookie,
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
                println!(
                    "回填{}：会话 {}，迁移行 {}，synthetic 行 {}，冲突行 {}",
                    if dry_run { "预演" } else { "完成" },
                    summary.processed_sessions,
                    summary.migrated_rows,
                    summary.synthetic_rows,
                    summary.conflict_rows
                );
                if summary.conflict_rows > 0 {
                    println!(
                        "存在冲突行，相关会话已被完整性闸门阻止投稿；\
                     请查询 upload_lifecycle_backfill_event 后人工处理。"
                    );
                }
            }
        };
        Ok(())
    };
    biliup_cli::observe::lifecycle::run(biliup_cli::observe::lifecycle::RUST_CLI, command, work)
        .await
}
