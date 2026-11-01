use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tracing::{error, info, warn};

use crate::FileSync;
use crate::error::Error;
use crate::file_picker::FFFMode;
use crate::git::GitDiscovery;
use crate::index::{build_bigram_index, sniff_binary_for_non_indexable};
use crate::parallelism::BACKGROUND_THREAD_POOL;
use crate::rescan_stats::RescanReason;
use crate::shared::{SharedFilePicker, SharedFrecency};
use crate::types::ContentCacheBudget;
use crate::watch::{BackgroundWatcher, Released, handle_debounced_events, request_recovery_rescan};

#[derive(Clone, Default)]
pub(crate) struct ScanSignals {
    /// Set to `true` while any scan phase is running
    pub(crate) scanning: Arc<AtomicBool>,
    /// Set once the watcher is subscribed and every change since the initial walk
    /// began is indexed or queued behind it
    pub(crate) watcher_ready: Arc<AtomicBool>,
    /// Set when the initial scan ends without installing the requested watcher
    pub(crate) watcher_failed: Arc<AtomicBool>,
    /// Indicates that that owning picker was requested to shut down
    pub(crate) cancelled: Arc<AtomicBool>,
    /// Claimed by the one scan job allowed to run for this picker, from its start
    /// until it has handed a pending request to a follow-up scan
    pub(crate) scan_job_running: Arc<AtomicBool>,
    /// A rescan requested while a job ran; the job starts it when it ends
    pub(crate) rescan_pending: Arc<AtomicBool>,
    /// Set by `post_scan_snapshot`, cleared by `PostScanSnapshot::drop`.
    /// DO NOT set or clear this manually — it is managed exclusively by the
    /// PostScanSnapshot lifecycle.
    pub(crate) post_scan_indexing_active: Arc<AtomicBool>,
}

/// Lifecycle of a picker's filesystem watcher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatcherStatus {
    /// The picker was created without a watcher.
    Disabled,
    /// The initial scan has not finished setting up the watcher.
    Starting,
    Ready,
    /// The watcher could not be installed; the index is not kept up to date.
    Failed,
    /// The picker was cancelled or its watcher was stopped.
    Stopped,
}

/// Lock-free view of a picker's background scan and watcher progress.
#[derive(Clone)]
pub struct BackgroundStatus {
    signals: ScanSignals,
    watch: bool,
    scanned_files: Arc<AtomicUsize>,
}

impl BackgroundStatus {
    pub(crate) fn new(signals: ScanSignals, watch: bool, scanned_files: Arc<AtomicUsize>) -> Self {
        Self {
            signals,
            watch,
            scanned_files,
        }
    }

    /// Files the current or last scan has walked so far.
    pub fn scanned_files(&self) -> usize {
        self.scanned_files.load(Ordering::Relaxed)
    }

    pub fn is_scanning(&self) -> bool {
        self.signals.scanning.load(Ordering::Acquire)
    }

    /// Whether a scan job runs, post-scan phase included, or a requested rescan waits for one.
    pub fn is_scan_pending(&self) -> bool {
        self.signals.scan_job_running.load(Ordering::SeqCst)
            || self.signals.rescan_pending.load(Ordering::SeqCst)
    }

    pub fn watcher(&self) -> WatcherStatus {
        if !self.watch {
            WatcherStatus::Disabled
        } else if self.signals.watcher_ready.load(Ordering::Acquire) {
            WatcherStatus::Ready
        } else if self.signals.watcher_failed.load(Ordering::Acquire) {
            WatcherStatus::Failed
        } else if self.signals.cancelled.load(Ordering::Acquire) {
            WatcherStatus::Stopped
        } else {
            WatcherStatus::Starting
        }
    }
}

/// Which optional phases a scan should run.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct ScanConfig {
    pub(crate) warmup: bool,
    pub(crate) content_indexing: bool,
    pub(crate) watch: bool,
    pub(crate) auto_cache_budget: bool,
    pub(crate) install_watcher: bool,
    pub(crate) follow_symlinks: bool,
    pub(crate) enable_fs_root_scanning: bool,
    pub(crate) enable_home_dir_scanning: bool,
}

/// A fully-configured scan job ready to run on a background thread.
///
/// Build with [`ScanJob::from_picker`] (reads all state from the
/// current `FilePicker`) or [`ScanJob::initial`] (for the bootstrap
/// scan, before the picker is published to `SharedPicker`).
pub(crate) struct ScanJob {
    shared_picker: SharedFilePicker,
    shared_frecency: SharedFrecency,
    base_path: PathBuf,
    mode: FFFMode,
    signals: ScanSignals,
    config: ScanConfig,
    /// Walker-maintained counter backing `get_scan_progress` on the UI
    /// side. Reset to 0 at scan start, incremented per-file by the
    /// walker. Shared `Arc` so the UI polls the same atomic.
    scanned_files_counter: Arc<AtomicUsize>,
    trace_span: tracing::Span,
}

