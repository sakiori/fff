use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use interprocess::local_socket::{
    ListenerOptions, Name,
    tokio::{Listener, Stream, prelude::*},
};
use parking_lot::Mutex;
use rmcp::{ServiceExt, transport::async_rw::AsyncRwTransport};
use tokio::io::BufReader;
use tokio::sync::{Notify, mpsc, oneshot};

use crate::namespace::Namespace;
use crate::protocol::{
    Endpoint, Hello, HelloHeader, MessageError, PROTOCOL, Reply, Request, ServiceError,
    ServiceStatus, read_line, send_message,
};
use crate::registry::Registry;

// Time a client has to send its hello after connecting.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
// Time an MCP client has to complete `initialize` after the session is ready.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(90);
const IDLE_CHECK_INTERVAL: Duration = Duration::from_millis(250);
const DISCOVERY_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const DISCOVERY_CHECK_TIMEOUT: Duration = Duration::from_secs(5);
// Lets in-flight replies such as the stop confirmation reach their clients.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const ACCEPT_BACKOFF_MIN: Duration = Duration::from_millis(10);
const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(1);
const BUILD_TAG_LEN: usize = 16;
const ENDPOINT_PREFIX: &str = "endpoint-";
const ENDPOINT_SUFFIX: &str = ".json";

// Identity of this executable; services of different builds run side by side.
pub(crate) struct Build {
    id: String,
}

impl Build {
    pub fn current() -> io::Result<Self> {
        let mut hasher = blake3::Hasher::new();
        hasher.update_reader(File::open(std::env::current_exe()?)?)?;
        Ok(Self {
            id: hasher.finalize().to_hex().to_string(),
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn files(&self) -> ServiceFiles {
        ServiceFiles {
            tag: self.id[..BUILD_TAG_LEN].to_owned(),
        }
    }
}

// Namespace file names of one build's service, derived from a short build tag.
pub(crate) struct ServiceFiles {
    tag: String,
}

impl ServiceFiles {
    pub fn from_endpoint(name: &str) -> Option<Self> {
        let tag = name
            .strip_prefix(ENDPOINT_PREFIX)?
            .strip_suffix(ENDPOINT_SUFFIX)?;
        Some(Self {
            tag: tag.to_owned(),
        })
    }

    pub fn endpoint_pattern() -> (&'static str, &'static str) {
        (ENDPOINT_PREFIX, ENDPOINT_SUFFIX)
    }

    pub fn endpoint(&self) -> String {
        format!("{ENDPOINT_PREFIX}{}{ENDPOINT_SUFFIX}", self.tag)
    }

    pub fn startup_lock(&self) -> String {
        format!("startup-{}.lock", self.tag)
    }

    pub fn owner_lock(&self) -> String {
        format!("owner-{}.lock", self.tag)
    }

    pub fn stderr(&self) -> String {
        format!("service-{}.stderr", self.tag)
    }

    #[cfg(unix)]
    fn socket(&self) -> String {
        format!("{}.sock", self.tag)
    }
}

pub(crate) fn directory(args: &crate::Args) -> PathBuf {
    args.service_dir
        .clone()
        .unwrap_or_else(|| crate::state_dir().join("fff").join("service"))
}

pub(crate) fn socket_name(name: &str) -> io::Result<Name<'_>> {
    #[cfg(unix)]
    {
        name.to_fs_name::<interprocess::local_socket::GenericFilePath>()
    }
    #[cfg(windows)]
    {
        name.to_ns_name::<interprocess::local_socket::GenericNamespaced>()
    }
}

pub(crate) async fn run(args: &crate::Args, build: &Build) -> io::Result<()> {
    let namespace = Arc::new(Namespace::open(&directory(args))?);
    let files = build.files();
    let startup = namespace.open_file(&files.startup_lock(), true)?;
    let owner = namespace.open_file(&files.owner_lock(), true)?;
    owner.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => {
            io::Error::other("Another service of this build owns the namespace")
        }
        std::fs::TryLockError::Error(error) => namespace.error("lock", &files.owner_lock(), error),
    })?;
    let idle = (args.idle_timeout_secs > 0).then(|| Duration::from_secs(args.idle_timeout_secs));
    let (mut service, listener, commands) =
        Service::bind(namespace, &files, build.id(), startup, idle)?;
    let result = service.run(listener, commands).await;
    drop(owner);
    drop(service);
    result.map(drop)
}

