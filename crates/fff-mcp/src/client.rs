use std::cell::Cell;
use std::io;
use std::time::Duration;

use fff_service::Build;
use fff_service::client::{
    CONNECT_TIMEOUT, ClientError, Connection, Launcher, open_session, query_all,
};
use fff_service::namespace::Namespace;
use fff_service::protocol::{IndexOptions, Request};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::Instant;

// Grace period for logs after the parent process exits.
const PARENT_EXIT_FLUSH: Duration = Duration::from_millis(250);
const COPY_BUFFER_BYTES: usize = 64 * 1024;

pub(crate) async fn run(args: &crate::Args, build: &Build) -> Result<(), ClientError> {
    let namespace = Namespace::open(&crate::service::directory(args))?;
    if args.service_status {
        let report = query_all(&namespace, build, || Request::Status).await?;
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(io::Error::from)?
        );
        return Ok(());
    }
    if args.service_stop {
        let report = query_all(&namespace, build, || Request::Stop).await?;
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(io::Error::from)?
        );
        return match report.refused() {
            0 => Ok(()),
            refused => Err(ClientError::StopRefused(refused)),
        };
    }
    let parent = parent_watcher();
    let scope = fff::path_utils::canonicalize(args.base_path.as_deref().unwrap_or("."))?;
    let hello = Request::Session {
        scope,
        options: index_options(args)?,
    };
    let launcher = Launcher {
        executable: std::env::current_exe()?,
        idle_timeout_secs: args.idle_timeout_secs,
        log_file: args.log_file.clone(),
        log_level: args.log_level.clone(),
        full_parallel_indexing: Some(args.full_parallel_indexing),
    };
    let (connection, warnings) =
        open_session(&namespace, build, &hello, &launcher, CONNECT_TIMEOUT).await?;
    for warning in warnings {
        eprintln!("{warning}");
    }
    proxy(connection, parent, args.idle_timeout_secs).await
}

fn parent_watcher() -> Option<crate::parent::ParentWatcher> {
    // Tests simulate a client whose parent cannot be observed.
    if cfg!(debug_assertions) && std::env::var_os("FFF_MCP_TEST_NO_PARENT").is_some() {
        return None;
    }
    crate::parent::ParentWatcher::new()
}

fn index_options(args: &crate::Args) -> io::Result<IndexOptions> {
    Ok(IndexOptions {
        warmup: !args.no_warmup,
        content_indexing: args.content_indexing || (!args.no_content_indexing && !args.no_warmup),
        watch: !args.no_watch,
        follow_symlinks: args.follow_symlinks,
        max_cached_files: args.max_cached_files,
        frecency_db: args
            .frecency_db_path
            .as_deref()
            .map(fff_service::stable_database_path)
            .transpose()?,
        enable_home_scan: args.enable_home_scan,
        enable_root_scan: args.enable_root_scan,
        extra_ignore: Vec::new(),
        disable_git: false,
        disable_ignore_files: false,
    })
}

async fn proxy(
    (mut recv, mut send): Connection,
    parent: Option<crate::parent::ParentWatcher>,
    idle_timeout_secs: u64,
) -> Result<(), ClientError> {
    let last_traffic = Cell::new(Instant::now());
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    match &parent {
        Some(parent) => tracing::info!(pid = parent.parent_pid(), "Watching client parent"),
        None => tracing::warn!(
            "Parent process liveness detection unavailable; idle timeout will exit unconditionally"
        ),
    }
    let upload = async {
        let mut buffer = vec![0; COPY_BUFFER_BYTES];
        loop {
            let read = stdin.read(&mut buffer).await?;
            if read == 0 {
                return Ok::<_, ClientError>(());
            }
            send.write_all(&buffer[..read]).await?;
            last_traffic.set(Instant::now());
        }
    };
    let download = async {
        let mut buffer = vec![0; COPY_BUFFER_BYTES];
        loop {
            let read = recv.read(&mut buffer).await?;
            if read == 0 {
                stdout.flush().await?;
                return Err::<(), _>(ClientError::Disconnected);
            }
            stdout.write_all(&buffer[..read]).await?;
            stdout.flush().await?;
            last_traffic.set(Instant::now());
        }
    };
    // A live parent owns the client's lifetime; without one, idle traffic ends it.
    let lifetime = async {
        let mut interval = tokio::time::interval(crate::watchdog_interval());
        let idle = Duration::from_secs(idle_timeout_secs);
        loop {
            interval.tick().await;
            match &parent {
                Some(parent) if !parent.parent_alive() => {
                    tracing::info!(
                        "Parent process (pid {}) exited, shutting down",
                        parent.parent_pid()
                    );
                    tokio::time::sleep(PARENT_EXIT_FLUSH).await;
                    return;
                }
                None if idle_timeout_secs > 0 && last_traffic.get().elapsed() >= idle => {
                    tracing::info!(idle_timeout_secs, "Exiting due to inactivity");
                    return;
                }
                _ => {}
            }
        }
    };
    tokio::select! {
        result = upload => result,
        result = download => result,
        _ = lifetime => Ok(()),
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}
