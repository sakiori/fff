use std::collections::HashMap;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use fff_service::client::ClientError;
use fff_service::program::{
    GlobRequest, GrepCursor, GrepRequest, GrepResults, MultiGrepRequest, ProgramError,
    ProgramRequest, SearchRequest,
};
use fff_service::protocol::{IndexOptions, ServiceError};
use fff_service::session::{CONNECT_TIMEOUT, ProgramConfig, ProgramSession, RESPONSE_TIMEOUT};
use fff_service::watch::{Delivery, Subscription, WatchEventKind, WatchRequest};

use crate::watch::{FFF_WATCH_EVENT_READY, FFF_WATCH_EVENT_REPOSITORY};

use crate::ffi_types::{
    FFF_ERROR_FAILED, FFF_ERROR_INVALID_REQUEST, FFF_ERROR_LOST_SERVICE, FFF_ERROR_OUTSIDE_SCOPE,
    FFF_ERROR_RETRY, FFF_ERROR_SCOPE_UNAVAILABLE, FFF_ERROR_STALE_CURSOR, FFF_ERROR_UNAVAILABLE,
    FffDirSearchResult, FffGrepResult, FffMixedSearchResult, FffResult, FffScanProgress,
    FffSearchResult,
};

// The default of `fff-mcp --idle-timeout-secs`, so a service a program starts behaves alike.
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 60 * 60;

// An instance whose index lives in the shared service.
pub(crate) struct ServiceInstance {
    session: ProgramSession,
    // What subscriptions open their connections with.
    config: ProgramConfig,
    watch_callback: Arc<crate::watch::WatchCallbackSlot>,
    watches: Mutex<HashMap<u64, Subscription>>,
    base_path: PathBuf,
    ai_syntax: bool,
    frecency_db: Option<String>,
    // The cursor of the last grep page, which the integer offsets of the grep functions continue.
    last_cursor: Mutex<Option<GrepCursor>>,
}

// What `fff_create_service_instance` opens a session with.
pub(crate) struct ServiceSettings {
    pub base_path: PathBuf,
    pub options: IndexOptions,
    pub ai_syntax: bool,
    pub executable: PathBuf,
    pub service_dir: Option<PathBuf>,
    pub log_file: Option<PathBuf>,
    pub log_level: Option<String>,
    // Bounds opening the session and each call; None takes the session's defaults.
    pub timeout: Option<Duration>,
}

