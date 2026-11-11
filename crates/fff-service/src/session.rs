use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use crate::Build;
pub use crate::client::CONNECT_TIMEOUT;
use crate::client::{ClientError, Connection, Launcher, open_session};
use crate::namespace::Namespace;
use crate::program::{
    DirResults, FileResults, FrameError, GlobRequest, GrepRequest, GrepResults, MAX_REPLY_BYTES,
    MAX_WAIT_READY, MixedResults, MultiGrepRequest, ProgramReply, ProgramRequest, ScanProgress,
    SearchRequest, read_frame, write_frame,
};
use crate::protocol::{IndexOptions, Request};

/// Default time a call waits for a service that stopped answering, beyond the time the call
/// itself asks for. Calls that do no search answer without waiting for searches, and searches
/// over an index answer well within it; a grep that may take longer passes a time budget.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// Where and how a program session is opened.
pub struct ProgramConfig {
    /// Directory the session searches.
    pub scope: PathBuf,
    pub options: IndexOptions,
    /// The `fff-mcp` executable started when no service of this build runs; it must
    /// be of the same build as this library.
    pub executable: PathBuf,
    /// Service namespace; `None` takes `FFF_MCP_SERVICE_DIR` or the default namespace.
    pub service_dir: Option<PathBuf>,
    /// Idle timeout of a service this session starts; zero disables idle exit.
    pub idle_timeout_secs: u64,
    pub log_file: Option<PathBuf>,
    pub log_level: Option<String>,
    /// How long opening the session may take, including starting the service;
    /// [`CONNECT_TIMEOUT`] by default.
    pub connect_timeout: Duration,
    /// How long a call waits for a service that does not answer, beyond the time the call
    /// itself asks for: a readiness wait's timeout or a grep's time budget;
    /// [`RESPONSE_TIMEOUT`] by default.
    pub response_timeout: Duration,
}

/// A blocking program session on the shared service. Calls are serialized; a program
/// that wants concurrent calls opens more sessions. Dropping it ends the session.
///
/// Every call is bounded in time, by one deadline that also covers reopening the session. A
/// call that fails or times out mid-exchange drops the connection, and the next call opens a
/// new session for the same directory and options within its own deadline.
pub struct ProgramSession {
    runtime: tokio::runtime::Runtime,
    target: Target,
    connection: Mutex<Option<Connection>>,
    warnings: Vec<String>,
}

// What the session connects to, resolved once against the program's working directory.
pub(crate) struct Target {
    directory: PathBuf,
    request: Request,
    launcher: Launcher,
    pub(crate) connect_timeout: Duration,
    response_timeout: Duration,
}

impl ProgramSession {
    /// Connects to the service of this build, starting it when none runs.
    pub fn open(config: &ProgramConfig) -> Result<Self, ClientError> {
        let target = Target::resolve(config, |scope, options| Request::Program { scope, options })?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let (connection, warnings) = match target.connect(&runtime) {
            Ok(opened) => opened,
            Err(error) => {
                // Nothing a failed or timed out attempt left running may hold up the caller.
                runtime.shutdown_background();
                return Err(error);
            }
        };
        Ok(Self {
            runtime,
            target,
            connection: Mutex::new(Some(connection)),
            warnings,
        })
    }

