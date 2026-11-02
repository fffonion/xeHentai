//! Application entry point: the reference `xeH.py` + `xeHentai/cli.py`
//! `start()`/`main()` startup.
//!
//! Order (reference `start`/`main`):
//!   load configuration -> parse the command line -> build the app ->
//!   optional POSIX daemon fork -> apply CLI config -> log start -> cookie and
//!   login -> interactive prompts -> submit URLs -> run the real task loop and
//!   optional RPC server -> stop on URL completion, signals or RPC end.

rust_i18n::i18n!("locales", fallback = "en_us");

mod cli;
mod config;
mod r#const;
mod core;
mod filters;
mod i18n;
mod proxy;
mod rpc;
mod task;
mod updater;
mod util;
mod worker;

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use cli::{Opts, Parsed};
use config::Config;
use core::App;
use i18n::Locale;
use util::logger::Logger;

fn main() {
    let root = std::env::current_dir().unwrap_or_else(|_| ".".into());
    std::process::exit(run(&root));
}

/// Startup with the process' own arguments.
pub fn run(root: &Path) -> i32 {
    run_with_args(root, std::env::args_os())
}

/// Startup with an explicit argv (`argv[0]` is the program name).
pub fn run_with_args<I, T>(root: &Path, argv: I) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let locale = i18n::detect_locale();
    let loaded = match Config::load(root) {
        Ok(loaded) => loaded,
        Err(error) => {
            eprintln!("xeHentai: configuration error: {error}");
            return 1;
        }
    };
    // The reference parses the command line before constructing the app; help,
    // version and usage errors therefore never touch the download pipeline.
    let opts = match cli::parse_from(argv, &loaded.config, locale) {
        Ok(Parsed::Help(text)) | Ok(Parsed::Version(text)) => {
            print!("{text}");
            use std::io::Write;
            let _ = std::io::stdout().flush();
            return 0;
        }
        Ok(Parsed::Opts(opts)) => opts,
        Err(error) => {
            eprint!("{error}");
            return 2;
        }
    };
    let logger = Logger::new();
    // FILEPATH in the reference is the application directory; the legacy
    // cookie file lives next to the program, not in the working directory.
    let filepath = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| root.to_path_buf());
    let mut app = match App::new(loaded.config, logger, root, &filepath) {
        Ok(app) => app,
        Err(error) => {
            eprintln!("xeHentai: session error: {error}");
            return 1;
        }
    };
    // The daemon fork must happen once, before any Tokio runtime or worker
    // thread exists; the child then owns the whole real startup.
    if opts.daemon {
        if opts.interactive {
            let message = i18n::render(locale, "XEH_OPT_IGNORING_I", &[])
                .unwrap_or_else(|| "ignoring -i option in daemon mode".to_string());
            let _ = app.logger_mut().warning(&message);
        }
        match daemon_fork() {
            Daemon::Parent(child) => {
                let message = i18n::render(locale, "XEH_DAEMON_START", &[&child.to_string()])
                    .unwrap_or_else(|| format!("daemon start at PID {child}"));
                let _ = app.logger_mut().info(&message);
                return 0;
            }
            Daemon::Child => {}
            Daemon::Unsupported(name) => {
                let message = i18n::render(locale, "XEH_PLATFORM_NO_DAEMON", &[name])
                    .unwrap_or_else(|| format!("daemon mode is not supported on platform: {name}"));
                let _ = app.logger_mut().error(&message);
                return 0;
            }
            Daemon::Failed(error) => {
                eprintln!("xeHentai: cannot fork daemon: {error}");
                return 1;
            }
        }
    }
    run_application(app, root, &opts, locale)
}

/// Resolve `rss/` and `locales/` from the program directory when the release
/// ships them next to the executable, else from the working directory. Never
/// from a build-time absolute path, so binaries stay cwd-independent.
pub fn resource_root(root: &Path) -> PathBuf {
    let exe_root = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf));
    exe_root
        .filter(|dir| dir.join("rss").is_dir())
        .unwrap_or_else(|| root.to_path_buf())
}