impl ServiceInstance {
    pub(crate) fn open(settings: ServiceSettings) -> Result<Self, ClientError> {
        let idle_timeout_secs = std::env::var("FFF_MCP_IDLE_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS);
        let frecency_db = settings.options.frecency_db.clone();
        let config = ProgramConfig {
            scope: settings.base_path.clone(),
            options: settings.options,
            executable: settings.executable,
            service_dir: settings.service_dir,
            idle_timeout_secs,
            log_file: settings.log_file,
            log_level: settings.log_level,
            connect_timeout: settings.timeout.unwrap_or(CONNECT_TIMEOUT),
            response_timeout: settings.timeout.unwrap_or(RESPONSE_TIMEOUT),
        };
        let session = ProgramSession::open(&config)?;
        Ok(Self {
            session,
            config,
            watch_callback: Arc::default(),
            watches: Mutex::default(),
            base_path: settings.base_path,
            ai_syntax: settings.ai_syntax,
            frecency_db,
            last_cursor: Mutex::new(None),
        })
    }

    // Ends the subscriptions first: the callback may be freed once this returns.
    pub(crate) fn close(self) {
        let watches = std::mem::take(&mut *self.watches());
        drop(watches);
        self.watch_callback.clear();
        let _ = self.session.close();
    }

    pub(crate) fn watch_callback(&self) -> &crate::watch::WatchCallbackSlot {
        &self.watch_callback
    }

    fn watches(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Subscription>> {
        self.watches.lock().unwrap_or_else(PoisonError::into_inner)
    }

    // Opens a subscription of the instance's directory on the service.
    pub(crate) fn watch(
        &self,
        pattern: &str,
        options: fff::WatchOptions,
        repository: bool,
    ) -> *mut FffResult {
        if !self.config.options.watch {
            return FffResult::err_with(
                "Watching is unavailable on an instance created without watch",
                FFF_ERROR_UNAVAILABLE,
            );
        }
        if !self.watch_callback.is_set() {
            return FffResult::err(
                "No watch callback registered. Call fff_set_watch_callback first.",
            );
        }
        let relative = |pattern: &str| self.relative_pattern(pattern);
        let request = match (
            relative(pattern),
            options.ignore.iter().map(|p| relative(p)).collect(),
        ) {
            (Ok(pattern), Ok(ignore)) => WatchRequest {
                pattern,
                ignore,
                repository,
            },
            (Err(error), _) | (_, Err(error)) => return error,
        };
        // The ids of in-process subscriptions, which bindings route through the same map.
        let id = fff::WatchId::next().0;
        let slot = self.watch_callback.clone();
        let base = self.base_path.clone();
        let deliver = move |delivery: Delivery| {
            let events = match delivery {
                Delivery::Events(events) => events,
                Delivery::Ready => {
                    return slot.deliver(id, &[(base.as_path(), FFF_WATCH_EVENT_READY, None)]);
                }
                Delivery::Failed(error) => {
                    let message = format!("The watch subscription ended: {error}");
                    return slot.deliver_failure(id, &base, &message, error_code(&error));
                }
            };
            // An empty path names the directory itself, which a join would end with a separator.
            let at = |path: &str| match path {
                "" => base.clone(),
                path => base.join(path),
            };
            let paths: Vec<_> = events
                .iter()
                .map(|event| (at(&event.path), event.from.as_deref().map(at)))
                .collect();
            let batch: Vec<_> = events
                .iter()
                .zip(&paths)
                .map(|(event, (path, from))| (path.as_path(), kind(event.kind), from.as_deref()))
                .collect();
            slot.deliver(id, &batch);
        };
        match Subscription::start(&self.config, request, deliver) {
            Ok(subscription) => {
                self.watches().insert(id, subscription);
                FffResult::ok_int(id as i64)
            }
            Err(error) => failure(&error, "Failed to subscribe: "),
        }
    }

    // Returns whether `id` was a subscription of this instance.
    pub(crate) fn unwatch(&self, id: u64) -> bool {
        let subscription = self.watches().remove(&id);
        subscription.is_some()
    }

    // Patterns travel relative to the instance's directory, which may be spelled otherwise.
    fn relative_pattern(&self, pattern: &str) -> Result<String, *mut FffResult> {
        let expanded = fff::path_utils::expand_tilde(pattern);
        if !expanded.is_absolute() {
            return Ok(pattern.to_owned());
        }
        let inside = |path: &Path| {
            path.strip_prefix(&self.base_path)
                .ok()
                .map(|relative| relative.to_string_lossy().into_owned())
        };
        inside(&expanded)
            .or_else(|| {
                fff::path_utils::canonicalize(&expanded)
                    .ok()
                    .and_then(|path| inside(&path))
            })
            .ok_or_else(|| {
                FffResult::err_with(
                    &format!("{pattern} is outside the instance's directory"),
                    FFF_ERROR_OUTSIDE_SCOPE,
                )
            })
    }

    pub(crate) fn base_path(&self) -> &Path {
        &self.base_path
    }

    pub(crate) fn ai_syntax(&self) -> bool {
        self.ai_syntax
    }

    pub(crate) fn search(&self, request: SearchRequest) -> *mut FffResult {
        reply(self.session.search_files(request), |results| {
            FffSearchResult::from_service(&results) as *mut c_void
        })
    }

    pub(crate) fn glob(&self, request: GlobRequest) -> *mut FffResult {
        reply(self.session.glob(request), |results| {
            FffSearchResult::from_service(&results) as *mut c_void
        })
    }

    pub(crate) fn search_directories(&self, request: SearchRequest) -> *mut FffResult {
        reply(self.session.search_directories(request), |results| {
            FffDirSearchResult::from_service(&results) as *mut c_void
        })
    }

    pub(crate) fn search_mixed(&self, request: SearchRequest) -> *mut FffResult {
        reply(self.session.search_mixed(request), |results| {
            FffMixedSearchResult::from_service(&results) as *mut c_void
        })
    }

    // An integer offset names no index, so it continues only the last grep page.
    pub(crate) fn cursor(&self, offset: u32) -> Result<Option<GrepCursor>, *mut FffResult> {
        if offset == 0 {
            return Ok(None);
        }
        let last = self
            .last_cursor
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match last.as_ref() {
            Some(cursor) if cursor.offset == offset as usize => Ok(Some(cursor.clone())),
            _ => Err(FffResult::err_with(
                "A service instance continues only its last grep page; search again without the offset",
                FFF_ERROR_STALE_CURSOR,
            )),
        }
    }

    pub(crate) fn grep(&self, request: GrepRequest) -> *mut FffResult {
        let results = self.session.grep(request);
        self.remember(&results);
        reply(results, |results| {
            FffGrepResult::from_service(&results) as *mut c_void
        })
    }

    pub(crate) fn multi_grep(&self, request: MultiGrepRequest) -> *mut FffResult {
        let results = self.session.multi_grep(request);
        self.remember(&results);
        reply(results, |results| {
            FffGrepResult::from_service(&results) as *mut c_void
        })
    }

    fn remember(&self, results: &Result<GrepResults, ClientError>) {
        if let Ok(results) = results {
            *self
                .last_cursor
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = results.next_cursor.clone();
        }
    }

    // One request of the program protocol as JSON, answered with the whole reply as JSON.
    pub(crate) fn call_json(&self, request: &str) -> *mut FffResult {
        let mut request: ProgramRequest = match serde_json::from_str(request) {
            Ok(request) => request,
            Err(error) => {
                return FffResult::err_with(
                    &format!("Invalid service request: {error}"),
                    FFF_ERROR_INVALID_REQUEST,
                );
            }
        };
        match &mut request {
            ProgramRequest::Close => {
                return FffResult::err_with(
                    "Close a service instance with fff_destroy",
                    FFF_ERROR_INVALID_REQUEST,
                );
            }
            ProgramRequest::Grep(grep) => grep.ai_syntax = self.ai_syntax,
            _ => {}
        }
        let reply = match self.session.request(request) {
            Ok(reply) => reply,
            Err(error) => return failure(&error, ""),
        };
        match crate::packed::reply_json(&reply) {
            Ok(json) => FffResult::ok_string(&json),
            Err(error) => FffResult::err(&format!("Cannot encode the service reply: {error}")),
        }
    }

    pub(crate) fn rescan(&self) -> *mut FffResult {
        match self.session.rescan() {
            Ok(()) => FffResult::ok_empty(),
            Err(error) => failure(&error, "Failed to trigger rescan: "),
        }
    }

    // An unreachable service reports no scan; fff_get_scan_progress and fff_service_call report the error.
    pub(crate) fn is_scanning(&self) -> bool {
        self.session.is_scanning().unwrap_or(false)
    }

    pub(crate) fn scan_progress(&self) -> *mut FffResult {
        reply(self.session.progress(), |progress| {
            Box::into_raw(Box::new(FffScanProgress::from(progress))) as *mut c_void
        })
    }

    pub(crate) fn wait_ready(&self, timeout: Duration) -> *mut FffResult {
        match self.session.wait_ready(timeout) {
            Ok(ready) => FffResult::ok_int(ready as i64),
            Err(error) => failure(&error, ""),
        }
    }

    // A restart in the same directory is a rescan; the session cannot move to another one.
    pub(crate) fn restart(&self, path: &Path) -> *mut FffResult {
        if path == self.base_path {
            return self.rescan();
        }
        FffResult::err_with(
            "Restarting the index in another directory is unavailable over the shared service; create a new instance",
            FFF_ERROR_UNAVAILABLE,
        )
    }

    // Adds the git, file picker, frecency and query tracker sections of a health check, or
    // fails as soon as the instance loses its service, as other calls do.
    pub(crate) fn health(
        &self,
        test_path: Option<&Path>,
        health: &mut serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), *mut FffResult> {
        // The session's repository state answers for its own directory.
        let own = test_path.is_none_or(|path| {
            fff::path_utils::canonicalize(path).is_ok_and(|path| path == self.base_path)
        });
        let git = match (own, test_path) {
            (false, Some(path)) => crate::discover_workdir(path),
            _ => match self.session.repository() {
                // git2 reports a work tree with a trailing separator.
                Ok(Some(root)) => Ok(Some(root.join(""))),
                Ok(None) => Err("no git work tree contains the directory".to_owned()),
                Err(error) if error.lost_service() => return Err(failure(&error, "")),
                Err(error) => Err(error.to_string()),
            },
        };
        health.insert("git".into(), crate::git_health(git));
        let progress = self.session.progress();
        if let Err(error) = &progress
            && error.lost_service()
        {
            return Err(failure(error, ""));
        }
        health.insert(
            "file_picker".into(),
            serde_json::json!({
                "initialized": true,
                "base_path": self.base_path.to_string_lossy(),
                "is_scanning": progress.as_ref().is_ok_and(|p| p.scanning),
                "indexed_files": progress.as_ref().map_or(0, |p| p.files),
            }),
        );
        let unusable = self
            .session
            .warnings()
            .iter()
            .find(|warning| warning.contains("frecency"));
        let mut frecency = serde_json::json!({
            "initialized": self.frecency_db.is_some() && unusable.is_none(),
        });
        if let Some(warning) = unusable {
            frecency["error"] = warning.clone().into();
        }
        health.insert("frecency".into(), frecency);
        health.insert(
            "query_tracker".into(),
            serde_json::json!({ "initialized": false }),
        );
        Ok(())
    }
}

// The `kind` byte of the C interface.
fn kind(kind: WatchEventKind) -> u8 {
    match kind {
        WatchEventKind::Created => 0,
        WatchEventKind::Modified => 1,
        WatchEventKind::Removed => 2,
        WatchEventKind::Rescan => 3,
        WatchEventKind::Renamed => 4,
        WatchEventKind::Repository => FFF_WATCH_EVENT_REPOSITORY,
    }
}

pub(crate) fn unavailable(call: &str) -> *mut FffResult {
    FffResult::err_with(
        &format!("{call} is unavailable over the shared service"),
        FFF_ERROR_UNAVAILABLE,
    )
}

fn reply<T>(
    result: Result<T, ClientError>,
    payload: impl FnOnce(T) -> *mut c_void,
) -> *mut FffResult {
    match result {
        Ok(value) => FffResult::ok_handle(payload(value)),
        Err(error) => failure(&error, ""),
    }
}

pub(crate) fn failure(error: &ClientError, context: &str) -> *mut FffResult {
    FffResult::err_with(&format!("{context}{error}"), error_code(error))
}

// The kind of a failure, from the error the session or the service reported.
fn error_code(error: &ClientError) -> i64 {
    let service = match error {
        _ if error.lost_service() => return FFF_ERROR_LOST_SERVICE,
        ClientError::Program(ProgramError::StaleCursor) => return FFF_ERROR_STALE_CURSOR,
        ClientError::Program(ProgramError::Invalid(_)) => return FFF_ERROR_INVALID_REQUEST,
        ClientError::Program(ProgramError::OutsideScope(_)) => return FFF_ERROR_OUTSIDE_SCOPE,
        // A session rebinding to another index; only a readiness wait could meet a building
        // index, and it reports one as not ready instead.
        ClientError::Program(ProgramError::Unstable | ProgramError::Building) => {
            return FFF_ERROR_RETRY;
        }
        ClientError::Program(ProgramError::Service(service)) | ClientError::Service(service) => {
            service
        }
        _ => return FFF_ERROR_FAILED,
    };
    match service {
        ServiceError::Scope { .. }
        | ServiceError::ScopeRefused { .. }
        | ServiceError::Index { .. } => FFF_ERROR_SCOPE_UNAVAILABLE,
        ServiceError::InvalidRequest { .. } => FFF_ERROR_INVALID_REQUEST,
        _ => FFF_ERROR_FAILED,
    }
}