    /// Warnings the service reported for the index, such as an unusable frecency database.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn search_files(&self, request: SearchRequest) -> Result<FileResults, ClientError> {
        match self.request(ProgramRequest::SearchFiles(request))? {
            ProgramReply::Files(results) => Ok(results),
            _ => Err(ClientError::UnexpectedReply),
        }
    }

    pub fn search_directories(&self, request: SearchRequest) -> Result<DirResults, ClientError> {
        match self.request(ProgramRequest::SearchDirectories(request))? {
            ProgramReply::Directories(results) => Ok(results),
            _ => Err(ClientError::UnexpectedReply),
        }
    }

    pub fn search_mixed(&self, request: SearchRequest) -> Result<MixedResults, ClientError> {
        match self.request(ProgramRequest::SearchMixed(request))? {
            ProgramReply::Mixed(results) => Ok(results),
            _ => Err(ClientError::UnexpectedReply),
        }
    }

    pub fn glob(&self, request: GlobRequest) -> Result<FileResults, ClientError> {
        match self.request(ProgramRequest::Glob(request))? {
            ProgramReply::Files(results) => Ok(results),
            _ => Err(ClientError::UnexpectedReply),
        }
    }

    pub fn grep(&self, request: GrepRequest) -> Result<GrepResults, ClientError> {
        match self.request(ProgramRequest::Grep(request))? {
            ProgramReply::Grep(results) => Ok(results),
            _ => Err(ClientError::UnexpectedReply),
        }
    }

    pub fn multi_grep(&self, request: MultiGrepRequest) -> Result<GrepResults, ClientError> {
        match self.request(ProgramRequest::MultiGrep(request))? {
            ProgramReply::Grep(results) => Ok(results),
            _ => Err(ClientError::UnexpectedReply),
        }
    }

    /// Returns false when the index is not ready within `timeout`, which the service caps
    /// at [`MAX_WAIT_READY`].
    pub fn wait_ready(&self, timeout: Duration) -> Result<bool, ClientError> {
        let timeout_ms = timeout
            .min(MAX_WAIT_READY)
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        match self.request(ProgramRequest::WaitReady { timeout_ms })? {
            ProgramReply::Ready(ready) => Ok(ready),
            _ => Err(ClientError::UnexpectedReply),
        }
    }

    pub fn is_scanning(&self) -> Result<bool, ClientError> {
        match self.request(ProgramRequest::IsScanning)? {
            ProgramReply::Scanning(scanning) => Ok(scanning),
            _ => Err(ClientError::UnexpectedReply),
        }
    }

    pub fn progress(&self) -> Result<ScanProgress, ClientError> {
        match self.request(ProgramRequest::Progress)? {
            ProgramReply::Progress(progress) => Ok(progress),
            _ => Err(ClientError::UnexpectedReply),
        }
    }

    pub fn rescan(&self) -> Result<(), ClientError> {
        match self.request(ProgramRequest::Rescan)? {
            ProgramReply::Done => Ok(()),
            _ => Err(ClientError::UnexpectedReply),
        }
    }

    /// Applies changes the program made to `paths` to the session's index without a rescan;
    /// see [`ProgramRequest::Changed`].
    pub fn changed(&self, paths: Vec<PathBuf>) -> Result<(), ClientError> {
        match self.request(ProgramRequest::Changed { paths })? {
            ProgramReply::Done => Ok(()),
            _ => Err(ClientError::UnexpectedReply),
        }
    }

    /// The root of the git work tree containing the session's directory, if any.
    pub fn repository(&self) -> Result<Option<PathBuf>, ClientError> {
        match self.request(ProgramRequest::Repository)? {
            ProgramReply::Repository(root) => Ok(root),
            _ => Err(ClientError::UnexpectedReply),
        }
    }

    /// Ends the session and waits for the service to acknowledge it.
    pub fn close(self) -> Result<(), ClientError> {
        match self.request(ProgramRequest::Close)? {
            ProgramReply::Done => Ok(()),
            _ => Err(ClientError::UnexpectedReply),
        }
    }

    /// Sends one request and returns its reply; an error reply is returned as the error.
    /// A failed or timed out exchange leaves the stream mid-frame, so the connection is
    /// dropped with it, and the next request opens a new session first.
    pub fn request(&self, request: ProgramRequest) -> Result<ProgramReply, ClientError> {
        let mut connection = self
            .connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // One deadline covers reopening the session and the exchange.
        let limit = self.target.time_limit(&request);
        let deadline = tokio::time::Instant::now() + limit;
        if connection.is_none() {
            if matches!(request, ProgramRequest::Close) {
                return Err(ClientError::Disconnected);
            }
            let (reopened, _) = self.target.connect_within(&self.runtime, limit)?;
            *connection = Some(reopened);
        }
        let (recv, send) = connection.as_mut().ok_or(ClientError::Disconnected)?;
        let exchange = async {
            write_frame(send, &request).await?;
            read_frame(recv, MAX_REPLY_BYTES).await
        };
        let result = self.runtime.block_on(async {
            tokio::time::timeout_at(deadline, exchange)
                .await
                .map_err(|_| ClientError::TimedOut(limit))
        });
        match result {
            Ok(Ok(ProgramReply::Error(error))) => Err(error.into()),
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(error)) => {
                *connection = None;
                Err(match error {
                    FrameError::Closed => ClientError::Disconnected,
                    error => error.into(),
                })
            }
            Err(error) => {
                *connection = None;
                Err(error)
            }
        }
    }
}