enum Exit {
    Withdrawn,
    // No client can reach the service through its namespace any more.
    Orphaned,
}

struct Service {
    context: Arc<Context>,
    discovery: Arc<Discovery>,
    startup: File,
    startup_held: Option<File>,
    idle: Option<Duration>,
}

struct Context {
    registry: Arc<Registry>,
    activity: Arc<Activity>,
    token: String,
    build: String,
    control: mpsc::Sender<StopRequest>,
}

type StopRequest = oneshot::Sender<Result<(), ServiceError>>;

impl Service {
    // The caller holds the owner lock, so no other service publishes in this namespace.
    fn bind(
        namespace: Arc<Namespace>,
        files: &ServiceFiles,
        build: &str,
        startup: File,
        idle: Option<Duration>,
    ) -> io::Result<(Self, Listener, mpsc::Receiver<StopRequest>)> {
        let name = endpoint_name(&namespace, files);
        let mut listener = crate::namespace::listener_options(
            ListenerOptions::new()
                .name(socket_name(&name)?)
                .try_overwrite(true),
        )?
        .create_tokio()
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "Cannot listen on {name}: {error}; socket paths have a platform length limit"
                ),
            )
        })?;
        // Dropping the listener would unlink its path, which a successor may have bound by then.
        listener.do_not_reclaim_name_on_drop();
        #[cfg(unix)]
        let socket = files.socket();
        #[cfg(unix)]
        let socket_id = namespace
            .file_id(&socket)?
            .ok_or_else(|| namespace.error("find", &socket, io::ErrorKind::NotFound.into()))?;
        let token = uuid::Uuid::new_v4().simple().to_string();
        let endpoint = Endpoint {
            name,
            token: token.clone(),
            protocol: PROTOCOL,
            build: build.to_owned(),
            pid: std::process::id(),
        };
        let discovery = Discovery {
            namespace,
            endpoint: files.endpoint(),
            content: serde_json::to_vec(&endpoint)?,
            token: token.clone(),
            #[cfg(unix)]
            socket,
            #[cfg(unix)]
            socket_id,
        };
        let (control, commands) = mpsc::channel(8);
        let service = Self {
            context: Arc::new(Context {
                registry: Registry::start(),
                activity: Arc::new(Activity::default()),
                token,
                build: build.to_owned(),
                control,
            }),
            discovery: Arc::new(discovery),
            startup,
            startup_held: None,
            idle,
        };
        Ok((service, listener, commands))
    }

    // Publishes the endpoint and serves; on any exit, retracts what still leads here.
    async fn run(
        &mut self,
        listener: Listener,
        commands: mpsc::Receiver<StopRequest>,
    ) -> io::Result<Exit> {
        let result = match self.discovery.publish() {
            Ok(()) => self.serve(listener, commands).await,
            Err(error) => Err(error),
        };
        self.discovery.retract();
        result
    }

    async fn serve(
        &mut self,
        listener: Listener,
        mut commands: mpsc::Receiver<StopRequest>,
    ) -> io::Result<Exit> {
        let mut connections = tokio::task::JoinSet::new();
        let mut last_busy = tokio::time::Instant::now();
        let mut tick = tokio::time::interval(IDLE_CHECK_INTERVAL);
        let mut discovery = tokio::time::interval(DISCOVERY_CHECK_INTERVAL);
        discovery.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut check: Option<DiscoveryCheck> = None;
        let mut unreachable = false;
        let mut backoff = ACCEPT_BACKOFF_MIN;
        let mut stopping: Option<tokio::task::JoinHandle<io::Result<File>>> = None;
        let mut stop_replies: Vec<StopRequest> = Vec::new();
        let activity = self.context.activity.clone();
        let result = loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok(stream) => {
                        backoff = ACCEPT_BACKOFF_MIN;
                        let context = self.context.clone();
                        connections.spawn(async move {
                            if let Err(error) = context.handle(stream).await {
                                tracing::warn!(%error, "Service connection ended");
                            }
                        });
                    }
                    Err(error) if is_listener_dead(&error) => break Err(error),
                    Err(error) => {
                        tracing::warn!(%error, "Cannot accept service connection");
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(ACCEPT_BACKOFF_MAX);
                    }
                },
                _ = connections.join_next(), if !connections.is_empty() => {}
                _ = activity.changed.notified() => last_busy = tokio::time::Instant::now(),
                Some(reply) = commands.recv() => {
                    if stopping.is_some() {
                        stop_replies.push(reply);
                        continue;
                    }
                    if let Err(error) = activity.try_close() {
                        let _ = reply.send(Err(error));
                        continue;
                    }
                    stop_replies.push(reply);
                    // Wait for clients that are starting a service without blocking this loop.
                    let startup = self.startup.try_clone()?;
                    stopping = Some(tokio::task::spawn_blocking(move || {
                        startup.lock()?;
                        Ok(startup)
                    }));
                }
                locked = async { stopping.as_mut().unwrap().await }, if stopping.is_some() => {
                    stopping = None;
                    match locked.map_err(io::Error::other).and_then(|result| result) {
                        Ok(startup) => {
                            self.startup_held = Some(startup);
                            self.discovery.withdraw();
                            for reply in stop_replies.drain(..) {
                                let _ = reply.send(Ok(()));
                            }
                            break Ok(Exit::Withdrawn);
                        }
                        Err(error) => {
                            activity.reopen();
                            for reply in stop_replies.drain(..) {
                                let _ = reply.send(Err(ServiceError::InvalidRequest {
                                    reason: error.to_string(),
                                }));
                            }
                        }
                    }
                }
                // A stopping service withdraws anyway, so it needs no discovery check.
                _ = discovery.tick(), if check.is_none() && stopping.is_none() => {
                    check = Some(DiscoveryCheck::start(self.discovery.clone()));
                }
                checked = async { check.as_mut().unwrap().wait().await }, if check.is_some() => {
                    let Some(result) = checked else {
                        tracing::warn!("Service discovery check is not answering");
                        continue;
                    };
                    // An overdue answer may describe a namespace that has changed since.
                    if check.take().is_some_and(|check| check.overdue) {
                        continue;
                    }
                    match result {
                        Ok(true) if unreachable => {
                            tracing::info!("Service namespace leads to this service again");
                            unreachable = false;
                        }
                        Ok(true) => {}
                        Ok(false) => {
                            if !unreachable {
                                tracing::warn!("Service namespace no longer leads to this service");
                                unreachable = true;
                            }
                            // Idle exit may be disabled, but an unreachable service cannot gain
                            // sessions. Only this fresh answer decides; an earlier one may be stale.
                            if stopping.is_none() && activity.try_close().is_ok() {
                                tracing::info!("Exiting orphaned shared service");
                                break Ok(Exit::Orphaned);
                            }
                        }
                        Err(error) => tracing::warn!(%error, "Cannot check service discovery"),
                    }
                }
                _ = tick.tick() => {
                    if let Some(idle) = self.idle
                        && stopping.is_none()
                        && activity.sessions() == 0
                        && last_busy.elapsed() >= idle
                        && self.startup.try_lock().is_ok()
                    {
                        if activity.try_close().is_ok() {
                            tracing::info!(?idle, "Exiting shared service after idle timeout");
                            self.discovery.withdraw();
                            break Ok(Exit::Withdrawn);
                        }
                        self.startup.unlock()?;
                    }
                }
            }
        };
        // Clients that connected after withdrawal see a closed stream and retry via startup.
        drop(listener);
        let _ = tokio::time::timeout(SHUTDOWN_GRACE, async {
            while connections.join_next().await.is_some() {}
        })
        .await;
        connections.abort_all();
        result
    }
}

