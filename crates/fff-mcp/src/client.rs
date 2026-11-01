use std::cell::Cell;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::process::Stdio;
use std::time::Duration;

use interprocess::local_socket::tokio::{RecvHalf, SendHalf, Stream, prelude::*};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::time::Instant;

use crate::namespace::Namespace;
use crate::protocol::{
    Endpoint, Hello, MessageError, PROTOCOL, Reply, Request, ServiceError, ServiceStatus,
    read_message, send_message,
};
use crate::service::{Build, ServiceFiles};
use crate::shared_index::IndexOptions;

// Overall budget for finding or starting a service.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(90);
// Time a service has to answer a hello.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const STARTUP_POLL: Duration = Duration::from_millis(50);
const RETRY_BACKOFF: Duration = Duration::from_millis(100);
// Grace period for logs after the parent process exits.
const PARENT_EXIT_FLUSH: Duration = Duration::from_millis(250);
const STARTUP_ERROR_BYTES: u64 = 4096;
const COPY_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ClientError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error("unexpected reply from the shared service")]
    UnexpectedReply,
    #[error("shared service failed to start ({status}){}", detail_suffix(.detail))]
    StartupFailed { status: String, detail: String },
    #[error("no shared service became available within {0:?}")]
    Unavailable(Duration),
    #[error("{} of the running services refused to stop; disconnect their clients first", .0)]
    StopRefused(usize),
    #[error("shared service disconnected")]
    Disconnected,
}

type Connection = (BufReader<RecvHalf>, SendHalf);

enum Attempt {
    Connected(Connection, Vec<String>),
    Retry(String),
    Failed(ClientError),
}

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
        let refused = report.services.iter().filter(|s| s.refused).count();
        return if refused == 0 {
            Ok(())
        } else {
            Err(ClientError::StopRefused(refused))
        };
    }
    let parent = parent_watcher();
    let scope = fff::path_utils::canonicalize(args.base_path.as_deref().unwrap_or("."))?;
    let hello = Request::Session {
        scope,
        options: index_options(args)?,
    };
    let (connection, warnings) =
        tokio::time::timeout(CONNECT_TIMEOUT, connect(args, &namespace, build, &hello))
            .await
            .map_err(|_| ClientError::Unavailable(CONNECT_TIMEOUT))??;
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
            .map(stable_database_path)
            .transpose()?,
        enable_home_scan: args.enable_home_scan,
        enable_root_scan: args.enable_root_scan,
    })
}

// The database may not exist yet, so only its directory is resolved; the spelling must not
// change once the first index creates it.
fn stable_database_path(path: &str) -> io::Result<String> {
    let path = std::path::absolute(path)?;
    let resolved = match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => fff::path_utils::canonicalize(parent)
            .map(|parent| parent.join(name))
            .ok(),
        _ => None,
    };
    Ok(resolved.unwrap_or(path).to_string_lossy().into_owned())
}

// Any failure before `ready` is retried through the startup lock, which also recovers
// stale discovery state and services that are shutting down.
async fn connect(
    args: &crate::Args,
    namespace: &Namespace,
    build: &Build,
    request: &Request,
) -> Result<(Connection, Vec<String>), ClientError> {
    let files = build.files();
    loop {
        match attempt(namespace, &files, build, request).await {
            Attempt::Connected(connection, warnings) => return Ok((connection, warnings)),
            Attempt::Failed(error) => return Err(error),
            Attempt::Retry(reason) => tracing::debug!(reason, "Shared service not ready"),
        }
        let startup = lock_startup(namespace, &files).await?;
        match attempt(namespace, &files, build, request).await {
            Attempt::Connected(connection, warnings) => return Ok((connection, warnings)),
            Attempt::Failed(error) => return Err(error),
            Attempt::Retry(_) => {}
        }
        if owner_is_free(namespace, &files)? {
            namespace.remove_file(&files.endpoint())?;
            let mut child = spawn_service(args, namespace, &files)?;
            loop {
                match attempt(namespace, &files, build, request).await {
                    Attempt::Connected(connection, warnings) => return Ok((connection, warnings)),
                    Attempt::Failed(error) => return Err(error),
                    Attempt::Retry(_) => {}
                }
                if let Some(status) = child.try_wait()? {
                    return Err(ClientError::StartupFailed {
                        status: status.to_string(),
                        detail: startup_error(namespace, &files),
                    });
                }
                tokio::time::sleep(STARTUP_POLL).await;
            }
        }
        // A live service owns the namespace but did not answer; it is starting or exiting.
        drop(startup);
        tokio::time::sleep(RETRY_BACKOFF).await;
    }
}

