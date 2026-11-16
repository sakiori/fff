use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use fff::file_picker::{FilePicker, FilePickerIgnoreOptions};
use fff::scope::SearchScope;
use fff::{
    BackgroundStatus, FFFMode, FilePickerOptions, ListenerSubscription, SharedFilePicker,
    SharedFrecency,
};
use fff_service::protocol::{IndexOptions, Readiness, ServiceError, Watcher};

const READINESS_POLL: Duration = Duration::from_millis(20);

pub(crate) fn allows_root(options: &IndexOptions, root: &Path) -> bool {
    fff::path_utils::validate_base_path(root, options.enable_root_scan, options.enable_home_scan)
        .is_ok()
}

// An index to build: its root, repository context and the options it is built with.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct IndexKey {
    pub root: PathBuf,
    pub repository: Option<PathBuf>,
    pub options: IndexOptions,
}

pub(crate) struct Index {
    pub root: PathBuf,
    pub repository: Option<PathBuf>,
    pub options: IndexOptions,
    pub id: String,
    pub picker: SharedFilePicker,
    pub warnings: Vec<String>,
    frecency: SharedFrecency,
    background: BackgroundStatus,
    ready: tokio::sync::watch::Sender<bool>,
    _coverage: ListenerSubscription,
}

impl Index {
    // Starts the scan and returns without waiting for it; see `wait_ready`.
    // `on_coverage_change` runs on indexing threads and must only hand off work.
    pub fn create(
        key: IndexKey,
        on_coverage_change: impl Fn() + Send + Sync + 'static,
    ) -> Result<Arc<Self>, ServiceError> {
        let IndexKey {
            root,
            repository,
            options,
        } = key;
        let index_error = |reason: String| ServiceError::Index {
            root: root.clone(),
            reason,
        };
        let frecency = SharedFrecency::default();
        let mut warnings = Vec::new();
        if let Some(path) = &options.frecency_db
            && let Err(error) = fff::frecency::FrecencyTracker::open(path)
                .and_then(|tracker| frecency.init(tracker))
        {
            tracing::warn!(%error, path, "Frecency database unavailable; ranking without it");
            warnings.push(format!(
                "Warning: failed to open frecency database {path}: {error}"
            ));
        }
        let picker = SharedFilePicker::default();
        let coverage = picker.subscribe_coverage(on_coverage_change);
        FilePicker::new_with_shared_state_and_ignore_options(
            picker.clone(),
            frecency.clone(),
            FilePickerOptions {
                base_path: root
                    .to_str()
                    .ok_or_else(|| index_error("path is not valid UTF-8".into()))?
                    .to_owned(),
                enable_mmap_cache: options.warmup,
                enable_content_indexing: options.content_indexing,
                watch: options.watch,
                follow_symlinks: options.follow_symlinks,
                cache_budget: options
                    .max_cached_files
                    .map(fff::ContentCacheBudget::with_max_files),
                enable_home_dir_scanning: options.enable_home_scan,
                enable_fs_root_scanning: options.enable_root_scan,
                mode: FFFMode::Ai,
                ..Default::default()
            },
            FilePickerIgnoreOptions {
                extra_ignore: options.extra_ignore.clone().into(),
                disable_git: options.disable_git,
                disable_ignore_files: options.disable_ignore_files,
            },
        )
        .map_err(|e| index_error(e.to_string()))?;
        let background = picker
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().map(FilePicker::background_status))
            .ok_or_else(|| index_error("file picker was not published".into()))?;
        Ok(Arc::new(Self {
            root,
            repository,
            options,
            id: uuid::Uuid::new_v4().to_string(),
            picker,
            warnings,
            frecency,
            background,
            ready: tokio::sync::watch::Sender::new(false),
            _coverage: coverage,
        }))
    }

    pub fn readiness(&self) -> Readiness {
        if *self.ready.borrow() {
            Readiness::Ready
        } else {
            Readiness::Scanning
        }
    }

    pub fn is_ready(&self) -> bool {
        self.readiness() == Readiness::Ready
    }

    // A scan runs or waits to run, post-scan phase included.
    pub fn is_busy(&self) -> bool {
        self.background.is_scanning() || self.background.is_scan_pending()
    }

    // Files the current or last scan has walked so far, read without the index lock.
    pub fn scanned_files(&self) -> usize {
        self.background.scanned_files()
    }

    // Starts a full rescan; during a scan, one more right after it, which later requests join.
    pub fn rescan(&self) -> Result<(), ServiceError> {
        self.picker
            .trigger_full_rescan_async(&self.frecency)
            .map_err(|error| ServiceError::Index {
                root: self.root.clone(),
                reason: error.to_string(),
            })?;
        Ok(())
    }

    // Applies changes a program reports to the index as the watcher would.
    pub fn apply_changes(&self, paths: Vec<PathBuf>) -> fff::ReportedChanges {
        self.picker.apply_reported_changes(&self.frecency, paths)
    }

    // Returns false when a scan still runs or waits to run at `deadline`.
    pub async fn wait_scanned(&self, deadline: tokio::time::Instant) -> bool {
        while self.is_busy() {
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(READINESS_POLL).await;
        }
        true
    }

    pub fn watcher(&self) -> Watcher {
        match self.background.watcher() {
            fff::WatcherStatus::Disabled => Watcher::Disabled,
            fff::WatcherStatus::Starting => Watcher::Starting,
            fff::WatcherStatus::Ready => Watcher::Ready,
            fff::WatcherStatus::Failed => Watcher::Failed,
            fff::WatcherStatus::Stopped => Watcher::Stopped,
        }
    }

    // A watcher that runs or is starting, as a watch subscription needs.
    pub fn watcher_runs(&self) -> bool {
        matches!(self.watcher(), Watcher::Starting | Watcher::Ready)
    }

    pub fn depth(&self) -> usize {
        self.root.components().count()
    }

    // The file set must match; capabilities may exceed the request, and a request that
    // names a frecency database only accepts that one.
    pub fn serves(&self, request: &IndexOptions) -> bool {
        let own = &self.options;
        same_file_set(own, request)
            && offers(own, request)
            && (request.frecency_db.is_none() || own.frecency_db == request.frecency_db)
    }

    // Whether this index offers every capability of `other` and more.
    pub fn extends(&self, other: &Index) -> bool {
        offers(&self.options, &other.options) && !offers(&other.options, &self.options)
    }

    // Broader coverage first, then more capabilities, so fewer sessions move later.
    pub fn preference(&self) -> (usize, std::cmp::Reverse<u8>) {
        let own = &self.options;
        let capabilities = own.content_indexing as u8 + own.watch as u8 + own.warmup as u8;
        (self.depth(), std::cmp::Reverse(capabilities))
    }

    // Returns false when the index is not ready by `deadline`.
    pub async fn wait_ready(&self, deadline: tokio::time::Instant) -> bool {
        let mut ready = self.ready.subscribe();
        tokio::time::timeout_at(deadline, ready.wait_for(|ready| *ready))
            .await
            .is_ok_and(|result| result.is_ok())
    }

    // Publishes readiness once the scan and a requested watcher settle, however long that takes.
    pub async fn monitor(index: Weak<Self>) -> Option<Arc<Self>> {
        loop {
            let current = index.upgrade()?;
            if !current.background.is_scanning()
                && current.watcher() != Watcher::Starting
                && !readiness_held()
            {
                if current.watcher() == Watcher::Failed {
                    tracing::warn!(
                        root = %current.root.display(),
                        "Filesystem watcher failed; serving this index without live updates"
                    );
                }
                current.ready.send_replace(true);
                return Some(current);
            }
            drop(current);
            tokio::time::sleep(READINESS_POLL).await;
        }
    }

    // Meaningful once ready: whether `scope` is indexed with compatible exclusion semantics.
    pub fn covers(&self, scope: &Path) -> bool {
        let Ok(scope) = SearchScope::new(&self.root, scope) else {
            return false;
        };
        if scope.is_root() {
            return true;
        }
        if !self.keeps_exclusions(&scope) {
            return false;
        }
        let Ok(guard) = self.picker.read() else {
            return false;
        };
        guard
            .as_ref()
            .is_some_and(|picker| picker.contains_scope(&scope))
    }

    // Coverage that is known without a scan: the exclusion semantics of an unscanned subtree.
    pub fn may_cover(&self, scope: &Path) -> bool {
        SearchScope::new(&self.root, scope).is_ok_and(|scope| self.keeps_exclusions(&scope))
    }

    // Whether the index holds below `scope` the files an index of `scope` would. Extra rules
    // are relative to the session's directory, so they hold at the root only.
    fn keeps_exclusions(&self, scope: &SearchScope) -> bool {
        if scope.is_root() {
            return true;
        }
        if !self.options.extra_ignore.is_empty() {
            return false;
        }
        // A repository index applies the repository's own ignore rules at every depth.
        if self.repository.is_some() {
            return true;
        }
        let dir = Path::new(scope.dir());
        (self.options.disable_git || fff::ignore::subtree_keeps_ignored_dirs(dir))
            && !self.ignore_files_above(dir)
    }

    // Outside a repository a walk reads no ignore file above its root, so one in this index's
    // root or between it and `dir` excludes files an index of `dir` would hold.
    fn ignore_files_above(&self, dir: &Path) -> bool {
        let names = [
            (!self.options.disable_git).then_some(".gitignore"),
            (!self.options.disable_ignore_files).then_some(".ignore"),
        ];
        let honored = |at: &Path| {
            (names.iter().flatten()).any(|name| at.join(name).symlink_metadata().is_ok())
        };
        let mut current = self.root.clone();
        let mut parts = dir.components().peekable();
        loop {
            if honored(&current) {
                return true;
            }
            match parts.next() {
                Some(part) if parts.peek().is_some() => current.push(part),
                _ => return false,
            }
        }
    }

    pub fn search_scope(&self, scope: &Path) -> Result<SearchScope, ServiceError> {
        SearchScope::new(&self.root, scope).map_err(|e| ServiceError::Scope {
            path: scope.to_path_buf(),
            reason: e.to_string(),
        })
    }
}