impl Target {
    // `request` builds the hello's request from the absolute scope and the options.
    pub(crate) fn resolve(
        config: &ProgramConfig,
        request: impl FnOnce(PathBuf, IndexOptions) -> Request,
    ) -> Result<Self, ClientError> {
        let directory = config
            .service_dir
            .clone()
            .or_else(|| std::env::var_os("FFF_MCP_SERVICE_DIR").map(PathBuf::from))
            .unwrap_or_else(crate::default_directory);
        // The service resolves paths from its own working directory, and equal databases
        // must be spelled alike to share an index.
        let mut options = config.options.clone();
        options.frecency_db = options
            .frecency_db
            .map(crate::stable_database_path)
            .transpose()?;
        Ok(Self {
            directory: std::path::absolute(directory)?,
            request: request(std::path::absolute(&config.scope)?, options),
            launcher: Launcher {
                executable: std::path::absolute(&config.executable)?,
                idle_timeout_secs: config.idle_timeout_secs,
                log_file: config
                    .log_file
                    .as_deref()
                    .map(std::path::absolute)
                    .transpose()?,
                log_level: config.log_level.clone(),
                // Programs set it through FFF_FULL_PARALLEL_INDEXING, which the service inherits.
                full_parallel_indexing: false,
            },
            connect_timeout: config.connect_timeout,
            response_timeout: config.response_timeout,
        })
    }

    fn connect(
        &self,
        runtime: &tokio::runtime::Runtime,
    ) -> Result<(Connection, Vec<String>), ClientError> {
        self.connect_within(runtime, self.connect_timeout)
    }

    fn connect_within(
        &self,
        runtime: &tokio::runtime::Runtime,
        timeout: Duration,
    ) -> Result<(Connection, Vec<String>), ClientError> {
        runtime.block_on(self.open(timeout))
    }

    // Opens a session within `timeout`, starting the service when none runs.
    pub(crate) async fn open(
        &self,
        timeout: Duration,
    ) -> Result<(Connection, Vec<String>), ClientError> {
        let namespace = Namespace::open(&self.directory).map_err(ClientError::Namespace)?;
        open_session(
            &namespace,
            &Build::current(),
            &self.request,
            &self.launcher,
            timeout,
        )
        .await
    }

    // How long a call may take before the session gives up on the service.
    fn time_limit(&self, request: &ProgramRequest) -> Duration {
        let response = self.response_timeout;
        match request {
            ProgramRequest::WaitReady { timeout_ms } => {
                Duration::from_millis(*timeout_ms).min(MAX_WAIT_READY) + response
            }
            ProgramRequest::Grep(grep) => {
                Duration::from_millis(grep.options.time_budget_ms) + response
            }
            ProgramRequest::MultiGrep(grep) => {
                Duration::from_millis(grep.options.time_budget_ms) + response
            }
            ProgramRequest::Close => CLOSE_TIMEOUT.min(response),
            _ => response,
        }
    }
}
