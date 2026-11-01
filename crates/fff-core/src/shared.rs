use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak};
use std::time::{Duration, Instant};

use crate::dbs::lmdb::{LmdbStore, spawn_lmdb_gc};
use crate::error::Error;
use crate::file_picker::FilePicker;
use crate::frecency::FrecencyTracker;
use crate::git::GitStatusCache;
use crate::git_recency;
use crate::git_status_worker::GitStatusWorker;
use crate::listeners::{CoverageListeners, ListenerSubscription, RefListeners, WorkTreeListeners};
use crate::query_tracker::QueryTracker;
use crate::rescan_stats::{RescanCounters, RescanReason, RescanStats};
use crate::rescan_throttle::{Admission, RescanThrottle};
use crate::scan::ScanJob;
use crate::watch::{EventHold, WatchEvent, WatchId, WatchOptions, WatchRegistry};
use git2::Repository;

/// What became of changes a program reported with [`SharedFilePicker::apply_reported_changes`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportedChanges {
    /// The index holds them.
    Applied,
    /// A scan runs; it applies them once it commits.
    Held,
}

/// Poll `<git_dir>/index.lock` until it disappears (git write completed), giving up
/// after [`GIT_LOCK_MAX_WAIT`]. Used by [`SharedPicker::refresh_git_status`]
/// to avoid reading a half-updated index when the watcher fires mid-`git add`.
///
/// The wait is bounded and cheap: the lock file is typically cleared within
/// a few milliseconds of the git command exiting.
fn wait_for_git_index_lock_release(git_dir: &Path) {
    const GIT_LOCK_POLL: Duration = Duration::from_millis(10);
    const GIT_LOCK_MAX_WAIT: Duration = Duration::from_millis(500);

    let lock = git_dir.join("index.lock");
    // Fast path: no lock present.
    if !lock.exists() {
        return;
    }
    let deadline = Instant::now() + GIT_LOCK_MAX_WAIT;
    while lock.exists() && Instant::now() < deadline {
        std::thread::sleep(GIT_LOCK_POLL);
    }
    if lock.exists() {
        tracing::warn!(
            "Proceeding with git status refresh despite lingering \
             .git/index.lock at {} — will retry once it clears",
            lock.display()
        );
    }
}