async fn attempt(
    namespace: &Namespace,
    files: &ServiceFiles,
    build: &Build,
    request: &Request,
) -> Attempt {
    let endpoint = match read_endpoint(namespace, &files.endpoint()) {
        Ok(endpoint) if endpoint.build == build.id() => endpoint,
        Ok(_) => return Attempt::Retry("endpoint belongs to another build".into()),
        Err(error) => return Attempt::Retry(error.to_string()),
    };
    match exchange(&endpoint, request).await {
        Ok((Reply::Ready { warnings }, connection)) => Attempt::Connected(connection, warnings),
        Ok((Reply::Error { error }, _)) if error.is_retryable() => {
            Attempt::Retry(error.to_string())
        }
        Ok((Reply::Error { error }, _)) => Attempt::Failed(error.into()),
        Ok(_) => Attempt::Failed(ClientError::UnexpectedReply),
        Err(ExchangeError::Untrusted(error)) => Attempt::Failed(error.into()),
        Err(ExchangeError::Transient(error)) => Attempt::Retry(error),
    }
}

enum ExchangeError {
    Untrusted(io::Error),
    Transient(String),
}

async fn exchange(
    endpoint: &Endpoint,
    request: &Request,
) -> Result<(Reply, Connection), ExchangeError> {
    let transient = |error: &dyn std::fmt::Display| ExchangeError::Transient(error.to_string());
    let name = crate::service::socket_name(&endpoint.name).map_err(|e| transient(&e))?;
    let stream = Stream::connect(name).await.map_err(|e| transient(&e))?;
    crate::namespace::verify_peer(&stream).map_err(ExchangeError::Untrusted)?;
    let (recv, mut send) = stream.split();
    let mut recv = BufReader::new(recv);
    let hello = Hello {
        token: endpoint.token.clone(),
        protocol: PROTOCOL,
        request: request.clone(),
    };
    send_message(&mut send, &hello)
        .await
        .map_err(|e| transient(&e))?;
    let reply = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_message::<Reply, _>(&mut recv))
        .await
        .map_err(|e| transient(&e))?
        .map_err(|e: MessageError| transient(&e))?;
    Ok((reply, (recv, send)))
}

fn read_endpoint(namespace: &Namespace, name: &str) -> io::Result<Endpoint> {
    Ok(serde_json::from_reader(namespace.open_file(name, false)?)?)
}

async fn lock_startup(namespace: &Namespace, files: &ServiceFiles) -> io::Result<File> {
    let name = files.startup_lock();
    let startup = namespace.open_file(&name, true)?;
    tokio::task::spawn_blocking(move || startup.lock().map(|()| startup))
        .await
        .map_err(io::Error::other)?
        .map_err(|error| namespace.error("lock", &name, error))
}

fn owner_is_free(namespace: &Namespace, files: &ServiceFiles) -> io::Result<bool> {
    let name = files.owner_lock();
    let owner = namespace.open_file(&name, true)?;
    match owner.try_lock() {
        Ok(()) => Ok(true),
        Err(std::fs::TryLockError::WouldBlock) => Ok(false),
        Err(std::fs::TryLockError::Error(error)) => Err(namespace.error("lock", &name, error)),
    }
}