impl ScanSignals {
    fn claim_scan_job(&self) -> bool {
        self.scan_job_running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }
}

impl ScanJob {
    /// Starts a rescan, or leaves it to the running scan job, which starts it when it ends.
    pub(crate) fn request_rescan(
        shared_picker: &SharedFilePicker,
        shared_frecency: &SharedFrecency,
    ) -> Result<(), Error> {
        if let Some(job) = Self::new_rescan(shared_picker, shared_frecency, None)? {
            #[cfg(test)]
            tests::reached(tests::Stage::Claimed, &job.base_path);
            job.spawn();
            return Ok(());
        }
        let signals = {
            let guard = shared_picker.read()?;
            guard
                .as_ref()
                .ok_or(Error::FilePickerMissing)?
                .scan_signals()
        };
        signals.rescan_pending.store(true, Ordering::SeqCst);
        info!("Full rescan requested while another scan is active; it runs next");
        // The running job may have ended between the claim and the flag.
        if let Some(job) = Self::new_rescan(shared_picker, shared_frecency, Some(&signals))? {
            job.spawn();
        }
        Ok(())
    }

    // Claims the picker's single scan job; `None` while another job holds it or
    // when the published picker is no longer the one owning `expected`.
    fn new_rescan(
        shared_picker: &SharedFilePicker,
        shared_frecency: &SharedFrecency,
        expected: Option<&ScanSignals>,
    ) -> Result<Option<Self>, Error> {
        let guard = shared_picker.read()?;
        let picker = guard.as_ref().ok_or(Error::FilePickerMissing)?;

        if expected.is_some_and(|expected| {
            !Arc::ptr_eq(&expected.scan_job_running, &picker.signals.scan_job_running)
        }) || !picker.signals.claim_scan_job()
        {
            return Ok(None);
        }
        // The walk has not started yet, so it covers every request made until now.
        picker.signals.rescan_pending.store(false, Ordering::SeqCst);

        let mode = picker.mode();
        let signals = picker.scan_signals();
        let scanned_files_counter = picker.scanned_files_counter();
        let base_path = picker.base_path().to_path_buf();
        let trace_span = picker.trace_span();

        let new_scan_config = ScanConfig {
            warmup: picker.has_mmap_cache(),
            content_indexing: picker.has_content_indexing(),
            watch: picker.has_watcher(),
            auto_cache_budget: !picker.has_explicit_cache_budget(),
            install_watcher: false, // the watcher is independent of rescan, it is not restarting EVER
            follow_symlinks: picker.follows_symlinks(),
            enable_fs_root_scanning: picker.fs_root_scanning_enabled(),
            enable_home_dir_scanning: picker.home_dir_scanning_enabled(),
        };

        drop(guard); // just a sanity check

        Ok(Some(Self {
            mode,
            signals,
            base_path,
            scanned_files_counter,
            config: new_scan_config,
            shared_picker: shared_picker.clone(),
            shared_frecency: shared_frecency.clone(),
            trace_span,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_initial(
        shared_picker: SharedFilePicker,
        shared_frecency: SharedFrecency,
        base_path: PathBuf,
        mode: FFFMode,
        signals: ScanSignals,
        scanned_files_counter: Arc<AtomicUsize>,
        trace_span: tracing::Span,
        config: ScanConfig,
    ) -> Self {
        Self {
            shared_picker,
            shared_frecency,
            base_path,
            mode,
            signals,
            scanned_files_counter,
            config,
            trace_span,
        }
    }

    /// Run the job on `BACKGROUND_THREAD_POOL`. Returns immediately.
    ///
    /// Routed through the pool — and not a fresh `std::thread::spawn` — so the
    /// orchestrator inherits rayon's QoS pin (USER_INITIATED). Without that
    /// pin, an interactive nvim's USER_INTERACTIVE main thread spawns a child
    /// at lower QoS, the walker's Zig worker pool inherits the demotion, and
    /// the kernel drifts those workers onto E-cores. On chromium that turns a
    /// ~800 ms walk into ~3 s.
    pub fn spawn(self) {
        self.signals.scanning.store(true, Ordering::Release);
        let span = self.trace_span.clone();
        BACKGROUND_THREAD_POOL.spawn(move || {
            let _g = span.enter();
            self.run();
        });
    }

    fn run(self) {
        let Self {
            shared_picker,
            shared_frecency,
            base_path,
            mode,
            signals,
            scanned_files_counter,
            config,
            trace_span: _,
        } = self;

        // Declared first so it ends last, after the scanning signal is cleared.
        let _job = ScanJobClaim {
            shared_picker: &shared_picker,
            shared_frecency: &shared_frecency,
            signals: &signals,
            base_path: &base_path,
        };
        let scanning = ScanningGuard::new(&signals);
        let mut watcher_install =
            WatcherInstallGuard::new(&signals, config.install_watcher && config.watch);
        scanned_files_counter.store(0, Ordering::Relaxed);
        let git = FileSync::discover_git(&base_path);
        let git_workdir = git.as_ref().map(|git| git.workdir.clone());

        // 1. Subscribe before walking. Events arriving from now on are held until
        // the walk's snapshot replaces the index, then replayed in order.
        let held_events = config
            .watch
            .then(|| HeldEvents::new(&shared_picker, &signals))
            .flatten();
        if config.install_watcher && config.watch && !signals.cancelled.load(Ordering::Acquire) {
            Self::install_watcher(
                &shared_picker,
                &shared_frecency,
                &base_path,
                &git,
                mode,
                &config,
                &signals,
            );
        }
        // Per-directory backends subscribe each directory before the walker reads it.
        let dir_hook = shared_picker.read().ok().and_then(|guard| {
            guard
                .as_ref()
                .filter(|picker| {
                    picker.base_path() == base_path && !signals.cancelled.load(Ordering::Acquire)
                })
                .and_then(|picker| picker.background_watcher.as_ref())
                .and_then(BackgroundWatcher::dir_hook)
        });

        // 2. Walk the file system and collect the list of files
        #[cfg(test)]
        tests::reached(tests::Stage::Walking, &base_path);
        let sync = match FileSync::walk_filesystem(
            &base_path,
            git.clone(),
            &scanned_files_counter,
            &shared_frecency,
            mode,
            config.follow_symlinks,
            dir_hook.as_ref(),
        ) {
            Ok(sync) => sync,
            Err(e) => {
                error!(?e, "scan walk failed");
                if config.install_watcher {
                    Self::remove_watcher(&shared_picker, &base_path, &signals);
                } else if let Some(held_events) = held_events {
                    held_events.replay(&shared_frecency, &base_path, &git_workdir, mode);
                }
                return;
            }
        };
        drop(dir_hook);
        #[cfg(test)]
        tests::reached(tests::Stage::Walked, &base_path);

        // 3. Populate the file list
        let git_status_worker;
        if let Ok(mut guard) = shared_picker.write()
            && let Some(picker) = guard.as_mut()
        {
            if signals.cancelled.load(Ordering::Acquire) {
                info!("scan cancelled between walk and commit, discarding");
                return;
            }

            let live_count = sync.live_count;
            picker.commit_new_sync(sync);
            git_status_worker = Arc::clone(&picker.git_status_worker);

            if config.auto_cache_budget && !picker.has_explicit_cache_budget() {
                picker.set_cache_budget(ContentCacheBudget::new_for_repo(live_count));
            }
        } else {
            error!("failed to install scan results into picker");
            return;
        }

        // Spawn the git status worker once. BUG PINNNING. If the user initiated git in the folder
        // which is a real use case we need to have a way to start the git worker background thread dynamically
        if git.is_some() && !signals.cancelled.load(Ordering::Acquire) {
            git_status_worker.spawn_once(shared_picker.weaken(), shared_frecency.clone());
            git_status_worker.request_full_rescan(); // this runs anyway
        }

        if let Some(held_events) = held_events {
            held_events.replay(&shared_frecency, &base_path, &git_workdir, mode);
        }

        // 4. Every change since the walk began is now indexed or queued behind the
        // live watcher. The lock orders this with a concurrent stop.
        if config.install_watcher
            && let Ok(guard) = shared_picker.read()
            && let Some(picker) = guard.as_ref()
            && picker.background_watcher.is_some()
            && !signals.cancelled.load(Ordering::Acquire)
        {
            signals.watcher_ready.store(true, Ordering::Release);
            watcher_install.installed();
        }
        drop(watcher_install);

        // BUG pinning: take the snapshot *before* the storing the scan=true, otherwise there is a tiny
        // race window when there scanned is set to true, but `post_scan_indexing_active` flag is `false`
        let snapshot = if !signals.cancelled.load(Ordering::Acquire) {
            shared_picker.read().ok().and_then(|guard| {
                guard
                    .as_ref()
                    .and_then(|picker| unsafe { picker.post_scan_snapshot() })
            })
        } else {
            None
        };

        signals.scanning.store(false, Ordering::Relaxed); // file are searchable

        // 5. Runs post scna in parallel with git status collection
        if !signals.cancelled.load(Ordering::Acquire)
            && let Some(snap) = snapshot.as_ref()
        {
            Self::run_post_scan(&shared_picker, &signals, &config, snap);
        }

        drop(snapshot); // SNAPSHOT SHOULD NOT BE USED AFTER THIS POINT

        drop(scanning);
    }

    // Installs the initial scan's watcher; its events stay held until the replay.
    fn install_watcher(
        shared_picker: &SharedFilePicker,
        shared_frecency: &SharedFrecency,
        base_path: &Path,
        git: &Option<GitDiscovery>,
        mode: FFFMode,
        config: &ScanConfig,
        signals: &ScanSignals,
    ) {
        let Some(git_status_worker) = shared_picker.read().ok().and_then(|guard| {
            guard
                .as_ref()
                .map(|picker| Arc::clone(&picker.git_status_worker))
        }) else {
            return;
        };
        match BackgroundWatcher::new(
            base_path.to_path_buf(),
            git.clone(),
            shared_picker.clone(),
            shared_frecency.clone(),
            mode,
            config.enable_fs_root_scanning,
            config.enable_home_dir_scanning,
            git_status_worker,
            tracing::Span::current(),
        ) {
            Ok(watcher) => {
                if let Ok(mut guard) = shared_picker.write()
                    && let Some(picker) = guard.as_mut()
                    && picker.base_path() == base_path
                    && !signals.cancelled.load(Ordering::Acquire)
                {
                    picker.background_watcher = Some(watcher);
                }
            }
            Err(e) => error!(?e, "failed to initialize background watcher"),
        }
    }

    // An index whose initial walk failed cannot follow the tree.
    fn remove_watcher(shared_picker: &SharedFilePicker, base_path: &Path, signals: &ScanSignals) {
        if let Ok(mut guard) = shared_picker.write()
            && let Some(picker) = guard.as_mut()
            && picker.base_path() == base_path
            && !signals.cancelled.load(Ordering::Acquire)
        {
            picker.stop_background_monitor();
        }
    }

    /// THIS IS VERY VERY IMPORTANT THAT ANYTHING INSIDE THIS FUNCTION TO NOT READ ANYTHING CLEARABLE OUTSIDE
    /// this is a very silly off lock implementation that actually matters, and that's why it is crafted
    /// to never read anything from the picker, it can only WRITE information using single instructions
    ///
    /// Things that are safe and immutable - file list, indexes of files, paths, and signals.
    #[tracing::instrument(skip_all, fields(warmup = ?config.warmup, indexing = ?config.content_indexing))]
    fn run_post_scan(
        shared_picker: &SharedFilePicker,
        signals: &ScanSignals,
        config: &ScanConfig,
        unsafe_snapshot: &crate::file_picker::PostScanUnsafeSnapshot,
    ) {
        let Some(arena) = unsafe_snapshot
            .arena // we are never touching overlays so this arena is always correct
            .as_ref()
            .map(|s| s.as_arena_ptr())
        else {
            tracing::error!("Failed to run post scan: arena is invalid");
            return;
        };

        let files: &[crate::types::FileItem] = &unsafe_snapshot.files[..unsafe_snapshot.base_count];
        if signals.cancelled.load(Ordering::Acquire) {
            return;
        }

        if config.content_indexing {
            let indexable_count = unsafe_snapshot.indexable_count.min(files.len());
            let (indexable_files, non_indexable_files) = files.split_at(indexable_count);
            let index = build_bigram_index(indexable_files, &unsafe_snapshot.base_path, arena);
            #[cfg(test)]
            tests::reached(tests::Stage::ContentIndexed, &unsafe_snapshot.base_path);

            // A root change may have replaced the picker while this index was building.
            if let Ok(mut guard) = shared_picker.write()
                && let Some(picker) = guard.as_mut()
                && !signals.cancelled.load(Ordering::Acquire)
                && picker.base_path() == unsafe_snapshot.base_path
            {
                picker.set_bigram_index(index);
            }
            crate::index::release_thread_buffers();

            // Bigram only sniffs files <= MAX_INDEXABLE_FILE_SIZE; large
            // unknown-extension binaries slip past it and would otherwise be
            // grep-able as text. Cheap header sniff catches those.
            if !signals.cancelled.load(Ordering::Acquire) {
                sniff_binary_for_non_indexable(
                    non_indexable_files,
                    &unsafe_snapshot.base_path,
                    arena,
                    &signals.cancelled,
                );
            }
        } else {
            // this potentially a long running as we are not parallelizing it but it's okay
            sniff_binary_for_non_indexable(
                files,
                &unsafe_snapshot.base_path,
                arena,
                &signals.cancelled,
            );
        }

        // TODO Skipped as potentially unsafe - figure this out later
        // if config.warmup && !signals.cancelled.load(Ordering::Acquire) {
        //     warmup_mmaps(files, budget, &unsafe_snapshot.base_path, arena);
        // }
    }
}

// Holds watcher events from before a scan's walk until its snapshot is committed,
// which would otherwise overwrite changes applied while the walk ran.
struct HeldEvents<'a> {
    shared_picker: &'a SharedFilePicker,
    owner: u64,
}

impl<'a> HeldEvents<'a> {
    fn new(shared_picker: &'a SharedFilePicker, signals: &ScanSignals) -> Option<Self> {
        let owner = shared_picker.event_hold().hold(&signals.cancelled)?;
        Some(Self {
            shared_picker,
            owner,
        })
    }

    // Applies the held events in arrival order, including those arriving meanwhile,
    // then lets later events through. Dropped events are recovered by a rescan.
    fn replay(
        self,
        shared_frecency: &SharedFrecency,
        base_path: &Path,
        git_workdir: &Option<PathBuf>,
        mode: FFFMode,
    ) {
        let shared_picker = self.shared_picker;
        let Some(git_status_worker) = shared_picker.read().ok().and_then(|guard| {
            guard
                .as_ref()
                .map(|picker| Arc::clone(&picker.git_status_worker))
        }) else {
            return;
        };
        loop {
            let events = match shared_picker.event_hold().release(self.owner) {
                Released::Events(events) => events,
                Released::Done => break,
                Released::Overflowed => {
                    warn!("Too many changes arrived while scanning; rescanning");
                    request_recovery_rescan(
                        shared_picker,
                        shared_frecency,
                        base_path,
                        RescanReason::EventBatchOverflow,
                    );
                    break;
                }
            };
            let new_dirs = handle_debounced_events(
                mode,
                events,
                base_path,
                git_workdir,
                shared_picker,
                shared_frecency,
                &git_status_worker,
            );
            if let Ok(guard) = shared_picker.read()
                && let Some(watcher) = guard
                    .as_ref()
                    .and_then(|picker| picker.background_watcher.as_ref())
            {
                for dir in new_dirs {
                    watcher.request_index_dir(dir);
                }
            }
        }
    }
}

impl Drop for HeldEvents<'_> {
    // A scan that ends without replaying (cancelled or unpublished) drops what it held.
    fn drop(&mut self) {
        self.shared_picker.event_hold().discard(self.owner);
    }
}

// Releases the scan job claim on every exit, then starts a rescan requested meanwhile.
struct ScanJobClaim<'a> {
    shared_picker: &'a SharedFilePicker,
    shared_frecency: &'a SharedFrecency,
    signals: &'a ScanSignals,
    base_path: &'a Path,
}

impl Drop for ScanJobClaim<'_> {
    fn drop(&mut self) {
        self.signals.scan_job_running.store(false, Ordering::SeqCst);
        // A requester sets the flag before retrying its claim, so one of us starts it.
        if !self.signals.cancelled.load(Ordering::Acquire)
            && self.signals.rescan_pending.load(Ordering::SeqCst)
        {
            // A replaced index is not claimed: its successor's own scan covers the request.
            match ScanJob::new_rescan(self.shared_picker, self.shared_frecency, Some(self.signals))
            {
                Ok(Some(follow_up)) => {
                    info!(base_path = %self.base_path.display(), "Starting the rescan requested while the scan ran");
                    follow_up.spawn();
                }
                Ok(None) => {}
                Err(e) => error!(?e, "Failed to start the requested rescan"),
            }
        }
        #[cfg(test)]
        tests::reached(tests::Stage::Finished, self.base_path);
    }
}

