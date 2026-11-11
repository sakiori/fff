use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use interprocess::local_socket::tokio::{RecvHalf, SendHalf, Stream, prelude::*};
use serde::Serialize;
use tokio::io::BufReader;

use crate::identity::{Build, ServiceFiles};
use crate::namespace::Namespace;
use crate::protocol::{
    Endpoint, Hello, MAX_REPLY_LINE_BYTES, MessageError, PROTOCOL, Reply, Request, ServiceError,
    ServiceStatus, read_message, send_message,
};

/// Default budget for finding or starting a service.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(90);
// Time a service has to answer a hello.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const STARTUP_POLL: Duration = Duration::from_millis(50);
const LOCK_POLL: Duration = Duration::from_millis(20);
const RETRY_BACKOFF: Duration = Duration::from_millis(100);
#[cfg(unix)]
const SPAWN_RETRY: Duration = Duration::from_millis(25);
#[cfg(unix)]
const SPAWN_ATTEMPTS: usize = 20;
const STARTUP_ERROR_BYTES: u64 = 4096;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The service namespace cannot be used, as opposed to a service that cannot be reached.
    #[error(transparent)]
    Namespace(io::Error),
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
    #[error("the shared service did not answer within {0:?}")]
    TimedOut(Duration),
    #[error(
        "the service of build {running} owns this build's namespace files; \
         use another service namespace for build {expected}"
    )]
    BuildCollision { running: String, expected: String },
    #[error(transparent)]
    Program(#[from] crate::program::ProgramError),
    #[error(transparent)]
    Frame(#[from] crate::program::FrameError),
}

impl ClientError {
    /// Whether the session lost its service, as opposed to the service refusing the request:
    /// the service did not answer in time, could not be reached or the connection broke.
    pub fn lost_service(&self) -> bool {
        matches!(
            self,
            Self::Io(_)
                | Self::Frame(_)
                | Self::Disconnected
                | Self::TimedOut(_)
                | Self::Unavailable(_)
                | Self::StartupFailed { .. }
        )
    }
}

/// An accepted session: the reader and writer halves of its connection.
pub type Connection = (BufReader<RecvHalf>, SendHalf);

/// How a client starts the service of its build when none runs.
pub struct Launcher {
    pub executable: PathBuf,
    pub idle_timeout_secs: u64,
    pub log_file: Option<PathBuf>,
    pub log_level: Option<String>,
    /// `None` leaves it to the service's `FFF_FULL_PARALLEL_INDEXING`.
    pub full_parallel_indexing: Option<bool>,
}

enum Attempt {
    Connected(Connection, Vec<String>),
    Retry(String),
    Failed(ClientError),
}

/// Opens a session with the service of `build`, starting one when none answers, within
/// `timeout`.
pub async fn open_session(
    namespace: &Namespace,
    build: &Build,
    request: &Request,
    launcher: &Launcher,
    timeout: Duration,
) -> Result<(Connection, Vec<String>), ClientError> {
    tokio::time::timeout(timeout, connect(namespace, build, request, launcher))
        .await
        .map_err(|_| ClientError::Unavailable(timeout))?
}

// Any failure before `ready` is retried through the startup lock, which also recovers
// stale discovery state and services that are shutting down.
async fn connect(
    namespace: &Namespace,
    build: &Build,
    request: &Request,
    launcher: &Launcher,
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
        let owner_free = owner_is_free(namespace, &files)?;
        // A live service of another build under the same file names never answers this one.
        if !owner_free
            && let Ok(endpoint) = read_endpoint(namespace, &files.endpoint())
            && endpoint.build != build.id()
        {
            return Err(ClientError::BuildCollision {
                running: endpoint.build,
                expected: build.id().to_owned(),
            });
        }
        if owner_free {
            namespace.remove_file(&files.endpoint())?;
            let mut child = Started(Some(
                spawn_service_retrying(launcher, namespace, build).await?,
            ));
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
    let name = crate::socket_name(&endpoint.name).map_err(|e| transient(&e))?;
    let stream = Stream::connect(name).await.map_err(|e| transient(&e))?;
    crate::namespace::verify_peer(&stream).map_err(ExchangeError::Untrusted)?;
    let (recv, mut send) = stream.split();
    crate::sigpipe::suppress(&send).map_err(|e| transient(&e))?;
    let mut recv = BufReader::new(recv);
    let hello = Hello {
        token: endpoint.token.clone(),
        protocol: PROTOCOL,
        request: request.clone(),
    };
    send_message(&mut send, &hello)
        .await
        .map_err(|e| transient(&e))?;
    let reply = read_message::<Reply, _>(&mut recv, MAX_REPLY_LINE_BYTES);
    // A subscription is ready once its index is, which a large tree may take longer than a
    // handshake to reach; the caller's own timeout bounds it.
    let reply = if matches!(request, Request::Watch { .. }) {
        reply.await
    } else {
        tokio::time::timeout(HANDSHAKE_TIMEOUT, reply)
            .await
            .map_err(|e| transient(&e))?
    }
    .map_err(|e: MessageError| transient(&e))?;
    Ok((reply, (recv, send)))
}

fn read_endpoint(namespace: &Namespace, name: &str) -> io::Result<Endpoint> {
    Ok(serde_json::from_reader(namespace.open_file(name, false)?)?)
}

// Polls rather than blocking a thread, so a caller's timeout also ends the wait.
async fn lock_startup(namespace: &Namespace, files: &ServiceFiles) -> io::Result<File> {
    let name = files.startup_lock();
    let startup = namespace.open_file(&name, true)?;
    loop {
        match startup.try_lock() {
            Ok(()) => return Ok(startup),
            Err(std::fs::TryLockError::WouldBlock) => tokio::time::sleep(LOCK_POLL).await,
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(namespace.error("lock", &name, error));
            }
        }
    }
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

// A freshly installed executable can be briefly busy: another thread's child may hold the
// descriptor it was written through until that child execs. The waits are the caller's, so
// its deadline still applies.
async fn spawn_service_retrying(
    launcher: &Launcher,
    namespace: &Namespace,
    build: &Build,
) -> io::Result<Child> {
    #[cfg(unix)]
    for _ in 0..SPAWN_ATTEMPTS {
        match spawn_service(launcher, namespace, build) {
            Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) => {
                tokio::time::sleep(SPAWN_RETRY).await;
            }
            spawned => return spawned,
        }
    }
    spawn_service(launcher, namespace, build)
}

fn spawn_service(launcher: &Launcher, namespace: &Namespace, build: &Build) -> io::Result<Child> {
    let stderr = namespace.open_file(&build.files().stderr(), true)?;
    stderr.set_len(0)?;
    let executable = service_executable(launcher, namespace, build)?;
    // The service resolves paths from its working directory, so paths of the client's
    // directory must be absolute.
    let mut command = Command::new(executable);
    command
        .args(["--service", "--service-dir"])
        .arg(namespace.path())
        .arg("--idle-timeout-secs")
        .arg(launcher.idle_timeout_secs.to_string())
        .arg("--service-build")
        .arg(build.id())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr));
    if let Some(level) = &launcher.log_level {
        command.arg("--log-level").arg(level);
    }
    if let Some(log) = &launcher.log_file {
        command.arg("--log-file").arg(std::path::absolute(log)?);
    }
    if let Some(enabled) = launcher.full_parallel_indexing {
        command.arg(format!("--full-parallel-indexing={enabled}"));
    }
    detach(&mut command, namespace)
}