/// Run the real application: config update, logging, cookies/login, prompts,
/// task submission and the supervised task loop with its optional RPC server.
fn run_application(app: App, root: &Path, opts: &Opts, locale: Locale) -> i32 {
    let resource = resource_root(root);
    let tokio_runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("xeHentai: cannot start runtime: {error}");
            return 1;
        }
    };
    let local = tokio::task::LocalSet::new();
    let code = tokio_runtime
        .block_on(local.run_until(async move { application(app, &resource, opts, locale).await }));
    // A cancelled prompt leaves the stdin reader blocked in the runtime's
    // blocking pool; the reference also leaves through a bounded exit, so the
    // runtime is not waited on here.
    tokio_runtime.shutdown_background();
    code
}

async fn application(mut app: App, resource: &Path, opts: &Opts, locale: Locale) -> i32 {
    if let Err(error) = app.update_config(config_patch(opts)) {
        eprintln!("xeHentai: configuration error: {error}");
        return 1;
    }
    let started = i18n::render(locale, "XEH_STARTED", &[r#const::VERSION_DISPLAY])
        .unwrap_or_else(|| format!("xeHentai {} started.", r#const::VERSION_DISPLAY));
    let _ = app.logger_mut().info(&started);
    if let Some(cookie) = &opts.cookie {
        app.set_cookie(cookie);
    }
    let mut urls = opts.urls.clone();
    let active_interactive = opts.interactive && !opts.daemon;
    let needs_login = opts.username.is_some() && opts.key.is_some() && !app.has_login();
    // An active interactive run always needs the filters module (the login
    // prompt uses it when logged out, and the download prompts need it even
    // when a valid cookie was restored); credential login needs it too.
    let mut filters = if needs_login || active_interactive {
        match filters::Filters::load(resource) {
            Ok(filters) => Some(filters),
            Err(error) => {
                eprintln!("xeHentai: filter error: {error}");
                return 1;
            }
        }
    } else {
        None
    };
    if needs_login && let Some(active) = filters.as_mut() {
        let username = opts.username.clone().unwrap_or_default();
        let key = opts.key.clone().unwrap_or_default();
        if let Err(error) = app.login_exhentai(active, &username, &key).await {
            let _ = app.logger_mut().error(&error.to_string());
        }
    }
    if active_interactive {
        let Some(active) = filters.as_mut() else {
            eprintln!("xeHentai: filter error: interactive login needs the filters module");
            return 1;
        };
        let mut interrupt = match signal_stream() {
            Ok(signals) => signals,
            Err(error) => {
                eprintln!("xeHentai: cannot listen for interrupts: {error}");
                return 1;
            }
        };
        match cli::interactive(&mut app, active, locale, &mut interrupt).await {
            Ok(Some(result)) => {
                urls = result.urls.clone();
                if let Err(error) = app.update_config(result.patch()) {
                    eprintln!("xeHentai: configuration error: {error}");
                    return 1;
                }
            }
            Ok(None) => {}
            Err(cli::PromptError::Interrupted) => {
                let message = i18n::render(locale, "XEH_CLEANUP", &[])
                    .unwrap_or_else(|| "cleaning up...".to_string());
                let _ = app.logger_mut().info(&message);
                let _ = app.save_session();
                app.logger_mut().cleanup();
                return 0;
            }
            Err(cli::PromptError::Io(error)) => {
                eprintln!("xeHentai: cannot read input: {error}");
                return 1;
            }
        }
    }
    for url in &urls {
        let _ = app.add_task(url, Map::new());
    }
    let runtime = core::Runtime::new(app, resource.to_path_buf());
    run_supervised(runtime, urls, locale, resource.to_path_buf()).await
}

/// The supervised real run: task loop, optional RPC server, URL-completion
/// auto-exit and the reference signal lifecycle.
async fn run_supervised(
    runtime: core::Runtime,
    urls: Vec<String>,
    locale: Locale,
    resource: PathBuf,
) -> i32 {
    let (task_loop, handle) = runtime.spawn();
    // The reference `cli.start` starts the updater as a daemon thread before
    // the task loop; this owned background task is retired by the same
    // supervisor on URL completion or the first signal.
    let update_task = spawn_update_check(handle.clone(), resource, locale);
    let rpc_enabled = handle.borrow_app(|app| app.cfg.rpc_port.is_some());
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let rpc = if rpc_enabled {
        Some(handle.serve_rpc(shutdown_rx, rpc::PRODUCTION_STATIC_ORIGIN.to_string()))
    } else {
        None
    };
    let rpc_future = async move {
        match rpc {
            Some(future) => Some(future.await),
            None => std::future::pending::<Option<Result<(), rpc::ServerError>>>().await,
        }
    };
    let mut interrupt = match signal_stream() {
        Ok(signals) => signals,
        Err(error) => {
            eprintln!("xeHentai: cannot listen for interrupts: {error}");
            return 1;
        }
    };
    let mut terminate = match terminate_stream() {
        Ok(signals) => signals,
        Err(error) => {
            eprintln!("xeHentai: cannot listen for termination: {error}");
            return 1;
        }
    };
    tokio::pin!(task_loop);
    tokio::pin!(rpc_future);
    let has_urls = !urls.is_empty();
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut rpc_result = None;
    let stopped = loop {
        tokio::select! {
            result = &mut rpc_future => {
                rpc_result = result;
                break Stop::RpcDone;
            }
            _ = &mut task_loop => break Stop::LoopFinished,
            _ = interrupt.recv() => break Stop::Signal,
            _ = terminate_signal(terminate.as_mut()) => break Stop::Signal,
            _ = tick.tick(), if has_urls => {
                // Reference `main`: with URLs, leave as soon as no submitted
                // task is still in [WAITING, FINISHED) -- failures included.
                if !handle.borrow_app(has_active_task) {
                    break Stop::AutoExit;
                }
            }
        }
    };
    if let Some(task) = update_task {
        task.abort();
        let _ = task.await;
    }
    let _ = shutdown_tx.send(true);
    match stopped {
        Stop::AutoExit => {
            handle.borrow_app_mut(|app| app.request_exit(r#const::XEH_STATE_SOFT_EXIT));
            let _ = task_loop.await;
            if rpc_enabled {
                let _ = rpc_future.await;
            }
            0
        }
        Stop::Signal => {
            let message = i18n::render(locale, "XEH_CLEANUP", &[])
                .unwrap_or_else(|| "cleaning up...".to_string());
            handle.borrow_app_mut(|app| {
                let _ = app.logger_mut().info(&message);
            });
            // First interrupt: real graceful stop through the AppHandle owner
            // (cancel every worker, drain, flush the session). A second
            // interrupt is the reference's bounded `os._exit(0)`: it must not
            // await the task loop or the drain (they can be blocked on native
            // work) and must leave without a Tokio runtime drop.
            let drain = handle.shutdown();
            tokio::pin!(drain);
            tokio::select! {
                _ = &mut drain => {}
                _ = interrupt.recv() => std::process::exit(0),
                _ = terminate_signal(terminate.as_mut()) => std::process::exit(0),
            }
            let _ = task_loop.await;
            if rpc_enabled {
                let _ = rpc_future.await;
            }
            0
        }
        Stop::LoopFinished => {
            if rpc_enabled {
                let _ = rpc_future.await;
            }
            0
        }
        Stop::RpcDone => {
            // A bind/serve error still retires the task loop through its
            // existing owner path before returning the real status.
            let _ = handle.shutdown().await;
            let _ = task_loop.await;
            match rpc_result {
                Some(Ok(())) => 0,
                Some(Err(error)) => {
                    eprintln!("xeHentai: RPC server error: {error}");
                    1
                }
                None => 0,
            }
        }
    }
}

enum Stop {
    AutoExit,
    Signal,
    LoopFinished,
    RpcDone,
}

fn has_active_task(app: &App) -> bool {
    app.all_task_guids().iter().any(|guid| {
        app.task(guid).is_some_and(|task| {
            r#const::TASK_STATE_WAITING <= task.state && task.state < r#const::TASK_STATE_FINISHED
        })
    })
}

/// The reference `cli.start` gate is in the caller: `off` never constructs the
/// update module or a request. Other modes run in one owned background local
/// task, so the check never blocks downloads, prompts or server startup.
fn spawn_update_check(
    app: core::AppHandle,
    resource: PathBuf,
    locale: Locale,
) -> Option<tokio::task::JoinHandle<()>> {
    let mode = app.borrow_app(|app| app.cfg.auto_update.clone());
    if mode == "off" {
        return None;
    }
    let beta = app.borrow_app(|app| app.cfg.update_beta_channel);
    Some(tokio::task::spawn_local(async move {
        let mut updater = match updater::Updater::load(&resource) {
            Ok(updater) => updater,
            Err(error) => {
                log_update_failure(&app, locale, &error.to_string());
                return;
            }
        };
        let channel = updater::render_channel(locale, beta);
        app.borrow_app_mut(|app| {
            let _ = app.logger_mut().debug(&channel);
        });
        // This build carries no applied update id yet; the package loader in
        // the following scope supplies the applied id on a later startup.
        let applied_update_id = None;
        let user_agent = format!("xeHentai/{}", r#const::VERSION_DISPLAY);
        match updater
            .check(&mode, beta, applied_update_id, &user_agent)
            .await
        {
            Ok(report) => {
                for event in &report.events {
                    log_update_event(&app, locale, event);
                }
            }
            Err(error) => log_update_failure(&app, locale, &error.to_string()),
        }
    }))
}

fn log_update_event(app: &core::AppHandle, locale: Locale, event: &updater::LogEvent) {
    let message = updater::render_event(locale, event);
    app.borrow_app_mut(|app| {
        let logger = app.logger_mut();
        let _ = match event.level.as_str() {
            "debug" => logger.debug(&message),
            "warning" => logger.warning(&message),
            "error" => logger.error(&message),
            _ => logger.info(&message),
        };
    });
}

fn log_update_failure(app: &core::AppHandle, locale: Locale, detail: &str) {
    let message = i18n::render(locale, "UPDATE_FAILED", &[detail])
        .unwrap_or_else(|| format!("Failure when updating program: {detail}"));
    app.borrow_app_mut(|app| {
        let _ = app.logger_mut().warning(&message);
    });
}

/// The interrupt stream behind `interactive()` and the supervisor: the Unix
/// SIGINT stream on Unix and the native Ctrl-C listener on Windows (the daemon
/// remains Unix-only).
#[cfg(unix)]
type SignalStream = tokio::signal::unix::Signal;
#[cfg(windows)]
type SignalStream = tokio::signal::windows::CtrlC;

fn signal_stream() -> std::io::Result<SignalStream> {
    #[cfg(unix)]
    {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
    }
    #[cfg(windows)]
    {
        tokio::signal::windows::ctrl_c()
    }
}

/// SIGTERM equivalent for the supervisor. There is no terminate stream outside
/// Unix, so only the interrupt stream is wired there.
fn terminate_stream() -> std::io::Result<Option<SignalStream>> {
    #[cfg(unix)]
    {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).map(Some)
    }
    #[cfg(not(unix))]
    {
        Ok(None)
    }
}

/// Wait for the terminate stream; stay pending forever when the platform has
/// none. Both stream kinds expose the same `recv() -> Option<()>`.
async fn terminate_signal(signals: Option<&mut SignalStream>) -> Option<()> {
    match signals {
        Some(signals) => signals.recv().await,
        None => std::future::pending().await,
    }
}

/// Project the parsed command line onto the reference `update_config(**vars)`
/// keys. `ignored_errors` is only sent when `-f` was given (the reference
/// skips a `None` value) and `download_range` is always sent, because the
/// reference namespace default is `None` even when the file configures one.
fn config_patch(opts: &Opts) -> Map<String, Value> {
    let mut patch = Map::new();
    patch.insert("daemon".into(), json!(opts.daemon));
    patch.insert("dir".into(), json!(opts.dir));
    patch.insert("download_ori".into(), json!(opts.download_ori));
    patch.insert("jpn_title".into(), json!(opts.jpn_title));
    patch.insert("rename_ori".into(), json!(opts.rename_ori));
    patch.insert("proxy".into(), json!(opts.proxy));
    patch.insert("proxy_image".into(), json!(opts.proxy_image));
    patch.insert("proxy_image_only".into(), json!(opts.proxy_image_only));
    patch.insert("rpc_interface".into(), json!(opts.rpc_interface));
    patch.insert("rpc_port".into(), json!(opts.rpc_port));
    patch.insert("rpc_secret".into(), json!(opts.rpc_secret));
    patch.insert("rpc_open_browser".into(), json!(opts.rpc_open_browser));
    patch.insert("make_archive".into(), json!(opts.make_archive));
    patch.insert(
        "download_range".into(),
        match &opts.download_range {
            Some(ranges) => json!(ranges),
            None => Value::Null,
        },
    );
    patch.insert(
        "download_thread_cnt".into(),
        json!(opts.download_thread_cnt),
    );
    patch.insert("download_timeout".into(), json!(opts.download_timeout));
    patch.insert(
        "low_speed_threshold".into(),
        json!(opts.low_speed_threshold),
    );
    if let Some(ignored) = &opts.ignored_errors {
        patch.insert("ignored_errors".into(), json!(ignored));
    }
    patch.insert("log_path".into(), json!(opts.log_path));
    patch.insert("log_verbose".into(), json!(opts.verbose));
    patch.insert("delete_task_files".into(), json!(opts.delete_task_files));
    patch.insert("auto_update".into(), json!(opts.auto_update));
    patch.insert(
        "update_beta_channel".into(),
        json!(opts.update_beta_channel),
    );
    patch
}

// MARK: - POSIX daemon

enum Daemon {
    Parent(i32),
    Child,
    // Only the non-Unix `daemon_fork` constructs this; the POSIX path reports
    // the localized "not supported" message through `Failed` instead.
    #[allow(dead_code)]
    Unsupported(&'static str),
    Failed(String),
}

#[cfg(unix)]
fn daemon_fork() -> Daemon {
    use nix::unistd::{ForkResult, fork};
    match unsafe { fork() } {
        Ok(ForkResult::Parent { child }) => Daemon::Parent(child.as_raw()),
        Ok(ForkResult::Child) => {
            // Reference child: close stdin, send stdout/stderr to /dev/null.
            let child = (|| -> std::io::Result<()> {
                let devnull = std::fs::OpenOptions::new().write(true).open("/dev/null")?;
                let to_io =
                    |error: nix::errno::Errno| std::io::Error::from_raw_os_error(error as i32);
                nix::unistd::dup2_stdout(&devnull).map_err(to_io)?;
                nix::unistd::dup2_stderr(&devnull).map_err(to_io)?;
                let _ = nix::unistd::close(0);
                Ok(())
            })();
            match child {
                Ok(()) => Daemon::Child,
                Err(error) => Daemon::Failed(error.to_string()),
            }
        }
        Err(error) => Daemon::Failed(error.to_string()),
    }
}

#[cfg(not(unix))]
fn daemon_fork() -> Daemon {
    Daemon::Unsupported(std::env::consts::OS)
}

#[cfg(test)]
mod tests;