/// Poll `done` every 10ms until it returns `true`, or until `timeout` elapses.
/// Returns `true` if the condition was met, `false` on timeout.
fn poll_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while !done() {
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

/// Thread-safe shared handle to the [`FilePicker`] instance.
/// This accumulates only asynchronous non-blocking operations against the
/// file picker: creating, triggering various rescans and so on.
///
/// For blocking access use internal picker via `.read()` or `.write()`
///
/// ```ignore
/// let shared_picker = SharedFilePicker::default();
///
/// if let Some(picker) = shared_picker.read()?.as_ref() {
///     let files = picker.fuzzy_search(&query, options);
///     println!("Found {} files", files.len());
/// } else {
///     println!("Picker not initialized");
/// }
/// ```
#[derive(Clone, Default)]
pub struct SharedFilePicker(pub(crate) Arc<SharedPickerInner>);

pub struct SharedPickerInner {
    picker: parking_lot::RwLock<Option<FilePicker>>,
    /// Watch subscriptions live outside the picker lock so delivery and
    /// (un)subscribing never contend with searches.
    watchers: Arc<WatchRegistry>,
    /// Coverage, repository and ref listeners live outside the picker lock and survive
    /// picker replacement.
    coverage: Arc<CoverageListeners>,
    repository: Arc<WorkTreeListeners>,
    refs: Arc<RefListeners>,
    // Git dirs of repositories inside the index, with their work trees.
    git_dirs: parking_lot::Mutex<ahash::AHashMap<PathBuf, GitDir>>,
    rescans: RescanCounters,
    rescan_throttle: RescanThrottle,
    held_events: EventHold,
    git_status_paused: Arc<AtomicBool>,
}

impl Default for SharedPickerInner {
    fn default() -> Self {
        Self {
            picker: parking_lot::RwLock::new(None),
            watchers: Arc::new(WatchRegistry::default()),
            coverage: Arc::default(),
            repository: Arc::default(),
            refs: Arc::default(),
            git_dirs: Default::default(),
            rescans: Default::default(),
            rescan_throttle: RescanThrottle::default(),
            held_events: EventHold::default(),
            git_status_paused: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Outcome of a rescan request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RescanRequest {
    /// Started, or queued behind the active scan.
    Started,
    /// Throttled; runs once the cooldown ends unless another rescan starts first.
    Deferred,
    /// Throttled; incremental updates remain the freshest state.
    Dropped,
}

impl RescanRequest {
    pub(crate) fn started(self) -> bool {
        self == RescanRequest::Started
    }
}

/// Non-owning handle to a [`SharedPicker`].
#[derive(Clone)]
pub(crate) struct WeakFilePicker(Weak<SharedPickerInner>);

impl WeakFilePicker {
    /// Try to promote the weak handle back to a strong [`SharedPicker`].
    ///
    /// Returns `None` once every strong `SharedPicker` clone has been
    /// dropped. Callers should treat that as "the picker is being
    /// torn down" and exit their current iteration cleanly.
    pub(crate) fn upgrade(&self) -> Option<SharedFilePicker> {
        self.0.upgrade().map(SharedFilePicker)
    }
}

impl std::fmt::Debug for SharedFilePicker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SharedPicker").field(&"..").finish()
    }
}

impl SharedFilePicker {
    pub fn read(&self) -> Result<parking_lot::RwLockReadGuard<'_, Option<FilePicker>>, Error> {
        Ok(self.0.picker.read())
    }

    pub fn write(&self) -> Result<parking_lot::RwLockWriteGuard<'_, Option<FilePicker>>, Error> {
        Ok(self.0.picker.write())
    }

    /// Read access without waiting; `None` while a writer holds or waits for the lock.
    pub fn try_read(&self) -> Option<parking_lot::RwLockReadGuard<'_, Option<FilePicker>>> {
        self.0.picker.try_read()
    }

    /// Signal the background scan to cancel. Non-blocking: post-scan
    /// threads check this flag and bail out at their next cancellation point.
    pub fn cancel(&self) {
        if let Ok(guard) = self.read()
            && let Some(picker) = guard.as_ref()
        {
            picker.cancel();
        }
    }

    /// Applies changes to `paths` that a program reports, as the watcher applies its events
    /// for them: each existing path is added or updated unless the index's ignore rules
    /// exclude it, each missing one is removed with any indexed files below it, and the
    /// content index follows. An index that does not follow symlinks treats a reported
    /// symlink as removed, as its scan skips symlinks. Returns [`ReportedChanges::Held`] while a scan runs; the scan
    /// applies them once it commits, as it does with watcher events. Applying a change the
    /// watcher also delivers is harmless.
    pub fn apply_reported_changes(
        &self,
        shared_frecency: &SharedFrecency,
        paths: Vec<PathBuf>,
    ) -> ReportedChanges {
        if crate::watch::apply_reported_changes(self, shared_frecency, paths) {
            ReportedChanges::Applied
        } else {
            ReportedChanges::Held
        }
    }

    /// Watcher events held while a scan replaces the index they would update.
    pub(crate) fn event_hold(&self) -> &EventHold {
        &self.0.held_events
    }

    /// Produce a non-owning handle to the same inner picker.
    /// Use it if you don't need to block internal threads from dropping while owning this ref
    pub(crate) fn weaken(&self) -> WeakFilePicker {
        WeakFilePicker(Arc::downgrade(&self.0))
    }

    /// Return `true` if this is an instance of the picker that requires a complicated post-scan
    /// indexing/cache warmup job. The indexing is not crazy but it takes time.
    pub fn need_complex_rebuild(&self) -> bool {
        let guard = self.0.picker.read();
        guard
            .as_ref()
            .is_some_and(|p| p.has_mmap_cache() || p.has_content_indexing())
    }

    /// Block until the background filesystem scan finishes.
    /// Returns `true` if scan completed, `false` on timeout.
    pub fn wait_for_scan(&self, timeout: Duration) -> bool {
        let signal = {
            let guard = self.0.picker.read();
            match &*guard {
                Some(picker) => Arc::clone(&picker.signals.scanning),
                None => return true,
            }
        };

        poll_until(timeout, || {
            !signal.load(std::sync::atomic::Ordering::Acquire)
        })
    }

    /// Block until the background file watcher is ready.
    /// Returns `true` if watcher ready, `false` on timeout or when it failed or was stopped.
    pub fn wait_for_watcher(&self, timeout: Duration) -> bool {
        let status = {
            let guard = self.0.picker.read();
            match &*guard {
                Some(picker) => picker.background_status(),
                None => return true,
            }
        };

        poll_until(timeout, || {
            status.watcher() != crate::WatcherStatus::Starting
        });
        status.watcher() == crate::WatcherStatus::Ready
    }

    /// Blocks until both the filesystem walk and post-scan indexing are done.
    /// Returns true once scanning=false AND post_scan_indexing_active=false.
    pub fn wait_for_indexing_complete(&self, timeout: Duration) -> bool {
        let (scanning, post_scan_active) = {
            let guard = self.0.picker.read();
            match &*guard {
                Some(picker) => (
                    Arc::clone(&picker.signals.scanning),
                    Arc::clone(&picker.signals.post_scan_indexing_active),
                ),
                None => return true,
            }
        };

        poll_until(timeout, || {
            !scanning.load(std::sync::atomic::Ordering::Acquire)
                && !post_scan_active.load(std::sync::atomic::Ordering::Acquire)
        })
    }

    /// Trigger a full filesystem rescan without blocking the caller.
    /// Performs a safe async rescan. Guarantees only single active rescan per picker.
    /// If many rescans requested the last one guaranteed to be finished.
    pub fn trigger_full_rescan_async(&self, shared_frecency: &SharedFrecency) -> Result<(), Error> {
        self.trigger_full_rescan_with_reason(shared_frecency, RescanReason::Explicit)
            .map(|_| ())
    }

    /// Returns admitted and throttled rescan requests by reason.
    /// Counters start at picker creation or the last reset.
    pub fn rescan_stats(&self) -> RescanStats {
        self.0.rescans.snapshot()
    }

    pub fn reset_rescan_stats(&self) {
        self.0.rescans.reset();
    }

    /// Requests a rescan. Watcher requests inside the cooldown are throttled;
    /// see [`RescanRequest`] for what happens to them.
    pub(crate) fn trigger_full_rescan_with_reason(
        &self,
        shared_frecency: &SharedFrecency,
        reason: RescanReason,
    ) -> Result<RescanRequest, Error> {
        self.trigger_rescan(shared_frecency, reason, reason.defers_when_throttled())
    }

    /// Requests a rescan for changes the index is known to miss: inside the
    /// cooldown it stays deferred instead of being dropped.
    pub(crate) fn trigger_recovery_rescan(
        &self,
        shared_frecency: &SharedFrecency,
        reason: RescanReason,
    ) -> Result<RescanRequest, Error> {
        self.trigger_rescan(shared_frecency, reason, true)
    }

    fn trigger_rescan(
        &self,
        shared_frecency: &SharedFrecency,
        reason: RescanReason,
        defers: bool,
    ) -> Result<RescanRequest, Error> {
        // for giant folders we have no other choice other than throttling rescans
        // if user is running application in millions of files with a ton of rescan events
        // we drop / throttle some of requests to avoid constant burst of IO
        if reason == RescanReason::Explicit {
            self.0.rescan_throttle.note_explicit_scan();
        } else {
            match self.admit_rescan(reason, defers) {
                Admission::Admitted => {}
                Admission::Deferred(_) => return Ok(RescanRequest::Deferred),
                Admission::Dropped => return Ok(RescanRequest::Dropped),
            }
        }

        self.0.rescans.record(reason);

        ScanJob::request_rescan(self, shared_frecency)?;
        Ok(RescanRequest::Started)
    }

    fn admit_rescan(&self, reason: RescanReason, defers: bool) -> Admission {
        let Some(cooldown) = self
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().map(FilePicker::rescan_cooldown))
        else {
            // No index left to recover.
            self.0.rescan_throttle.cancel_deferred();
            return Admission::Dropped;
        };

        let admission = if defers {
            self.0.rescan_throttle.admit_deferring(reason, cooldown)
        } else {
            self.0.rescan_throttle.admit(reason, cooldown)
        };
        if admission != Admission::Admitted {
            self.0.rescans.record_throttled(reason);
            tracing::debug!(%reason, ?admission, "Rescan throttled");
        }
        admission
    }

    /// When the pending deferred rescan is due, if there is one.
    pub(crate) fn deferred_rescan_due(&self) -> Option<Instant> {
        self.0
            .rescan_throttle
            .deferred()
            .map(|deferred| deferred.due)
    }

    /// The reason of a deferred rescan whose cooldown has passed.
    pub(crate) fn due_deferred_rescan(&self) -> Option<RescanReason> {
        self.0
            .rescan_throttle
            .deferred()
            .filter(|deferred| deferred.due <= Instant::now())
            .map(|deferred| deferred.reason)
    }

    /// Test control: ends the rescan cooldown, so a deferred rescan runs now
    /// instead of when the cooldown would have ended. Debug builds only.
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn end_rescan_cooldown(&self) {
        self.0.rescan_throttle.end_cooldown();
        if let Ok(guard) = self.read()
            && let Some(watcher) = guard.as_ref().and_then(|p| p.background_watcher.as_ref())
        {
            watcher.wake_for_deferred_rescan();
        }
    }

    pub(crate) fn cancel_deferred_rescan(&self) {
        self.0.rescan_throttle.cancel_deferred();
    }

    /// Subscribe to filesystem changes matching `pattern`.
    ///
    /// Patterns may be base-relative globs (./ works), exact paths inside the indexed
    /// tree, or existing directories. An empty pattern watches the whole tree.
    ///
    /// Events are debounced over a 50-ms window and submitted in batches of at most 128 events.
    /// Gitignored and other ignored files are never triggering watcher.
    pub fn watch(
        &self,
        pattern: &str,
        options: WatchOptions,
        callback: impl Fn(WatchId, &[WatchEvent]) + Send + Sync + 'static,
    ) -> Result<WatchId, Error> {
        self.watch_in(None, pattern, options, callback)
    }

    /// Like [`Self::watch`] for the changes below `dir`, a directory inside the indexed
    /// base path: `pattern` and the ignores are relative to `dir`, as if it were the base
    /// path. Rescan events still name the base path.
    pub fn watch_within(
        &self,
        dir: &Path,
        pattern: &str,
        options: WatchOptions,
        callback: impl Fn(WatchId, &[WatchEvent]) + Send + Sync + 'static,
    ) -> Result<WatchId, Error> {
        self.watch_in(Some(dir), pattern, options, callback)
    }

    fn watch_in(
        &self,
        dir: Option<&Path>,
        pattern: &str,
        options: WatchOptions,
        callback: impl Fn(WatchId, &[WatchEvent]) + Send + Sync + 'static,
    ) -> Result<WatchId, Error> {
        let (base_path, has_watcher, watcher_ready) = {
            let guard = self.read()?;
            let picker = guard.as_ref().ok_or(Error::FilePickerMissing)?;

            (
                picker.base_path().to_path_buf(),
                picker.has_watcher(),
                picker.is_watcher_ready(),
            )
        };

        if !has_watcher {
            return Err(Error::WatcherDisabled);
        }
        if !watcher_ready {
            return Err(Error::WatcherNotReady);
        }

        let dir = match dir {
            Some(dir) => dir
                .strip_prefix(&base_path)
                .map_err(|_| Error::InvalidGlobPattern {
                    pattern: dir.display().to_string(),
                    reason: "the watched directory must be inside the indexed base path".into(),
                })?
                .to_path_buf(),
            None => PathBuf::new(),
        };
        self.0
            .watchers
            .subscribe_within(&base_path, &dir, pattern, options, Box::new(callback))
    }

    /// Remove a watch subscription. Returns `true` if the id was active.
    pub fn unwatch(&self, id: WatchId) -> bool {
        self.0.watchers.unsubscribe(id)
    }

    /// Return whether a watch subscription is active.
    pub fn is_watch_active(&self, id: WatchId) -> bool {
        self.0.watchers.contains(id)
    }

    /// Remove every subscription without waiting for an executing callback.
    pub fn shutdown_watches(&self) {
        self.0.watchers.shutdown();
    }

    /// Remove every subscription and wait for an executing callback.
    /// When called by that callback, it does not wait on itself.
    pub fn shutdown_watches_and_wait(&self) {
        self.0.watchers.shutdown_and_wait();
    }

    pub(crate) fn rebase_watches(&self, base_path: &Path) {
        self.0.watchers.rebase(base_path);
        self.0.git_dirs.lock().clear();
    }

    // Returns false when `git_dir` was known already as the directory it is now; true for a
    // new one and for one removed and created again since, also within one batch.
    pub(crate) fn register_git_dir(&self, git_dir: &Path, work_tree: &Path) -> bool {
        let identity = dir_identity(git_dir);
        let common_dir = crate::watch::common_dir_of(git_dir);
        let known = self.0.git_dirs.lock().insert(
            git_dir.to_path_buf(),
            GitDir {
                work_tree: work_tree.to_path_buf(),
                common_dir,
                identity,
            },
        );
        known.is_none_or(|known| known.identity != identity || identity.is_none())
    }

    // Forgets the git dirs of repositories at or below a removed `path`, so that one created
    // there again is recorded and watched anew: the old watches went with it. The removal
    // decides, also when the path is back by the time the batch is read, as a file system
    // may give the new directory the old one's inode.
    pub(crate) fn forget_git_dirs_under(&self, path: &Path) {
        self.0.git_dirs.lock().retain(|git_dir, known| {
            !git_dir.starts_with(path) && !known.work_tree.starts_with(path)
        });
    }

    // The registered git dir holding `path`, with its work tree.
    pub(crate) fn registered_git_dir(&self, path: &Path) -> Option<(PathBuf, PathBuf)> {
        let git_dirs = self.0.git_dirs.lock();
        path.ancestors().skip(1).find_map(|dir| {
            git_dirs
                .get(dir)
                .map(|known| (dir.to_path_buf(), known.work_tree.clone()))
        })
    }

    // Every registered git dir with its work tree and common dir.
    pub(crate) fn registered_git_dirs(&self) -> Vec<RegisteredGitDir> {
        (self.0.git_dirs.lock().iter())
            .map(|(git_dir, known)| RegisteredGitDir {
                git_dir: git_dir.clone(),
                work_tree: known.work_tree.clone(),
                common_dir: known.common_dir.clone(),
            })
            .collect()
    }

    // The work tree of the registered git dir `git_dir` itself.
    pub(crate) fn registered_work_tree(&self, git_dir: &Path) -> Option<PathBuf> {
        (self.0.git_dirs.lock().get(git_dir)).map(|known| known.work_tree.clone())
    }

    pub(crate) fn watch_registry(&self) -> &Arc<WatchRegistry> {
        &self.0.watchers
    }

    /// Calls `listener` whenever the set of live indexed directories changes: a scan
    /// result is installed, or the watcher adds, restores or removes directories.
    /// The listener may run while the picker lock is held: it must return promptly
    /// and must not access this picker. Dropping the subscription removes it.
    pub fn subscribe_coverage(
        &self,
        listener: impl Fn() + Send + Sync + 'static,
    ) -> ListenerSubscription {
        self.0.coverage.subscribe(move |_: &()| listener())
    }

    /// Calls `listener` with the root of a git work tree when the watcher sees a change of
    /// its git state that affects file status: the index, HEAD, its log or an operation in
    /// progress such as a merge. The work tree is the index's own repository's or that of a
    /// repository nested in the index, which is also announced once it is first watched.
    /// Files under `.git` are never watch events. The listener runs on the watcher's or a
    /// scan's thread: it must return promptly and must not access this picker. Dropping the
    /// subscription removes it.
    pub fn subscribe_repository_state(
        &self,
        listener: impl Fn(&Path) + Send + Sync + 'static,
    ) -> ListenerSubscription {
        self.0.repository.subscribe(listener)
    }

    // Git spells a work tree with a trailing separator; listeners hear one spelling.
    pub(crate) fn notify_repository_change(&self, work_tree: &Path) {
        let work_tree: PathBuf = work_tree.components().collect();
        self.0.repository.notify(&work_tree);
    }

    /// Calls `listener` with the root of a git work tree inside `scope` or containing it when
    /// the watcher sees one of its refs change: a branch, tag or remote-tracking ref created,
    /// moved or deleted, loose under `refs/heads`, `refs/tags` or `refs/remotes`, in
    /// `packed-refs` or in a reftable repository's tables, or its HEAD moved, as by a switch
    /// or a commit on a detached HEAD. A linked worktree's branches live in its main
    /// repository's common dir, so a change there names every work tree of the index sharing
    /// it; its HEAD names only itself. The work trees are those
    /// [`Self::subscribe_repository_state`] names. The watcher watches the refs of the
    /// repositories related to a listener's scope only while one is subscribed, from before
    /// this returns. This and dropping the subscription change the watcher's watches, so call
    /// them off async executors. The listener runs on the watcher's thread: it must return
    /// promptly and must not access this picker.
    pub fn subscribe_ref_changes(
        &self,
        scope: &Path,
        listener: impl Fn(&Path) + Send + Sync + 'static,
    ) -> ListenerSubscription {
        let scope: PathBuf = scope.components().collect();
        let subscription = self.0.refs.subscribe_with(scope, listener);
        self.sync_ref_watches();
        let picker = self.weaken();
        subscription.after_drop(move || {
            if let Some(picker) = picker.upgrade() {
                picker.sync_ref_watches();
            }
        })
    }

    // The scopes the ref listeners asked for.
    pub(crate) fn ref_scopes(&self) -> Vec<PathBuf> {
        self.0.refs.asked()
    }

    // Tells the ref listeners whose scope is related to `work_tree` that its refs changed.
    // Git spells a work tree with a trailing separator; listeners hear one spelling.
    pub(crate) fn notify_ref_change(&self, work_tree: &Path) {
        let work_tree: PathBuf = work_tree.components().collect();
        self.0
            .refs
            .notify_where(&work_tree, |scope| related(scope, &work_tree));
    }

    // Brings the watcher's ref watches in line with the ref listeners, outside the picker lock.
    fn sync_ref_watches(&self) {
        let watches = self.read().ok().and_then(|guard| {
            (guard.as_ref())
                .and_then(|picker| picker.background_watcher.as_ref())
                .map(crate::watch::BackgroundWatcher::watches)
        });
        if let Some(watches) = watches {
            watches.sync_refs(self);
        }
    }

    pub(crate) fn coverage_listeners(&self) -> &Arc<CoverageListeners> {
        &self.0.coverage
    }

    /// Stop background git status updates; the file index and watches keep updating.
    /// Manual [`Self::refresh_git_status`] still works. The pause survives re-indexing, and
    /// pausing before [`FilePicker::new_with_shared_state`] skips the initial status read.
    pub fn pause_git_status(&self) {
        self.0.git_status_paused.store(true, Ordering::Release);
        if let Some(worker) = self.git_status_worker() {
            worker.drop_pending();
        }
    }

    /// Resume background git status updates and queue one full refresh.
    /// Use [`Self::wait_for_git_status`] to know when it has been applied.
    pub fn resume_git_status(&self) {
        self.0.git_status_paused.store(false, Ordering::Release);
        if let Some(worker) = self.git_status_worker() {
            worker.request_full_rescan();
        }
    }

    pub fn is_git_status_paused(&self) -> bool {
        self.0.git_status_paused.load(Ordering::Acquire)
    }

    /// Block until queued background git status work is applied.
    /// Returns `false` on timeout. A zero timeout makes it a non-blocking check.
    pub fn wait_for_git_status(&self, timeout: Duration) -> bool {
        let Some(worker) = self.git_status_worker() else {
            return true;
        };

        poll_until(timeout, || worker.is_idle())
    }

    pub(crate) fn git_status_pause_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.0.git_status_paused)
    }

    fn git_status_worker(&self) -> Option<Arc<GitStatusWorker>> {
        let guard = self.0.picker.read();
        guard
            .as_ref()
            .map(|picker| Arc::clone(&picker.git_status_worker))
    }

    /// Refresh git statuses for all indexed files
    #[tracing::instrument(level = "info", skip_all)]
    pub fn refresh_git_status(&self, shared_frecency: &SharedFrecency) -> Result<usize, Error> {
        let (git_root, recency_config, base_path, picker_id) = {
            // we do the libgit2 off lock cause it might take quite some time on very large repos
            let guard = self.read()?;
            let Some(ref picker) = *guard else {
                return Err(Error::FilePickerMissing);
            };
            (
                picker.git_root().map(|p| p.to_path_buf()),
                picker.git_recency_config(),
                picker.base_path().to_path_buf(),
                picker.trace_id().to_owned(),
            )
        };

        let repo = git_root.as_deref().and_then(|root| {
            let repo = Repository::open(root)
                .inspect_err(|e| tracing::error!(?e, "Failed to open repo for git refresh"))
                .ok()?;
            wait_for_git_index_lock_release(repo.path());
            Some(repo)
        });

        let git_status = repo.as_ref().and_then(|repo| {
            GitStatusCache::read_full_status(repo)
                .inspect_err(|e| tracing::error!(?e, "Failed to read git status"))
                .ok()
        });

        let recency = repo
            .as_ref()
            .and_then(|repo| git_recency::compute_git_recency(repo, &recency_config, &base_path));

        let mut guard = self.write()?;
        let picker = guard.as_mut().ok_or(Error::FilePickerMissing)?;

        // ensure consistency
        if picker.trace_id() != picker_id {
            return Ok(0);
        }

        let statuses_count = if let Some(git_status) = git_status {
            let count = git_status.statuses_len();
            picker.update_git_statuses(git_status, shared_frecency)?;
            count
        } else {
            0
        };

        picker.apply_git_recency(recency.as_ref());

        Ok(statuses_count)
    }

    /// Recompute and apply git status for a specific set of paths.
    pub fn update_git_status_for_paths(
        &self,
        paths: &[PathBuf],
        shared_frecency: &SharedFrecency,
    ) -> Result<(), Error> {
        if paths.is_empty() {
            return Ok(());
        }

        let git_root = {
            let guard = self.read()?;
            let Some(ref picker) = *guard else {
                return Err(Error::FilePickerMissing);
            };
            picker.git_root().map(|p| p.to_path_buf())
        };
        let Some(git_root) = git_root else {
            return Ok(());
        };

        let repo = Repository::open(&git_root)?;
        wait_for_git_index_lock_release(repo.path());
        let status = GitStatusCache::git_status_for_paths(&repo, paths)?;

        let mut guard = self.write()?;
        let picker = guard.as_mut().ok_or(Error::FilePickerMissing)?;
        picker.update_git_statuses(status, shared_frecency)
    }
}

