use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use fff::file_picker::FilePicker;
use fff::scope::SearchScope;
use fff::{
    BackgroundStatus, CoverageSubscription, FFFMode, FilePickerOptions, SharedFilePicker,
    SharedFrecency,
};
use serde::{Deserialize, Serialize};

use crate::protocol::ServiceError;

const READINESS_POLL: Duration = Duration::from_millis(20);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct IndexOptions {
    pub warmup: bool,
    pub content_indexing: bool,
    pub watch: bool,
    pub follow_symlinks: bool,
    pub max_cached_files: Option<usize>,
    pub frecency_db: Option<String>,
    pub enable_home_scan: bool,
    pub enable_root_scan: bool,
}

impl IndexOptions {
    pub fn allows_root(&self, root: &Path) -> bool {
        fff::path_utils::validate_base_path(root, self.enable_root_scan, self.enable_home_scan)
            .is_ok()
    }
}

// Everything that decides whether two requests may share one index.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct IndexKey {
    pub root: PathBuf,
    pub repository: Option<PathBuf>,
    pub options: IndexOptions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Readiness {
    Scanning,
    Ready,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Watcher {
    Disabled,
    Starting,
    Ready,
    Failed,
    Stopped,
}

impl From<fff::WatcherStatus> for Watcher {
    fn from(status: fff::WatcherStatus) -> Self {
        match status {
            fff::WatcherStatus::Disabled => Self::Disabled,
            fff::WatcherStatus::Starting => Self::Starting,
            fff::WatcherStatus::Ready => Self::Ready,
            fff::WatcherStatus::Failed => Self::Failed,
            fff::WatcherStatus::Stopped => Self::Stopped,
        }
    }
}

pub(crate) struct Index {
    pub root: PathBuf,
    pub repository: Option<PathBuf>,
    pub options: IndexOptions,
    pub id: String,
    pub picker: SharedFilePicker,
    pub warnings: Vec<String>,
    background: BackgroundStatus,
    ready: tokio::sync::watch::Sender<bool>,
    _coverage: CoverageSubscription,
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
        FilePicker::new_with_shared_state(
            picker.clone(),
            frecency,
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

    pub fn watcher(&self) -> Watcher {
        self.background.watcher().into()
    }

    pub fn depth(&self) -> usize {
        self.root.components().count()
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
            if !current.background.is_scanning() && current.watcher() != Watcher::Starting {
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

    // A repository index applies the repository's own ignore rules at every depth.
    fn keeps_exclusions(&self, scope: &SearchScope) -> bool {
        self.repository.is_some() || fff::ignore::subtree_keeps_ignored_dirs(Path::new(scope.dir()))
    }

    pub fn search_scope(&self, scope: &Path) -> Result<SearchScope, ServiceError> {
        SearchScope::new(&self.root, scope).map_err(|e| ServiceError::Scope {
            path: scope.to_path_buf(),
            reason: e.to_string(),
        })
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
}

impl RepositoryProbe {
    pub fn discover(scope: &Path) -> Self {
        Self {
            marker: git_marker(scope),
            root: repository_root(scope),
        }
    }

    pub fn root(&self) -> Option<&PathBuf> {
        self.root.as_ref()
    }

    // Returns true when the repository containing `scope` changed.
    pub fn refresh(&mut self, scope: &Path) -> bool {
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

    #[test]
    fn repository_probe_detects_new_repositories() {
        let temp = tempfile::tempdir().unwrap();
        let scope = temp.path().canonicalize().unwrap().join("scope");
        std::fs::create_dir(&scope).unwrap();
        let mut probe = RepositoryProbe::discover(&scope);
        let outer = probe.root().cloned();
        assert!(!probe.refresh(&scope));
        git2::Repository::init(&scope).unwrap();
        assert!(probe.refresh(&scope));
        assert_ne!(probe.root().cloned(), outer);
        assert_eq!(probe.root(), Some(&scope));
        assert!(!probe.refresh(&scope));
    }
}