// The namespace entries through which clients reach this service.
struct Discovery {
    namespace: Arc<Namespace>,
    endpoint: String,
    content: Vec<u8>,
    token: String,
    #[cfg(unix)]
    socket: String,
    #[cfg(unix)]
    socket_id: crate::namespace::FileId,
}

impl Discovery {
    fn publish(&self) -> io::Result<()> {
        self.namespace.write_atomic(&self.endpoint, &self.content)
    }

    // Ok(false) only when clients definitely cannot reach this service; errors are indeterminate.
    fn reachable(&self) -> io::Result<bool> {
        if !self.namespace.names_opened_directory()? || !self.endpoint_is_ours()? {
            return Ok(false);
        }
        #[cfg(unix)]
        {
            Ok(self.namespace.file_id(&self.socket)? == Some(self.socket_id))
        }
        #[cfg(windows)]
        {
            Ok(true)
        }
    }

    fn endpoint_is_ours(&self) -> io::Result<bool> {
        let name = &self.endpoint;
        let mut file = match self.namespace.open_file(name, false) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| self.namespace.error("read", name, error))?;
        // Endpoints are replaced atomically, so content that does not parse is not a partial
        // write but a file no client can use to reach this service.
        Ok(serde_json::from_slice::<Endpoint>(&bytes)
            .is_ok_and(|endpoint| endpoint.token == self.token))
    }

    // Removes the endpoint before the listener closes so no new client can find it.
    fn withdraw(&self) {
        if let Err(error) = self.remove_endpoint() {
            tracing::warn!(%error, "Cannot remove service endpoint");
        }
    }

    // Removes whatever discovery state still leads to this service, and nothing a successor
    // published in its place.
    fn retract(&self) {
        self.withdraw();
        #[cfg(unix)]
        if let Err(error) = self.remove_socket() {
            tracing::warn!(%error, "Cannot remove service socket");
        }
    }

    fn remove_endpoint(&self) -> io::Result<()> {
        if self.namespace.names_opened_directory()? && self.endpoint_is_ours()? {
            self.namespace.remove_file(&self.endpoint)?;
        }
        Ok(())
    }

    #[cfg(unix)]
    fn remove_socket(&self) -> io::Result<()> {
        if self.namespace.names_opened_directory()? {
            self.namespace
                .remove_file_if(&self.socket, self.socket_id)?;
        }
        Ok(())
    }
}