fn spawn_service(
    args: &crate::Args,
    namespace: &Namespace,
    files: &ServiceFiles,
) -> io::Result<tokio::process::Child> {
    let stderr = namespace.open_file(&files.stderr(), true)?;
    stderr.set_len(0)?;
    let mut command = tokio::process::Command::new(std::env::current_exe()?);
    command
        .args(["--service", "--service-dir"])
        .arg(namespace.path())
        .arg("--idle-timeout-secs")
        .arg(args.idle_timeout_secs.to_string())
        .current_dir(namespace.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr));
    if args.no_update_check {
        command.arg("--no-update-check");
    }
    if let Some(level) = &args.log_level {
        command.arg("--log-level").arg(level);
    }
    if let Some(log) = &args.log_file {
        command.arg("--log-file").arg(log);
    }
    detach(&mut command)
}

#[cfg(unix)]
fn detach(command: &mut tokio::process::Command) -> io::Result<tokio::process::Child> {
    command.process_group(0);
    command.spawn()
}

// The service must outlive the client's console, process group and job object.
#[cfg(windows)]
fn detach(command: &mut tokio::process::Command) -> io::Result<tokio::process::Child> {
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, HANDLE_FLAG_INHERIT, SetHandleInformation,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS,
    };
    // Keep the host's stdio pipes out of the long-lived service.
    for handle in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        unsafe {
            let handle = GetStdHandle(handle);
            if !handle.is_null() {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
    // DETACHED_PROCESS already implies no console window.
    let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    command.creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB);
    match command.spawn() {
        // The client's job object may forbid breakaway; the service then stays in the job.
        Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => {
            command.creation_flags(flags);
            command.spawn()
        }
        result => result,
    }
}

fn startup_error(namespace: &Namespace, files: &ServiceFiles) -> String {
    let read = || -> io::Result<String> {
        let mut file = namespace.open_file(&files.stderr(), false)?;
        let length = file.metadata()?.len();
        file.seek(SeekFrom::Start(length.saturating_sub(STARTUP_ERROR_BYTES)))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).trim().to_owned())
    };
    read().unwrap_or_else(|error| format!("service output unavailable: {error}"))
}

fn detail_suffix(detail: &str) -> String {
    if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    }
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

#[derive(Serialize)]
struct NamespaceReport {
    services: Vec<ServiceReport>,
}

#[derive(Serialize)]
struct ServiceReport {
    endpoint: String,
    current: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stopped: bool,
    #[serde(skip)]
    refused: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(flatten)]
    status: Option<ServiceStatus>,
}

// Status and stop cover every build's service in the namespace.
async fn query_all(
    namespace: &Namespace,
    build: &Build,
    request: impl Fn() -> Request,
) -> Result<NamespaceReport, ClientError> {
    let (prefix, suffix) = ServiceFiles::endpoint_pattern();
    let mut services = Vec::new();
    for name in namespace.list(prefix, suffix)? {
        let Some(files) = ServiceFiles::from_endpoint(&name) else {
            continue;
        };
        let mut report = ServiceReport {
            current: files.endpoint() == build.files().endpoint(),
            endpoint: name,
            stopped: false,
            refused: false,
            error: None,
            status: None,
        };
        let reply = match read_endpoint(namespace, &report.endpoint) {
            Ok(endpoint) => exchange(&endpoint, &request())
                .await
                .map(|(reply, _)| reply),
            Err(error) => Err(ExchangeError::Transient(error.to_string())),
        };
        match reply {
            Ok(Reply::Status(status)) => report.status = Some(status),
            Ok(Reply::Stopped) => report.stopped = true,
            Ok(Reply::Error { error }) => {
                report.refused = matches!(error, ServiceError::Busy { .. });
                report.error = Some(error.to_string());
            }
            Ok(_) => report.error = Some(ClientError::UnexpectedReply.to_string()),
            Err(ExchangeError::Untrusted(error)) => return Err(error.into()),
            Err(ExchangeError::Transient(error)) => {
                report.error = Some(format!("unreachable: {error}"));
                // Discovery state without a live owner is stale.
                if owner_is_free(namespace, &files)? {
                    namespace.remove_file(&report.endpoint)?;
                    report.error = Some(format!("removed stale endpoint: {error}"));
                }
            }
        }
        services.push(report);
    }
    Ok(NamespaceReport { services })
}