// In debug builds, tests keep indexes building while the file this names exists.
fn readiness_held() -> bool {
    cfg!(debug_assertions)
        && std::env::var_os("FFF_MCP_TEST_HOLD_READINESS")
            .is_some_and(|file| Path::new(&file).exists())
}

// Whether indexes built with either options hold the same files.
pub(crate) fn same_file_set(a: &IndexOptions, b: &IndexOptions) -> bool {
    a.follow_symlinks == b.follow_symlinks
        && a.extra_ignore == b.extra_ignore
        && a.disable_git == b.disable_git
        && a.disable_ignore_files == b.disable_ignore_files
}

// Whether `own` offers every capability `request` needs.
pub(crate) fn offers(own: &IndexOptions, request: &IndexOptions) -> bool {
    (own.content_indexing || !request.content_indexing)
        && (own.watch || !request.watch)
        && (own.warmup || !request.warmup)
        && caches_enough(own.max_cached_files, request.max_cached_files)
}

// Raises the capabilities of `options` to those `other` offers, keeping its file set policy.
pub(crate) fn upgrade(options: &mut IndexOptions, other: &IndexOptions) {
    options.content_indexing |= other.content_indexing;
    options.watch |= other.watch;
    options.warmup |= other.warmup;
    let (own, wanted) = (options.max_cached_files, other.max_cached_files);
    if !caches_enough(own, wanted) {
        options.max_cached_files = if caches_enough(wanted, own) {
            wanted
        } else {
            // A core budget and an explicit one between its bounds: an explicit one above both.
            own.max(wanted)
                .map(|n| n.max(fff::ContentCacheBudget::AUTO_MAX_FILES))
        };
    }
}