// A discovery check off the service loop, so a hung filesystem cannot stall the service.
struct DiscoveryCheck {
    task: tokio::task::JoinHandle<io::Result<bool>>,
    deadline: tokio::time::Instant,
    overdue: bool,
}

impl DiscoveryCheck {
    fn start(discovery: Arc<Discovery>) -> Self {
        Self {
            task: tokio::task::spawn_blocking(move || discovery.reachable()),
            deadline: tokio::time::Instant::now() + DISCOVERY_CHECK_TIMEOUT,
            overdue: false,
        }
    }

    // Yields None once when the check exceeds its timeout, then its eventual answer.
    async fn wait(&mut self) -> Option<io::Result<bool>> {
        let joined = if self.overdue {
            (&mut self.task).await
        } else {
            match tokio::time::timeout_at(self.deadline, &mut self.task).await {
                Ok(joined) => joined,
                Err(_) => {
                    self.overdue = true;
                    return None;
                }
            }
        };
        Some(joined.map_err(io::Error::other).and_then(|result| result))
    }
}

impl Context {
    async fn handle(self: Arc<Self>, stream: Stream) -> io::Result<()> {
        crate::namespace::verify_peer(&stream)?;
        let (recv, mut send) = stream.split();
        let mut recv = BufReader::new(recv);
        let line = match tokio::time::timeout(HELLO_TIMEOUT, read_line(&mut recv)).await {
            Err(_) => return Err(io::Error::new(io::ErrorKind::TimedOut, "No service hello")),
            Ok(Err(MessageError::Io(error))) => return Err(error),
            Ok(Err(MessageError::Closed)) => return Ok(()),
            Ok(Err(error)) => {
                let error = ServiceError::InvalidRequest {
                    reason: error.to_string(),
                };
                return send_message(&mut send, &Reply::Error { error }).await;
            }
            Ok(Ok(line)) => line,
        };
        let request = match self.authenticate(&line) {
            Ok(request) => request,
            Err(error) => return send_message(&mut send, &Reply::Error { error }).await,
        };
        let session = match request {
            Request::Status => return send_message(&mut send, &Reply::Status(self.status())).await,
            Request::Stop => {
                let reply = match self.stop().await {
                    Ok(()) => Reply::Stopped,
                    Err(error) => Reply::Error { error },
                };
                return send_message(&mut send, &reply).await;
            }
            Request::Session { scope, options } => {
                let registered = match self.activity.try_lease() {
                    Ok(lease) => self.registry.register(scope, options, lease).await,
                    Err(error) => Err(error),
                };
                match registered {
                    Ok(session) => session,
                    Err(error) => return send_message(&mut send, &Reply::Error { error }).await,
                }
            }
        };
        let warnings = session.snapshot().index.warnings.clone();
        send_message(&mut send, &Reply::Ready { warnings }).await?;
        let server = crate::server::FffServer::new(session);
        let transport =
            crate::handshake::ProbeTolerantTransport::new(AsyncRwTransport::new_server(recv, send));
        let service = tokio::time::timeout(INITIALIZE_TIMEOUT, server.serve(transport))
            .await?
            .map_err(io::Error::other)?;
        service.waiting().await.map_err(io::Error::other)?;
        Ok(())
    }

