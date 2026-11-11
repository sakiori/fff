mod client;
mod cursor;
mod file_search;
mod handshake;
mod healthcheck;
mod output;
mod parent;
mod program;
mod registry;
mod scope;
mod server;
mod service;
mod shared_index;
mod subscription;
mod update_check;
mod view;

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

// Runs at load time, before mimalloc maps its first arena.
#[used]
#[cfg_attr(
    any(target_os = "linux", target_os = "android"),
    unsafe(link_section = ".init_array")
)]
#[cfg_attr(
    target_vendor = "apple",
    unsafe(link_section = "__DATA,__mod_init_func")
)]
#[cfg_attr(windows, unsafe(link_section = ".CRT$XCU"))]
static TUNE_MIMALLOC: extern "C" fn() = fff::tune_mimalloc;

pub const MCP_INSTRUCTIONS: &str = include_str!("instructions.md");

/// FFF MCP Server - a high performance & accuracy file finder for AI code assistants.
#[derive(Parser)]
#[command(name = "fff-mcp", version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("FFF_GIT_HASH"), ")"))]
pub(crate) struct Args {
    /// Base directory to index. Defaults to the current working directory.
    #[arg(value_name = "PATH")]
    base_path: Option<String>,

    #[arg(long, hide = true)]
    service: bool,

    // The build identity the launching client expects of this service.
    #[arg(long, hide = true, requires = "service")]
    service_build: Option<String>,

    /// Report the shared services in the namespace without starting one.
    #[arg(long)]
    service_status: bool,

    /// Stop every shared service in the namespace that has no connected clients.
    #[arg(long, conflicts_with = "service_status")]
    service_stop: bool,

    /// Isolate service discovery and runtime files.
    #[arg(long, env = "FFF_MCP_SERVICE_DIR")]
    service_dir: Option<PathBuf>,

    /// Path to the frecency database.
    #[arg(long = "frecency-db")]
    frecency_db_path: Option<String>,

    /// Path to the query history database.
    #[arg(long = "history-db")]
    #[allow(dead_code)]
    history_db_path: Option<String>,

    /// Path-shape hint for per-session log files.
    /// Each fff-mcp startup writes a fresh sibling file `<stem>+<UTC-timestamp>+<pid>.<ext>`
    /// Defaults to `$XDG_STATE_HOME/fff/fff_mcp.log` (`~/.local/state/fff/fff_mcp.log`),
    /// on Windows to `%LOCALAPPDATA%\fff\fff_mcp.log` (`~\AppData\Local\fff\fff_mcp.log`).
    #[arg(long = "log-file")]
    log_file: Option<PathBuf>,

    /// Log level (e.g. trace, debug, info, warn, error).
    #[arg(long = "log-level")]
    log_level: Option<String>,

    /// Accepted for compatibility: this build never checks for updates.
    #[arg(long = "no-update-check")]
    no_update_check: bool,

    /// Disable eager mmap warmup after the initial scan. Grep results will
    /// still work (files are mmap'd lazily on first access), but the first
    /// search may be slightly slower. Useful on very large repos where the
    /// warmup would consume too many kernel resources.
    #[arg(long = "no-warmup")]
    no_warmup: bool,

    /// Disable the content index built after the initial scan.
    /// This makes grep calls slower but consumes less RAM (recommended to not turn off)
    #[arg(long = "no-content-indexing")]
    no_content_indexing: bool,

    /// Explicitly enable content indexing even when `--no-warmup` is set.
    #[arg(long = "content-indexing")]
    content_indexing: bool,

    /// Disable the background file-system watcher. Files are scanned once
    /// at startup but not monitored for changes.
    #[arg(long = "no-watch")]
    no_watch: bool,

    /// Index with all cores (performance cores on macOS) instead of half of them.
    /// Faster startup on very large repos, but uses more CPU and searches made
    /// while indexing wait for it. Also settable via FFF_FULL_PARALLEL_INDEXING=1.
    #[arg(
        long = "full-parallel-indexing",
        env = "FFF_FULL_PARALLEL_INDEXING",
        num_args = 0..=1,
        default_missing_value = "true",
        default_value_t = false,
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    full_parallel_indexing: bool,

    /// Maximum number of files whose content is kept persistently in memory.
    /// Files beyond this limit are still searchable via temporary mmaps that
    /// are released after each grep. `0` disables persistent caching entirely.
    /// Unset: auto-sized from the scanned file count.
    /// Also settable via the FFF_MAX_CACHED_FILES environment variable.
    #[arg(long = "max-cached-files", env = "FFF_MAX_CACHED_FILES")]
    max_cached_files: Option<usize>,