/// Thread-safe shared handle to an LMDB-backed store. A disabled (`noop`)
/// instance silently ignores writes. See the [`SharedFrecency`] and
/// [`SharedQueryTracker`] aliases.
///
/// `LmdbStore` is intentionally crate-private, so the store type is sealed:
/// only `FrecencyTracker` / `QueryTracker` can ever instantiate this.
#[allow(private_bounds)]
pub struct SharedDb<T: LmdbStore> {
    inner: Arc<RwLock<Option<T>>>,
    enabled: bool,
}

// Hand-written to avoid a spurious `T: Clone` bound — `Arc` is always `Clone`.
impl<T: LmdbStore> Clone for SharedDb<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            enabled: self.enabled,
        }
    }
}

impl<T: LmdbStore> Default for SharedDb<T> {
    fn default() -> Self {
        Self {
            inner: Arc::new(RwLock::new(None)),
            enabled: true,
        }
    }
}

impl<T: LmdbStore> std::fmt::Debug for SharedDb<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SharedDb").field(&T::LABEL).finish()
    }
}

#[allow(private_bounds)]
impl<T: LmdbStore> SharedDb<T> {
    /// Creates a disabled instance that silently ignores all writes.
    pub fn noop() -> Self {
        Self {
            inner: Arc::new(RwLock::new(None)),
            enabled: false,
        }
    }