    fn authenticate(&self, line: &[u8]) -> Result<Request, ServiceError> {
        let invalid = |error: serde_json::Error| ServiceError::InvalidRequest {
            reason: error.to_string(),
        };
        let header: HelloHeader = serde_json::from_slice(line).map_err(invalid)?;
        if !constant_time_eq::constant_time_eq(header.token.as_bytes(), self.token.as_bytes()) {
            return Err(ServiceError::Unauthorized);
        }
        if header.protocol != PROTOCOL {
            return Err(ServiceError::Protocol {
                service: PROTOCOL,
                client: header.protocol,
            });
        }
        let hello: Hello = serde_json::from_slice(line).map_err(invalid)?;
        Ok(hello.request)
    }

    async fn stop(&self) -> Result<(), ServiceError> {
        let (reply, result) = oneshot::channel();
        self.control
            .send(reply)
            .await
            .map_err(|_| ServiceError::ShuttingDown)?;
        result.await.map_err(|_| ServiceError::ShuttingDown)?
    }

    fn status(&self) -> ServiceStatus {
        let (indexes, sessions) = self.registry.status();
        ServiceStatus {
            pid: std::process::id(),
            build: self.build.clone(),
            protocol: PROTOCOL,
            index_count: indexes.len(),
            indexes,
            sessions,
        }
    }
}

// Counts search sessions (including ones still registering); diagnostics do not count.
#[derive(Default)]
pub(crate) struct Activity {
    state: Mutex<ActivityState>,
    changed: Notify,
}

#[derive(Default)]
struct ActivityState {
    sessions: usize,
    closing: bool,
}

pub(crate) struct Lease(Arc<Activity>);

impl Activity {
    fn try_lease(self: &Arc<Self>) -> Result<Lease, ServiceError> {
        {
            let mut state = self.state.lock();
            if state.closing {
                return Err(ServiceError::ShuttingDown);
            }
            state.sessions += 1;
        }
        self.changed.notify_one();
        Ok(Lease(self.clone()))
    }

    // Atomically refuses new sessions if none are active.
    fn try_close(&self) -> Result<(), ServiceError> {
        let mut state = self.state.lock();
        if state.sessions > 0 {
            return Err(ServiceError::Busy {
                sessions: state.sessions,
            });
        }
        state.closing = true;
        Ok(())
    }

    fn reopen(&self) {
        self.state.lock().closing = false;
    }

    fn sessions(&self) -> usize {
        self.state.lock().sessions
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.0.state.lock().sessions -= 1;
        self.0.changed.notify_one();
    }
}

fn endpoint_name(namespace: &Namespace, files: &ServiceFiles) -> String {
    #[cfg(unix)]
    {
        namespace
            .path()
            .join(files.socket())
            .to_string_lossy()
            .into_owned()
    }
    #[cfg(windows)]
    {
        let _ = (namespace, files);
        format!("fff-{}", uuid::Uuid::new_v4().simple())
    }
}