    /// Follow symlinks during scan and watcher walks. Off by default —
    /// enabling on cyclic symlink layouts can wedge the watcher.
    #[arg(long = "follow-symlinks")]
    follow_symlinks: bool,

    /// Allow indexing the user's home directory. FFF refuses to init in `~`
    /// unless this is set. Also settable via FFF_ENABLE_HOME_SCAN=1.
    #[arg(
        long = "enable-home-scan",
        env = "FFF_ENABLE_HOME_SCAN",
        num_args = 0..=1,
        default_missing_value = "true",
        default_value_t = false,
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    enable_home_scan: bool,

    /// Allow indexing the filesystem root, off by default for the same reason.
    /// Also settable via FFF_ENABLE_ROOT_SCAN=1.
    #[arg(
        long = "enable-root-scan",
        env = "FFF_ENABLE_ROOT_SCAN",
        num_args = 0..=1,
        default_missing_value = "true",
        default_value_t = false,
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    enable_root_scan: bool,

    /// Run a health check and print diagnostic information, then exit.
    #[arg(long = "healthcheck")]
    pub(crate) healthcheck: bool,

    /// Exit the shared service after this many seconds without clients, and a client without a
    /// detectable parent after this many seconds without MCP traffic; 0 disables idle exit.
    #[arg(
        long = "idle-timeout-secs",
        env = "FFF_MCP_IDLE_TIMEOUT_SECS",
        default_value_t = 60 * 60
    )]
    idle_timeout_secs: u64,
}

/// Resolve default paths for the log file.
/// Database paths (frecency, history) must be explicitly provided via flags.
fn resolve_defaults(args: &mut Args) {
    // Ensure parent directories exist for database paths when provided
    for path in [&args.frecency_db_path, &args.history_db_path]
        .into_iter()
        .flatten()
    {
        if let Some(parent) = Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
    }

    if args.log_file.is_none() {
        args.log_file = Some(default_log_file());
    }
}

// Own subdirectory: log rotation read_dir's the parent on every startup, and the
// per-session `<stem>+<ts>+<pid>.log` files otherwise pile up in its root.
fn default_log_file() -> PathBuf {
    fff_service::state_dir().join("fff").join("fff_mcp.log")
}

fn main() -> std::process::ExitCode {
    let args = Args::parse();
    let mut builder = if args.service {
        tokio::runtime::Builder::new_multi_thread()
    } else {
        tokio::runtime::Builder::new_current_thread()
    };
    let result = builder
        .enable_all()
        .build()
        .map_err(Into::into)
        .and_then(|runtime| {
            let result = runtime.block_on(run(args));
            runtime.shutdown_timeout(Duration::from_secs(1));
            result
        });
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(mut args: Args) -> Result<(), Box<dyn std::error::Error>> {
    resolve_defaults(&mut args);

    if args.healthcheck {
        return healthcheck::run_healthcheck(&args);
    }

    // init_tracing takes &str (stable across fff-c/-python/-nvim), so reject a
    // non-UTF-8 --log-file rather than let to_string_lossy retarget the file.
    let log_file = args.log_file.as_deref().unwrap_or(Path::new(""));
    match log_file.to_str() {
        Some(log_file) => {
            if let Err(e) = fff::log::init_tracing(log_file, args.log_level.as_deref(), None) {
                eprintln!("Warning: Failed to init tracing: {}", e);
            }
        }
        None => eprintln!(
            "Warning: --log-file is not valid UTF-8, file logging disabled: {}",
            log_file.display()
        ),
    }

    let build = fff_service::Build::current();
    if args.service {
        if !args.no_update_check {
            update_check::spawn_update_check();
        }
        fff::set_full_parallel_indexing(args.full_parallel_indexing);
        service::run(&args, &build).await?;
    } else {
        client::run(&args, &build).await?;
    }
    Ok(())
}

fn watchdog_interval() -> Duration {
    if cfg!(debug_assertions)
        && let Some(milliseconds) = std::env::var("FFF_MCP_TEST_WATCHDOG_INTERVAL_MS")
            .ok()
            .and_then(|value| value.parse().ok())
    {
        return Duration::from_millis(milliseconds);
    }
    Duration::from_secs(60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_log_file_lives_in_own_dir() {
        let path = default_log_file();
        assert!(path.is_absolute(), "{}", path.display());
        assert_eq!(path.file_name().unwrap(), "fff_mcp.log");
        assert_eq!(path.parent().unwrap().file_name().unwrap(), "fff");
    }
}
