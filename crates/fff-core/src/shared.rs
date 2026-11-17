use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak};
use std::time::{Duration, Instant};

use crate::dbs::lmdb::{LmdbStore, spawn_lmdb_gc};
use crate::error::Error;
use crate::file_picker::FilePicker;
use crate::frecency::FrecencyTracker;
use crate::git_dirs::RegisteredGitDir;
use crate::listeners::{CoverageListeners, ListenerSubscription, RefListeners, WorkTreeListeners};
use crate::query_tracker::QueryTracker;
use crate::rescan_stats::{RescanCounters, RescanReason, RescanStats};
use crate::scm::ScmStatusWorker;
use crate::watch::{IndexWriter, Message, WatchEvent, WatchId, WatchOptions, WatchRegistry};

/// What became of changes a program reported with [`SharedFilePicker::apply_reported_changes`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportedChanges {
    /// The index holds them: they were applied before the call returned. New files past what
    /// the index takes in without a full rescan are the exception: they wait for that rescan.
    Applied,
    /// They are pending: reported while a scan runs, the scan applies them when it publishes
    /// its result, before [`crate::BackgroundStatus::is_scan_pending`] turns false.
    Held,
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
    rescans: RescanCounters,
    writer: IndexWriter,
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
            rescans: Default::default(),
            writer: IndexWriter::default(),
            git_status_paused: Arc::new(AtomicBool::new(false)),
        }
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
        let index = self.read().ok().and_then(|guard| {
            let picker = guard.as_ref()?;
            picker.cancel();
            Some(picker.signals.index_key())
        });
        if let Some(index) = index {
            self.send(Message::Cancel { index });
        }
    }

    /// Applies changes to `paths` that a program reports, as the watcher applies its events
    /// for them: each existing path is added or updated unless the index's ignore rules
    /// exclude it, each missing one is removed with any indexed files below it, and the
    /// content index follows. An index that does not follow symlinks treats a reported
    /// symlink as removed, as its scan skips symlinks. The changes are applied on the index's
    /// writer in order with the watcher's, and the call waits for that, also from a listener
    /// or a watch callback. While a scan runs, the writer keeps them for it and the call
    /// returns [`ReportedChanges::Held`]. New files past what the index takes in without a
    /// full rescan, reported one by one or below a reported directory, are not read: they wait
    /// for the full rescan that then follows. Applying a change the watcher also delivers is
    /// harmless.
    pub fn apply_reported_changes(
        &self,
        shared_frecency: &SharedFrecency,
        paths: Vec<PathBuf>,
    ) -> ReportedChanges {
        if crate::watch::apply_reported_changes(self, shared_frecency.clone(), paths) {
            ReportedChanges::Applied
        } else {
            ReportedChanges::Held
        }
    }

    /// Drops the index as setting `None` through [`Self::write`] does, but in order with what its
    /// loop applies, and waits; the handle holds no index until a new one is created on it.
    pub fn remove_index(&self) {
        // Cancelled first, so the loop's work for it stops and the removal never waits it out.
        let held = (self.read().ok())
            .is_some_and(|guard| guard.as_ref().inspect(|picker| picker.cancel()).is_some());
        if !held {
            return;
        }
        let (removed, done) = std::sync::mpsc::channel();
        if !self.send(Message::Remove { removed }) || done.recv().is_err() {
            let index = self.write().ok().and_then(|mut guard| guard.take());
            drop(index);
        }
    }

    // Queued on the index's loop, which records them for a running scan or applies them, and
    // answers whether it applied them. `None` without an index; no frecency uses the index's own.
    pub(crate) fn submit_events(
        &self,
        frecency: Option<SharedFrecency>,
        events: Vec<notify_debouncer_full::DebouncedEvent>,
    ) -> Option<std::sync::mpsc::Receiver<bool>> {
        let index = (self.read().ok())
            .and_then(|guard| guard.as_ref().map(|picker| picker.signals.index_key()))?;
        let (applied, done) = std::sync::mpsc::channel();
        self.send(Message::Events {
            index,
            events,
            frecency,
            applied: Some(applied),
        });
        Some(done)
    }

    // Has the index's loop take a batch, as from its watcher, and waits for it.
    #[cfg(test)]
    pub(crate) fn feed(
        &self,
        frecency: Option<SharedFrecency>,
        events: Vec<notify_debouncer_full::DebouncedEvent>,
    ) -> bool {
        (self.submit_events(frecency, events)).is_some_and(|done| done.recv().unwrap_or(false))
    }

    // Everything that changes the index reaches its loop through here.
    pub(crate) fn send(&self, message: Message) -> bool {
        self.0.writer.send(self, message)
    }

    #[cfg(test)]
    pub(crate) fn submit(&self, index: u64, work: crate::watch::Work) {
        self.send(Message::Work { index, work });
    }

    pub(crate) fn rescans(&self) -> &RescanCounters {
        &self.0.rescans
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
        let signals = {
            let guard = self.0.picker.read();
            match &*guard {
                Some(picker) => picker.scan_signals(),
                None => return true,
            }
        };

        // A rescan requested and not yet started counts as running.
        poll_until(timeout, || !signals.is_scanning())
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
        let signals = {
            let guard = self.0.picker.read();
            match &*guard {
                Some(picker) => picker.scan_signals(),
                None => return true,
            }
        };

        let done = || {
            !signals.is_scanning()
                && !(signals.post_scan_indexing_active).load(std::sync::atomic::Ordering::Acquire)
        };
        poll_until(timeout, done)
    }

    /// Trigger a full filesystem rescan without blocking the caller. It starts at once, or
    /// right after the running scan ends; requests made meanwhile join it.
    pub fn trigger_full_rescan_async(
        &self,
        _shared_frecency: &SharedFrecency,
    ) -> Result<(), Error> {
        self.request_full_rescan(RescanReason::Explicit, true)
    }

    /// Returns the full rescans that started, and the requests that joined a pending one, by
    /// reason. Counters start at picker creation or the last reset.
    pub fn rescan_stats(&self) -> RescanStats {
        self.0.rescans.snapshot()
    }

    pub fn reset_rescan_stats(&self) {
        self.0.rescans.reset();
    }

    /// Requests a watcher's full rescan, after the running scan and the interval; returns whether
    /// it started now. Waits for the index's loop to take it: never call it there.
    #[cfg(test)]
    pub(crate) fn trigger_full_rescan_with_reason(
        &self,
        _shared_frecency: &SharedFrecency,
        reason: RescanReason,
    ) -> Result<bool, Error> {
        let index = self.index_key()?;
        let (started, answer) = std::sync::mpsc::channel();
        self.send(Message::Rescan {
            index,
            reason,
            immediate: false,
            ticket: None,
            started: Some(started),
        });
        Ok(answer.recv().unwrap_or(false))
    }

    // Requests are never dropped: they join the one pending full rescan (Syncthing queues at
    // most one scan behind the running one). An immediate one counts as pending from now on.
    fn request_full_rescan(&self, reason: RescanReason, immediate: bool) -> Result<(), Error> {
        let (index, ticket) = {
            let guard = self.read()?;
            let picker = guard.as_ref().ok_or(Error::FilePickerMissing)?;
            // A cancelled index rescans no more.
            if picker.signals.cancelled.load(Ordering::Acquire) {
                return Ok(());
            }
            let in_flight = &picker.signals.requests_in_flight;
            let ticket = immediate.then(|| crate::scan::RequestTicket::new(in_flight));
            (picker.signals.index_key(), ticket)
        };
        // A message the loop cannot take is dropped with its ticket.
        self.send(Message::Rescan {
            index,
            reason,
            immediate,
            ticket,
            started: None,
        });
        Ok(())
    }

    fn index_key(&self) -> Result<u64, Error> {
        let guard = self.read()?;
        let picker = guard.as_ref().ok_or(Error::FilePickerMissing)?;
        Ok(picker.signals.index_key())
    }

    // When the loop starts the pending full rescan: `None` while a scan runs, whose end starts
    // it, while none is pending, or once the index is cancelled.
    #[cfg(test)]
    pub(crate) fn next_rescan_due(&self) -> Option<Instant> {
        let (due, answer) = std::sync::mpsc::channel();
        self.send(Message::Due(due));
        answer.recv().ok().flatten()
    }

    #[cfg(test)]
    pub(crate) fn wake_writer(&self) {
        self.send(Message::Wake);
    }

    /// Test control: ends the interval after the last full scan, so a pending rescan the
    /// watcher requested starts now. Debug builds only.
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn end_rescan_cooldown(&self) {
        let Ok(index) = self.index_key() else {
            return;
        };
        let (done, ended) = std::sync::mpsc::channel();
        self.send(Message::EndInterval { index, done });
        let _ = ended.recv();
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
            // While a lost watcher is replaced, the rebuild's rescan event covers the gap.
            let published = picker.signals.watcher();
            (
                picker.base_path().to_path_buf(),
                picker.has_watcher(),
                published.accepts_watches(),
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
    }

    // What the installed watcher watches, to use once the picker lock is released.
    pub(crate) fn installed_watches(&self) -> Option<Arc<crate::watch::Watches>> {
        let guard = self.read().ok()?;
        Some(guard.as_ref()?.background_watcher.as_ref()?.watches())
    }

    // The git dirs the installed watcher watches; none while no watcher is installed.
    fn git_dirs(&self) -> Option<Arc<crate::git_dirs::GitDirs>> {
        Some(Arc::clone(&self.installed_watches()?.git_dirs))
    }

    pub(crate) fn register_git_dir(&self, git_dir: &Path, work_tree: &Path) -> bool {
        self.git_dirs()
            .is_some_and(|git_dirs| git_dirs.register(git_dir, work_tree))
    }

    pub(crate) fn forget_git_dirs_under(&self, path: &Path) {
        if let Some(git_dirs) = self.git_dirs() {
            git_dirs.forget_under(path);
        }
    }

    pub(crate) fn registered_git_dir(&self, path: &Path) -> Option<(PathBuf, PathBuf)> {
        self.git_dirs()?.holding(path)
    }

    pub(crate) fn registered_git_dirs(&self) -> Vec<RegisteredGitDir> {
        self.git_dirs()
            .map(|git_dirs| git_dirs.all())
            .unwrap_or_default()
    }

    pub(crate) fn registered_work_tree(&self, git_dir: &Path) -> Option<PathBuf> {
        self.git_dirs()?.work_tree(git_dir)
    }

    pub(crate) fn watch_registry(&self) -> &Arc<WatchRegistry> {
        &self.0.watchers
    }

    /// Calls `listener` whenever the set of live indexed directories changes: a scan
    /// result is installed, the watcher adds, restores or removes directories, or the
    /// index reads ignore rules again after an ignore file changed.
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
    /// Files under `.git` are never watch events. Listeners run in order on the thread that
    /// calls the watch callbacks, never on one the index waits for: one may use this picker
    /// and report changes, while a slow one delays later notifications and watch events.
    /// Dropping the last handle of the picker, or replacing its index, waits for the
    /// notifications already queued and the one running, so do neither while holding a lock
    /// a listener takes. Dropping the subscription removes it.
    pub fn subscribe_repository_state(
        &self,
        listener: impl Fn(&Path) + Send + Sync + 'static,
    ) -> ListenerSubscription {
        self.0.repository.subscribe(listener)
    }

    // Git spells a work tree with a trailing separator; listeners hear one spelling.
    pub(crate) fn notify_repository_change(&self, work_tree: &Path) {
        let work_tree: PathBuf = work_tree.components().collect();
        #[cfg(test)]
        crate::watch::repository_told(&work_tree);
        let listeners = Arc::clone(&self.0.repository);
        (self.0.watchers).notify_listeners(move || listeners.notify(&work_tree));
    }

    /// Calls `listener` with the root of a git work tree inside `scope` or containing it when
    /// the watcher sees one of its refs change: a branch, tag or remote-tracking ref created,
    /// moved or deleted, loose under `refs/heads`, `refs/tags` or `refs/remotes`, in
    /// `packed-refs` or in a reftable repository's tables, or its HEAD moved, as by a switch
    /// or a commit on a detached HEAD. A linked worktree's branches live in its main
    /// repository's common dir, so a change there names every work tree of the index sharing
    /// it; its HEAD names itself to the listeners whose scope is related to it, and to any
    /// other listener a work tree sharing its common dir. The work trees are those
    /// [`Self::subscribe_repository_state`] names. The watcher watches the refs of the
    /// repositories related to a listener's scope only while one is subscribed, from before
    /// this returns. This and dropping the subscription change the watcher's watches, so call
    /// them off async executors. Listeners run as those of
    /// [`Self::subscribe_repository_state`] do. Builds without the `libgit2` feature watch no
    /// refs: listeners hear only the git state announced after a rebuild or a lost watcher.
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
        #[cfg(test)]
        crate::watch::ref_told(&work_tree);
        let listeners = Arc::clone(&self.0.refs);
        (self.0.watchers).notify_listeners(move || {
            listeners.notify_where(&work_tree, |scope| related(scope, &work_tree));
        });
    }

    // Tells the ref listeners of `work_tree` whose scope is not related to `moved` of its HEAD.
    pub(crate) fn notify_head_beside(&self, moved: &Path, work_tree: &Path) {
        let moved: PathBuf = moved.components().collect();
        let work_tree: PathBuf = work_tree.components().collect();
        let listeners = Arc::clone(&self.0.refs);
        (self.0.watchers).notify_listeners(move || {
            listeners.notify_where(&work_tree, |scope| {
                related(scope, &work_tree) && !related(scope, &moved)
            });
        });
    }

    // Has the watcher's owner thread bring its ref watches in line with the ref listeners, and
    // waits for it outside the picker lock.
    fn sync_ref_watches(&self) {
        let (done, followed) = std::sync::mpsc::channel();
        let sent = self.read().ok().is_some_and(|guard| {
            (guard.as_ref())
                .and_then(|picker| picker.background_watcher.as_ref())
                .is_some_and(|watcher| watcher.follow_refs(done))
        });
        if sent {
            let _ = followed.recv();
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
        if let Some(worker) = self.scm_status_worker() {
            worker.drop_pending();
        }
    }

    /// Resume background git status updates and queue one full refresh.
    /// Use [`Self::wait_for_git_status`] to know when it has been applied.
    pub fn resume_git_status(&self) {
        self.0.git_status_paused.store(false, Ordering::Release);
        if let Some(worker) = self.scm_status_worker() {
            worker.request_full_rescan();
        }
    }

    pub fn is_git_status_paused(&self) -> bool {
        self.0.git_status_paused.load(Ordering::Acquire)
    }

    /// Block until queued background git status work is applied.
    /// Returns `false` on timeout. A zero timeout makes it a non-blocking check.
    pub fn wait_for_git_status(&self, timeout: Duration) -> bool {
        let Some(worker) = self.scm_status_worker() else {
            return true;
        };

        poll_until(timeout, || worker.is_idle())
    }

    pub(crate) fn git_status_pause_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.0.git_status_paused)
    }

    fn scm_status_worker(&self) -> Option<Arc<dyn ScmStatusWorker>> {
        let guard = self.0.picker.read();
        guard
            .as_ref()
            .map(|picker| Arc::clone(&picker.scm_status_worker))
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
                picker.scm_workspace().cloned(),
                picker.git_recency_config(),
                picker.base_path().to_path_buf(),
                picker.trace_id().to_owned(),
            )
        };

        let repo = git_root.as_ref().and_then(|workspace| {
            workspace
                .open()
                .inspect_err(|e| tracing::error!(?e, "Failed to open SCM workspace"))
                .ok()
        });
        let git_status = repo.as_ref().and_then(|repo| {
            repo.wait_for_write();
            repo.status(true)
                .inspect_err(|e| tracing::error!(?e, "Failed to read SCM status"))
                .ok()
        });
        let recency = repo
            .as_ref()
            .zip(git_root.as_ref())
            .and_then(|(repo, workspace)| {
                repo.recency(&recency_config, &workspace.git_path(&base_path))
            });

        let mut guard = self.write()?;
        let picker = guard.as_mut().ok_or(Error::FilePickerMissing)?;

        // ensure consistency
        if picker.trace_id() != picker_id {
            return Ok(0);
        }

        let statuses_count = if let Some(git_status) = git_status {
            picker.update_git_statuses(git_status, shared_frecency)?
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

        let (git_root, worker) = {
            let guard = self.read()?;
            let Some(ref picker) = *guard else {
                return Err(Error::FilePickerMissing);
            };
            (
                picker.scm_workspace().cloned(),
                Arc::clone(&picker.scm_status_worker),
            )
        };
        let Some(git_root) = git_root else {
            return Ok(());
        };

        let status = git_root.status_for_paths(paths)?;

        let mut guard = self.write()?;
        let picker = guard.as_mut().ok_or(Error::FilePickerMissing)?;
        if Arc::ptr_eq(&worker, &picker.scm_status_worker) {
            picker.update_git_statuses(status, shared_frecency)?;
        }
        Ok(())
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

// Whether a ref listener of `scope` hears `work_tree`: one inside it or containing it.
pub(crate) fn related(scope: &Path, work_tree: &Path) -> bool {
    work_tree.starts_with(scope) || scope.starts_with(work_tree)
}