// `None` is the core's size-based budget, which caches between its bounds.
fn caches_enough(own: Option<usize>, request: Option<usize>) -> bool {
    use fff::ContentCacheBudget as Budget;
    match (own, request) {
        (None, None) => true,
        (Some(own), Some(request)) => own >= request,
        (None, Some(request)) => request <= Budget::AUTO_MIN_FILES,
        (Some(own), None) => own >= Budget::AUTO_MAX_FILES,
    }
}

impl Drop for Index {
    // Cancels a scan in flight and stops the watcher; the scan never installs one afterwards.
    fn drop(&mut self) {
        if let Ok(mut guard) = self.picker.write()
            && let Some(picker) = guard.as_mut()
        {
            picker.cancel();
            picker.stop_background_monitor();
        }
    }
}

// Cached repository identity; discovery reruns only when the nearest `.git` entry moves.
pub(crate) struct RepositoryProbe {
    marker: Option<PathBuf>,
    root: Option<PathBuf>,
    // Without Git integration a scope belongs to no repository.
    disabled: bool,
}

impl RepositoryProbe {
    pub fn discover(scope: &Path, options: &IndexOptions) -> Self {
        if options.disable_git {
            return Self {
                marker: None,
                root: None,
                disabled: true,
            };
        }
        Self {
            marker: git_marker(scope),
            root: repository_root(scope),
            disabled: false,
        }
    }