// The executable to start. One that lives inside the running AppImage is copied into the
// namespace first: the AppImage's mount goes away once its application exits, and a
// service still running from it would fail on its next page-in.
#[cfg(unix)]
fn service_executable(
    launcher: &Launcher,
    namespace: &Namespace,
    build: &Build,
) -> io::Result<PathBuf> {
    let executable = std::path::absolute(&launcher.executable)?;
    let Some(mount) = std::env::var_os("APPDIR").map(PathBuf::from) else {
        return Ok(executable);
    };
    let inside = match (executable.canonicalize(), mount.canonicalize()) {
        (Ok(executable), Ok(mount)) => executable.starts_with(mount),
        _ => false,
    };
    if !inside {
        return Ok(executable);
    }
    let files = build.files();
    let name = files.executable();
    remove_stale_executables(namespace, &files);
    // A copy is installed atomically under the build's name, so an existing one is complete.
    // It is looked up without opening it, which a running copy would refuse for writing.
    match namespace.file_id(&name)? {
        Some(_) => Ok(namespace.path().join(name)),
        None => namespace.install_executable(&name, &executable),
    }
}

// Removes executable copies of other builds that no service runs and no client is starting,
// both of which hold that build's locks. A failure leaves the copy for a later start.
#[cfg(unix)]
fn remove_stale_executables(namespace: &Namespace, current: &ServiceFiles) {
    let (prefix, suffix) = ServiceFiles::executable_pattern();
    let Ok(names) = namespace.list(prefix, suffix) else {
        return;
    };
    for name in names {
        let Some(files) = ServiceFiles::from_executable(&name) else {
            continue;
        };
        if name == current.executable() {
            continue;
        }
        // A build that never started here has no locks; none are created for it.
        let unlocked = |lock: String| match namespace.open_file(&lock, false) {
            Ok(file) => file.try_lock().is_ok().then_some(Some(file)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Some(None),
            Err(_) => None,
        };
        let Some(_startup) = unlocked(files.startup_lock()) else {
            continue;
        };
        if unlocked(files.owner_lock()).is_some() {
            let _ = namespace.remove_file(&name);
        }
    }
}

#[cfg(windows)]
fn service_executable(
    launcher: &Launcher,
    _namespace: &Namespace,
    _build: &Build,
) -> io::Result<PathBuf> {
    std::path::absolute(&launcher.executable)
}

// What the service keeps of the client's environment. It serves every client of the
// namespace, so it takes none of one client's loader, git or application settings.
#[cfg(unix)]
fn kept_variable(name: &str) -> bool {
    // No `LD_*` or `DYLD_*`: an application's library path, such as an AppImage's, would load
    // its libraries into the service. The glibc build needs only the system loader, which
    // finds glibc through its own configuration, and the musl and macOS builds load nothing
    // from the environment. On NixOS, nix-ld's loader stub needs `NIX_LD` and
    // `NIX_LD_LIBRARY_PATH` to start any binary built for other distributions.
    const NAMES: &[&str] = &[
        "HOME",
        "USER",
        "LOGNAME",
        "PATH",
        "LANG",
        "TZ",
        "TMPDIR",
        "NIX_LD",
        "NIX_LD_LIBRARY_PATH",
    ];
    const PREFIXES: &[&str] = &["LC_", "XDG_", "FFF_", "RUST_"];
    NAMES.contains(&name) || PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

// A started service is the client's child, and a long-running client must not collect a
// zombie each time one exits, so a thread waits for it once the client lets go.
struct Started(Option<Child>);

impl Started {
    fn try_wait(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        self.0.as_mut().map_or(Ok(None), Child::try_wait)
    }
}

impl Drop for Started {
    fn drop(&mut self) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        let reaper = std::thread::Builder::new()
            .name("fff-service-reaper".into())
            .spawn(move || child.wait());
        if let Err(error) = reaper {
            tracing::warn!(%error, "Cannot wait for the started service");
        }
    }
}

// The service outlives the client in its own process group, in `/` with a reduced
// environment, and holds none of the client's descriptors beyond its standard streams, so it
// pins no mount or pipe of the program that started it.
#[cfg(unix)]
fn detach(command: &mut Command, _namespace: &Namespace) -> io::Result<Child> {
    use std::os::unix::process::CommandExt;
    command.env_clear();
    command.envs(std::env::vars_os().filter(|(name, _)| name.to_str().is_some_and(kept_variable)));
    command.current_dir("/").process_group(0);
    let last = descriptor_bound();
    // SAFETY: the closure only makes system calls, which are async-signal-safe, and
    // allocates nothing.
    unsafe {
        command.pre_exec(move || {
            close_inherited_descriptors(last);
            Ok(())
        });
    }
    command.spawn()
}

// One past the highest descriptor the process can hold, computed before fork: the soft
// limit, or, when that is unlimited, the system's per-process maximum.
#[cfg(unix)]
fn descriptor_bound() -> libc::c_int {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes only the struct it is given.
    let soft = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 {
        limit.rlim_cur
    } else {
        libc::RLIM_INFINITY
    };
    if soft != libc::RLIM_INFINITY {
        return libc::c_int::try_from(soft).unwrap_or(libc::c_int::MAX);
    }
    system_descriptor_maximum()
}

#[cfg(target_os = "linux")]
fn system_descriptor_maximum() -> libc::c_int {
    std::fs::read_to_string("/proc/sys/fs/nr_open")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(1 << 20)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn system_descriptor_maximum() -> libc::c_int {
    let mut maximum: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    // SAFETY: sysctlbyname writes at most `size` bytes into `maximum`.
    let found = unsafe {
        libc::sysctlbyname(
            c"kern.maxfilesperproc".as_ptr(),
            (&mut maximum as *mut libc::c_int).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } == 0;
    if found && maximum > 0 {
        maximum
    } else {
        1 << 20
    }
}

// Marks every descriptor above the standard streams close-on-exec, in the child between fork
// and exec, so exec drops those the client inherited without that flag. Marking rather than
// closing keeps the descriptor through which the standard library reports a failed exec.
#[cfg(unix)]
fn close_inherited_descriptors(last: libc::c_int) {
    const FIRST: libc::c_int = 3;
    #[cfg(target_os = "linux")]
    {
        const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
        // SAFETY: close_range only changes descriptor flags.
        let done = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                FIRST as libc::c_uint,
                libc::c_uint::MAX,
                CLOSE_RANGE_CLOEXEC,
            )
        };
        if done == 0 {
            return;
        }
    }
    // Kernels without close_range, and macOS: every descriptor the process can hold.
    for fd in FIRST..last {
        // SAFETY: fcntl on a descriptor that may not exist fails with EBADF and changes nothing.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
}

// The service must outlive the client's console, process group and job object.
#[cfg(windows)]
fn detach(command: &mut Command, namespace: &Namespace) -> io::Result<Child> {
    command.current_dir(namespace.path());
    use std::os::windows::process::CommandExt;
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

/// What every service of a namespace answered to a status or stop request.
#[derive(Serialize)]
pub struct NamespaceReport {
    services: Vec<ServiceReport>,
}

impl NamespaceReport {
    /// Services that refused to stop because clients are connected.
    pub fn refused(&self) -> usize {
        self.services.iter().filter(|s| s.refused).count()
    }
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

/// Sends a status or stop request to every build's service in the namespace.
pub async fn query_all(
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