    pub fn read(&self) -> Result<RwLockReadGuard<'_, Option<T>>, Error> {
        self.inner.read().map_err(|_| Error::AcquireFrecencyLock)
    }

    pub fn write(&self) -> Result<RwLockWriteGuard<'_, Option<T>>, Error> {
        self.inner.write().map_err(|_| Error::AcquireFrecencyLock)
    }

    /// Initialize the store + spawn GC in the background. No-op when disabled.
    pub fn init(&self, tracker: T) -> Result<(), Error> {
        if !self.enabled {
            return Ok(());
        }

        {
            let mut guard = self.write()?;
            *guard = Some(tracker);
        }

        // GC holds a read guard on this lock, so destroy / re-init wait won't race
        spawn_lmdb_gc(self.inner.clone());
        Ok(())
    }

    /// Drop the in-memory tracker and delete the on-disk database directory.
    ///
    /// Returns `Ok(Some(path))` with the deleted path, or `Ok(None)` if no tracker was initialized.
    pub fn destroy(&self) -> Result<Option<PathBuf>, Error> {
        let mut guard = self.write()?;
        let Some(tracker) = guard.take() else {
            return Ok(None);
        };

        let closing_event = match tracker.shared_env().destroy() {
            Ok(closing) => closing,
            Err(e) => {
                *guard = Some(tracker);
                return Err(e);
            }
        };

        let db_path = tracker.env().path().to_path_buf();
        // Drop closes the LMDB env and unmaps the files
        drop(tracker);
        drop(guard);

        // Deleting before mdb_env_close finishes would race the unmap.
        if let Some(event) = closing_event {
            event.wait_timeout(Duration::from_secs(5));
        }

        std::fs::remove_dir_all(&db_path).map_err(|source| Error::RemoveDbDir {
            path: db_path.clone(),
            source,
        })?;
        Ok(Some(db_path))
    }
}