// Only errors that mean the listening socket itself is unusable end the service.
fn is_listener_dead(error: &io::Error) -> bool {
    #[cfg(unix)]
    {
        use rustix::io::Errno;
        matches!(
            Errno::from_io_error(error),
            Some(Errno::BADF | Errno::INVAL | Errno::NOTSOCK | Errno::OPNOTSUPP)
        )
    }
    #[cfg(windows)]
    {
        let _ = error;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::fs::Permissions;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::path::Path;

    #[test]
    fn closing_is_refused_while_sessions_are_active() {
        let activity = Arc::new(Activity::default());
        let lease = activity.try_lease().unwrap();
        assert_eq!(
            activity.try_close(),
            Err(ServiceError::Busy { sessions: 1 })
        );
        drop(lease);
        assert!(activity.try_close().is_ok());
        assert!(matches!(
            activity.try_lease(),
            Err(ServiceError::ShuttingDown)
        ));
        activity.reopen();
        assert!(activity.try_lease().is_ok());
    }

    #[test]
    fn endpoint_names_round_trip_to_service_files() {
        let files = ServiceFiles {
            tag: "0123456789abcdef".into(),
        };
        let parsed = ServiceFiles::from_endpoint(&files.endpoint()).unwrap();
        assert_eq!(parsed.owner_lock(), files.owner_lock());
        assert!(ServiceFiles::from_endpoint("owner-0123.lock").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn only_unusable_listeners_are_fatal() {
        use rustix::io::Errno;
        assert!(is_listener_dead(&io::Error::from(Errno::BADF)));
        assert!(!is_listener_dead(&io::Error::from(Errno::MFILE)));
        assert!(!is_listener_dead(&io::Error::from(Errno::CONNABORTED)));
    }

    // Waits long enough for several discovery checks to complete.
    #[cfg(unix)]
    async fn settle() {
        tokio::time::sleep(DISCOVERY_CHECK_INTERVAL * 2).await;
    }

    #[cfg(unix)]
    struct Running {
        path: PathBuf,
        namespace: Arc<Namespace>,
        files: ServiceFiles,
        activity: Arc<Activity>,
        exit: tokio::task::JoinHandle<io::Result<Exit>>,
    }

    #[cfg(unix)]
    impl Running {
        async fn start(temp: &Path) -> Self {
            let path = temp.join("service");
            let namespace = Arc::new(Namespace::open(&path).unwrap());
            let files = ServiceFiles {
                tag: "0123456789abcdef".into(),
            };
            let startup = namespace.open_file(&files.startup_lock(), true).unwrap();
            let (mut service, listener, commands) =
                Service::bind(namespace.clone(), &files, "test", startup, None).unwrap();
            let activity = service.context.activity.clone();
            let exit = tokio::spawn(async move { service.run(listener, commands).await });
            let running = Self {
                path,
                namespace,
                files,
                activity,
                exit,
            };
            while !running.endpoint().exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            running
        }

        fn endpoint(&self) -> PathBuf {
            self.path.join(self.files.endpoint())
        }

        fn socket(&self) -> PathBuf {
            self.path.join(self.files.socket())
        }

        async fn orphaned(&mut self) {
            let exit = tokio::time::timeout(Duration::from_secs(15), &mut self.exit)
                .await
                .expect("unreachable service kept running")
                .unwrap();
            assert!(matches!(exit, Ok(Exit::Orphaned)));
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn only_a_fresh_definite_answer_orphans_the_service() {
        let temp = tempfile::tempdir().unwrap();
        let mut service = Running::start(temp.path()).await;
        let endpoint = service.endpoint();
        let published = std::fs::read(&endpoint).unwrap();

        // An endpoint that cannot be read proves nothing about reachability.
        let mode = |mode| std::fs::set_permissions(&endpoint, Permissions::from_mode(mode));
        mode(0o644).unwrap();
        settle().await;
        assert!(
            !service.exit.is_finished(),
            "indeterminate check orphaned the service"
        );
        mode(0o600).unwrap();

        // Unreachable while a session keeps it alive, then reachable again before it ends.
        let lease = service.activity.try_lease().unwrap();
        std::fs::remove_file(&endpoint).unwrap();
        settle().await;
        service
            .namespace
            .write_atomic(&service.files.endpoint(), &published)
            .unwrap();
        settle().await;
        drop(lease);
        settle().await;
        assert!(
            !service.exit.is_finished(),
            "stale answer orphaned the service"
        );

        std::fs::remove_file(&endpoint).unwrap();
        service.orphaned().await;
        assert!(!service.socket().exists(), "orphan left its own socket");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn service_orphaned_by_a_lost_socket_removes_its_endpoint() {
        let temp = tempfile::tempdir().unwrap();
        let mut service = Running::start(temp.path()).await;
        std::fs::remove_file(service.socket()).unwrap();
        service.orphaned().await;
        assert!(!service.endpoint().exists(), "orphan left its own endpoint");
    }
}