    pub fn root(&self) -> Option<&PathBuf> {
        self.root.as_ref()
    }

    // Returns true when the repository containing `scope` changed.
    pub fn refresh(&mut self, scope: &Path) -> bool {
        if self.disabled {
            return false;
        }
        let marker = git_marker(scope);
        if marker == self.marker {
            return false;
        }
        self.marker = marker;
        let root = repository_root(scope);
        let changed = root != self.root;
        self.root = root;
        changed
    }
}

fn git_marker(scope: &Path) -> Option<PathBuf> {
    scope
        .ancestors()
        .find(|dir| dir.join(".git").symlink_metadata().is_ok())
        .map(Path::to_path_buf)
}

fn repository_root(path: &Path) -> Option<PathBuf> {
    fff::path_utils::canonicalize(git2::Repository::discover(path).ok()?.workdir()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(warmup: bool, max_cached_files: Option<usize>) -> IndexOptions {
        IndexOptions {
            warmup,
            content_indexing: true,
            watch: true,
            follow_symlinks: false,
            max_cached_files,
            frecency_db: None,
            enable_home_scan: false,
            enable_root_scan: false,
            extra_ignore: Vec::new(),
            disable_git: false,
            disable_ignore_files: false,
        }
    }

    #[test]
    fn warmup_and_cache_budget_are_capabilities() {
        let agent = options(true, None);
        let app = options(false, Some(100));
        assert!(offers(&agent, &app));
        assert!(!offers(&app, &agent));
        assert!(offers(&options(true, Some(50_000)), &agent));
        assert!(!offers(&agent, &options(true, Some(20_000))));
        let mut upgraded = app.clone();
        upgrade(&mut upgraded, &agent);
        assert!(offers(&upgraded, &agent) && offers(&upgraded, &app));
        let mut between = options(false, Some(20_000));
        upgrade(&mut between, &agent);
        assert!(offers(&between, &agent) && offers(&between, &options(false, Some(20_000))));
        assert_eq!(between.max_cached_files, Some(30_000));
    }

    #[test]
    fn repository_probe_detects_new_repositories() {
        let temp = tempfile::tempdir().unwrap();
        let scope = temp.path().canonicalize().unwrap().join("scope");
        std::fs::create_dir(&scope).unwrap();
        let mut probe = RepositoryProbe::discover(&scope, &options(false, None));
        let outer = probe.root().cloned();
        assert!(!probe.refresh(&scope));
        git2::Repository::init(&scope).unwrap();
        assert!(probe.refresh(&scope));
        assert_ne!(probe.root().cloned(), outer);
        assert_eq!(probe.root(), Some(&scope));

        let without_git = IndexOptions {
            disable_git: true,
            ..options(false, None)
        };
        let mut probe = RepositoryProbe::discover(&scope, &without_git);
        assert_eq!(probe.root(), None);
        assert!(!probe.refresh(&scope));
        assert!(!probe.refresh(&scope));
    }
}