/// Thread-safe shared handle to the [`FrecencyTracker`] instance.
pub type SharedFrecency = SharedDb<FrecencyTracker>;

/// Thread-safe shared handle to the [`QueryTracker`] instance.
pub type SharedQueryTracker = SharedDb<QueryTracker>;

// A git dir the index registered, with its work tree and the common dir holding its refs.
pub(crate) struct RegisteredGitDir {
    pub(crate) git_dir: PathBuf,
    pub(crate) work_tree: PathBuf,
    pub(crate) common_dir: PathBuf,
}

// Whether a ref listener of `scope` hears `work_tree`: one inside it or containing it.
pub(crate) fn related(scope: &Path, work_tree: &Path) -> bool {
    work_tree.starts_with(scope) || scope.starts_with(work_tree)
}

// A recorded git dir: its work tree, the common dir holding its refs, and which directory
// it was when recorded.
pub(crate) struct GitDir {
    work_tree: PathBuf,
    common_dir: PathBuf,
    identity: Option<DirIdentity>,
}

// What tells a directory from one created at the same path later: a file system may give
// the new one the inode the old one freed, but not its birth time, where it records one.
#[derive(Clone, Copy, PartialEq, Eq)]
struct DirIdentity {
    file: (u64, u64),
    born: Option<std::time::SystemTime>,
}

fn dir_identity(path: &Path) -> Option<DirIdentity> {
    let metadata = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::MetadataExt;
        (metadata.dev(), metadata.ino())
    };
    #[cfg(not(unix))]
    let file = (0, 0);
    Some(DirIdentity {
        file,
        born: metadata.created().ok(),
    })
}