// Ensures early returns clear the scanning signal.
struct ScanningGuard<'a> {
    signals: &'a ScanSignals,
}

impl<'a> ScanningGuard<'a> {
    fn new(signals: &'a ScanSignals) -> Self {
        signals.scanning.store(true, Ordering::Relaxed);
        Self { signals }
    }
}

impl Drop for ScanningGuard<'_> {
    fn drop(&mut self) {
        self.signals.scanning.store(false, Ordering::Relaxed);
    }
}

// Reports a requested watcher that the initial scan did not install, unless cancelled.
struct WatcherInstallGuard<'a> {
    signals: &'a ScanSignals,
    pending: bool,
}

impl<'a> WatcherInstallGuard<'a> {
    fn new(signals: &'a ScanSignals, requested: bool) -> Self {
        Self {
            signals,
            pending: requested,
        }
    }

    fn installed(&mut self) {
        self.pending = false;
    }
}

impl Drop for WatcherInstallGuard<'_> {
    fn drop(&mut self) {
        if self.pending && !self.signals.cancelled.load(Ordering::Acquire) {
            self.signals.watcher_failed.store(true, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_picker::{FilePicker, FilePickerOptions};
    use crate::grep::{GrepSearchOptions, parse_grep_query};
    use crate::scope::SearchScope;
    use crate::watch::MAX_BATCH_PATHS;
    use notify::event::{CreateKind, EventKind};
    use notify_debouncer_full::DebouncedEvent;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum Stage {
        Claimed,
        Walking,
        Walked,
        ContentIndexed,
        Finished,
    }

    type Hook = Box<dyn FnOnce() + Send>;

    // Each hook runs once, when the next scan of its base path reaches its stage.
    static HOOKS: Mutex<Vec<(PathBuf, Stage, Hook)>> = Mutex::new(Vec::new());
    static REACHED: Mutex<Vec<(PathBuf, Stage)>> = Mutex::new(Vec::new());

    // The hook runs before the stage is recorded.
    pub(super) fn reached(stage: Stage, base_path: &Path) {
        let hook = {
            let mut hooks = HOOKS.lock().unwrap();
            let position = hooks
                .iter()
                .position(|(path, at, _)| path == base_path && *at == stage);
            position.map(|position| hooks.remove(position).2)
        };
        if let Some(hook) = hook {
            hook();
        }
        REACHED
            .lock()
            .unwrap()
            .push((base_path.to_path_buf(), stage));
    }

    fn most_concurrent_walks(root: &Path) -> usize {
        let mut walking = 0usize;
        let mut most = 0;
        for (path, stage) in REACHED.lock().unwrap().iter() {
            match stage {
                _ if path != root => {}
                Stage::Walking => {
                    walking += 1;
                    most = most.max(walking);
                }
                Stage::Walked => walking = walking.saturating_sub(1),
                _ => {}
            }
        }
        most
    }

    fn on(root: &Path, stage: Stage, hook: impl FnOnce() + Send + 'static) {
        HOOKS
            .lock()
            .unwrap()
            .push((root.to_path_buf(), stage, Box::new(hook)));
    }

    fn times_reached(root: &Path, stage: Stage) -> usize {
        REACHED
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, at)| path == root && *at == stage)
            .count()
    }

    fn poll(done: impl FnMut() -> bool) -> bool {
        poll_for(Duration::from_secs(20), done)
    }

    fn poll_for(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + limit;
        while !done() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }

    fn eventually(what: &str, done: impl FnMut() -> bool) {
        assert!(poll(done), "timed out waiting until {what}");
    }

    // Waits until every scan that walked `root` has finished and none is starting.
    fn settle(shared: &SharedFilePicker, root: &Path) {
        eventually("the scans settle", || {
            let walks = times_reached(root, Stage::Walked);
            walks > 0
                && times_reached(root, Stage::Finished) == walks
                && shared.wait_for_indexing_complete(Duration::ZERO)
        });
    }

    fn start(shared: &SharedFilePicker, root: &Path, content_indexing: bool) -> SharedFrecency {
        // These tests check how scans and watcher events interleave, not OS event delivery.
        crate::watch::poll_watcher_for(root);
        let frecency = SharedFrecency::default();
        FilePicker::new_with_shared_state(
            shared.clone(),
            frecency.clone(),
            FilePickerOptions {
                base_path: root.to_string_lossy().into_owned(),
                enable_content_indexing: content_indexing,
                watch: true,
                ..Default::default()
            },
        )
        .unwrap();
        frecency
    }

    fn live_files(shared: &SharedFilePicker) -> Vec<String> {
        let guard = shared.read().unwrap();
        let picker = guard.as_ref().unwrap();
        let mut files: Vec<_> = picker
            .get_files()
            .iter()
            .filter(|file| !file.is_deleted())
            .map(|file| file.relative_path(picker))
            .collect();
        files.sort();
        files
    }

    fn files_containing(shared: &SharedFilePicker, text: &str) -> Vec<String> {
        let guard = shared.read().unwrap();
        let picker = guard.as_ref().unwrap();
        let result = picker.grep(&parse_grep_query(text), &GrepSearchOptions::default());
        let mut files: Vec<_> = result
            .files
            .iter()
            .map(|file| file.relative_path(picker))
            .collect();
        files.sort();
        files
    }

    fn indexed_size(shared: &SharedFilePicker, path: &Path) -> Option<u64> {
        let guard = shared.read().unwrap();
        let picker = guard.as_ref()?;
        picker.get_file_by_path(path).map(|file| file.size)
    }

    fn canonical_tempdir() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = crate::path_utils::canonicalize(tmp.path()).unwrap();
        (tmp, root)
    }

    #[test]
    fn changes_after_the_walk_reach_the_index_without_a_rescan() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/removed.rs"), "fn removed() {}\n").unwrap();
        std::fs::write(root.join("src/edited.rs"), "").unwrap();
        std::fs::write(root.join("src/kept.rs"), "fn kept() {}\n").unwrap();
        let changes = root.clone();
        on(&root, Stage::Walked, move || {
            let _ = std::fs::create_dir_all(changes.join("new/empty"));
            let _ = std::fs::write(changes.join("src/added.rs"), "fn added() {}\n");
            let _ = std::fs::remove_file(changes.join("src/removed.rs"));
            let _ = std::fs::write(
                changes.join("src/edited.rs"),
                "const edited_after_walk: u8 = 1;\n",
            );
        });

        let shared = SharedFilePicker::default();
        start(&shared, &root, true);
        assert!(shared.wait_for_watcher(Duration::from_secs(10)));

        let empty = SearchScope::new(&root, &root.join("new/empty")).unwrap();
        eventually("the changes are indexed", || {
            shared
                .read()
                .unwrap()
                .as_ref()
                .is_some_and(|picker| picker.contains_scope(&empty))
                && live_files(&shared) == ["src/added.rs", "src/edited.rs", "src/kept.rs"]
                && files_containing(&shared, "edited_after_walk") == ["src/edited.rs"]
        });
        settle(&shared, &root);
        assert_eq!(times_reached(&root, Stage::Walked), 1);
        assert_eq!(shared.rescan_stats().total, 0);
        assert_eq!(
            files_containing(&shared, "edited_after_walk"),
            ["src/edited.rs"]
        );
    }

    #[test]
    fn a_scan_stays_pending_through_post_scan_and_a_queued_rescan() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let observed = shared.clone();
        // Hooks run on a pool thread, where a panic aborts: they report instead.
        let held = Arc::new(AtomicBool::new(false));
        let report = Arc::clone(&held);
        on(&root, Stage::ContentIndexed, move || {
            let status = observed
                .read()
                .unwrap()
                .as_ref()
                .unwrap()
                .background_status();
            let frecency = SharedFrecency::default();
            let queued = observed.trigger_full_rescan_async(&frecency).is_ok();
            report.store(
                queued && !status.is_scanning() && status.is_scan_pending(),
                Ordering::Release,
            );
        });
        start(&shared, &root, true);
        eventually("the queued rescan walks", || {
            times_reached(&root, Stage::Walked) == 2
        });
        settle(&shared, &root);
        assert!(held.load(Ordering::Acquire), "no pending scan was reported");
        let status = shared.read().unwrap().as_ref().unwrap().background_status();
        assert!(!status.is_scan_pending());
    }

    #[test]
    fn an_edit_after_the_content_index_read_the_file_stays_searchable() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("holder.rs"), "const zebra_quokka: u8 = 0;\n").unwrap();
        std::fs::write(root.join("edited.rs"), "fn edited() {}\n").unwrap();
        for i in 0..3 {
            std::fs::write(root.join(format!("filler{i}.rs")), "fn filler() {}\n").unwrap();
        }
        let shared = SharedFilePicker::default();
        let edited = root.join("edited.rs");
        let observed = shared.clone();
        // Hooks run on a pool thread, where a panic aborts: they report instead.
        let applied = Arc::new(AtomicBool::new(false));
        let edit_applied = Arc::clone(&applied);
        on(&root, Stage::ContentIndexed, move || {
            let content = "const zebra_quokka: u8 = 1;\n";
            let written = std::fs::write(&edited, content).is_ok();
            let seen = poll(|| indexed_size(&observed, &edited) == Some(content.len() as u64));
            edit_applied.store(written && seen, Ordering::Release);
        });

        start(&shared, &root, true);
        settle(&shared, &root);
        assert!(
            applied.load(Ordering::Acquire),
            "the watcher missed the edit"
        );
        let guard = shared.read().unwrap();
        assert!(guard.as_ref().unwrap().bigram_index().is_some());
        drop(guard);
        assert_eq!(
            files_containing(&shared, "zebra_quokka"),
            ["edited.rs", "holder.rs"]
        );
    }

    #[test]
    fn a_hold_beyond_one_batch_falls_back_to_one_rescan() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let watcher_side = shared.clone();
        let changes = root.clone();
        let still_held = Arc::new(AtomicUsize::new(usize::MAX));
        let held_after_burst = Arc::clone(&still_held);
        on(&root, Stage::Walked, move || {
            let _ = std::fs::write(changes.join("late.rs"), "fn late() {}\n");
            let burst = (0..=MAX_BATCH_PATHS)
                .map(|i| {
                    let event = notify::Event::new(EventKind::Create(CreateKind::File))
                        .add_path(changes.join(format!("burst/{i}.rs")));
                    DebouncedEvent::new(event, Instant::now())
                })
                .collect();
            if watcher_side.event_hold().defer(burst).is_none() {
                held_after_burst.store(watcher_side.event_hold().held_count(), Ordering::Release);
            }
        });

        start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(10)));
        assert_eq!(still_held.load(Ordering::Acquire), 0);
        eventually("the fallback rescan walks", || {
            times_reached(&root, Stage::Walked) == 2
        });
        settle(&shared, &root);
        assert_eq!(times_reached(&root, Stage::Walked), 2);
        #[cfg(rescan_stats)]
        assert_eq!(
            shared
                .rescan_stats()
                .count(RescanReason::EventBatchOverflow),
            1
        );
        assert_eq!(live_files(&shared), ["kept.rs", "late.rs"]);
    }

    #[test]
    fn rescans_requested_together_run_one_at_a_time_and_none_is_lost() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let frecency = start(&shared, &root, false);
        settle(&shared, &root);

        // A second request arrives after the first claimed its scan, before it runs.
        let (requester, requester_frecency) = (shared.clone(), frecency.clone());
        on(&root, Stage::Claimed, move || {
            let _ = requester.trigger_full_rescan_async(&requester_frecency);
        });
        // The first walk to end waits to see whether another walk starts meanwhile.
        let observed = root.clone();
        on(&root, Stage::Walked, move || {
            poll_for(Duration::from_secs(2), || {
                times_reached(&observed, Stage::Walking) >= 3
            });
        });
        shared.trigger_full_rescan_async(&frecency).unwrap();

        eventually("the second request runs", || {
            times_reached(&root, Stage::Walked) == 3
        });
        settle(&shared, &root);
        assert_eq!(times_reached(&root, Stage::Walked), 3);
        assert_eq!(most_concurrent_walks(&root), 1);
    }

    #[test]
    fn a_rescan_keeps_changes_the_watcher_reports_during_its_walk() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let frecency = start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(10)));
        settle(&shared, &root);

        // The file appears after the rescan's walk; its event arrives before the commit.
        let changes = root.clone();
        on(&root, Stage::Walked, move || {
            let _ = std::fs::write(changes.join("src/late.rs"), "fn late() {}\n");
            std::thread::sleep(Duration::from_millis(500));
        });
        shared.trigger_full_rescan_async(&frecency).unwrap();
        settle(&shared, &root);

        eventually("the change reported during the walk is indexed", || {
            live_files(&shared) == ["src/kept.rs", "src/late.rs"]
        });
    }

    fn status_after_install(requested: bool, installed: bool, cancelled: bool) -> WatcherStatus {
        let signals = ScanSignals::default();
        let mut guard = WatcherInstallGuard::new(&signals, requested);
        if installed {
            signals.watcher_ready.store(true, Ordering::Release);
            guard.installed();
        }
        if cancelled {
            signals.cancelled.store(true, Ordering::Release);
        }
        drop(guard);
        BackgroundStatus::new(signals, true, Arc::default()).watcher()
    }

    #[test]
    fn a_requested_watcher_that_is_not_installed_fails() {
        assert_eq!(
            status_after_install(true, false, false),
            WatcherStatus::Failed
        );
    }

    #[test]
    fn an_installed_watcher_is_ready() {
        assert_eq!(
            status_after_install(true, true, false),
            WatcherStatus::Ready
        );
    }

    #[test]
    fn cancellation_before_installation_is_a_stop_not_a_failure() {
        assert_eq!(
            status_after_install(true, false, true),
            WatcherStatus::Stopped
        );
    }

    #[test]
    fn a_scan_that_does_not_request_the_watcher_leaves_its_status_alone() {
        assert_eq!(
            status_after_install(false, false, false),
            WatcherStatus::Starting
        );
    }

    fn collected(root: &std::path::Path) -> FilePicker {
        let mut picker = FilePicker::new(FilePickerOptions {
            base_path: root.to_string_lossy().into_owned(),
            enable_mmap_cache: false,
            enable_content_indexing: true,
            watch: false,
            ..Default::default()
        })
        .unwrap();
        picker.collect_files().unwrap();
        picker
    }

    #[test]
    fn replaced_picker_does_not_install_its_bigram_index_into_the_successor() {
        let old_root = tempfile::tempdir().unwrap();
        // The stale index tracks the marker, but only for a file the new root does not have.
        std::fs::write(old_root.path().join("a.txt"), "unrelated words").unwrap();
        std::fs::write(old_root.path().join("b.txt"), "isolated_marker").unwrap();
        let new_root = tempfile::tempdir().unwrap();
        std::fs::write(new_root.path().join("new.txt"), "isolated_marker").unwrap();

        let shared = SharedFilePicker::default();
        let old = collected(old_root.path());
        let signals = old.scan_signals();
        *shared.write().unwrap() = Some(old);
        let snapshot = shared
            .read()
            .unwrap()
            .as_ref()
            .and_then(|picker| unsafe { picker.post_scan_snapshot() })
            .unwrap();
        // The root changes while the old post-scan is still building its index.
        let old = shared.write().unwrap().replace(collected(new_root.path()));
        let config = ScanConfig {
            warmup: false,
            content_indexing: true,
            watch: false,
            auto_cache_budget: false,
            install_watcher: false,
            follow_symlinks: false,
            enable_fs_root_scanning: false,
            enable_home_dir_scanning: false,
        };
        ScanJob::run_post_scan(&shared, &signals, &config, &snapshot);
        drop(snapshot);
        drop(old);

        let guard = shared.read().unwrap();
        let picker = guard.as_ref().unwrap();
        let query = crate::grep::parse_grep_query("isolated_marker");
        let options = crate::grep::GrepSearchOptions {
            time_budget_ms: 0,
            ..Default::default()
        };
        assert_eq!(picker.grep(&query, &options).matches.len(), 1);
    }
}
