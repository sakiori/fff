use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};

use tracing::error;

use crate::FileSync;
use crate::file_picker::{FFFMode, FilePicker, FilePickerIgnoreOptions};
use crate::index::{build_bigram_index, sniff_binary_for_non_indexable};
use crate::parallelism::{BACKGROUND_THREAD_POOL, indexing_pool};
use crate::scm::ScmWorkspace;
use crate::shared::{SharedFilePicker, SharedFrecency};
use crate::walk::DirHook;
use crate::watch::{Message, Published, WatcherOptions};

#[derive(Clone)]
pub(crate) struct ScanSignals {
    /// Set to `true` while any scan phase is running
    pub(crate) scanning: Arc<AtomicBool>,
    /// Set once the watcher is subscribed and every change since the initial walk
    /// began is indexed or queued behind it
    pub(crate) watcher_ready: Arc<AtomicBool>,
    // The watcher's status, which the index's loop computes and publishes with the ready flag.
    watcher: Arc<AtomicU8>,
    /// Indicates that that owning picker was requested to shut down
    pub(crate) cancelled: Arc<AtomicBool>,
    /// Set by `post_scan_snapshot`, cleared by `PostScanSnapshot::drop`.
    /// DO NOT set or clear this manually — it is managed exclusively by the
    /// PostScanSnapshot lifecycle.
    pub(crate) post_scan_indexing_active: Arc<AtomicBool>,
    // A scan runs, post-scan included, or a rescan waits to start right after it: written by
    // the index's loop alone.
    pub(crate) scan_pending: Arc<AtomicBool>,
    // Explicit rescan requests whose rescan has not started yet, counted by their tickets.
    pub(crate) requests_in_flight: Arc<AtomicUsize>,
    // Tells the index apart from any other, also from one replacing it at the same root.
    index: u64,
}

impl Default for ScanSignals {
    fn default() -> Self {
        static NEXT_INDEX: AtomicU64 = AtomicU64::new(1);
        Self {
            scanning: Arc::default(),
            watcher_ready: Arc::default(),
            watcher: Arc::new(AtomicU8::new(Published::Starting as u8)),
            cancelled: Arc::default(),
            post_scan_indexing_active: Arc::default(),
            scan_pending: Arc::default(),
            requests_in_flight: Arc::default(),
            index: NEXT_INDEX.fetch_add(1, Ordering::Relaxed),
        }
    }
}

/// Lifecycle of a picker's filesystem watcher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatcherStatus {
    /// The picker was created without a watcher.
    Disabled,
    /// The watcher is being set up, by the initial scan or by a rebuild replacing a lost one, also
    /// while it waits; subscriptions are taken then, as the rebuild's rescan event covers the gap.
    Starting,
    /// The watcher is installed, and every change since its scan began is indexed or queued.
    Ready,
    /// The requested watcher could not be installed, or the index's first scan failed; the
    /// index is not kept up to date.
    Failed,
    /// The picker was cancelled or its watcher was stopped. A stopped watcher stays stopped:
    /// no later scan of the index installs one, nor replaces a lost one.
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

    /// Whether a scan walks, or a full rescan was requested and has not run yet.
    pub fn is_scanning(&self) -> bool {
        self.signals.is_scanning()
    }

    /// Whether a scan job runs, post-scan phase included, or a requested rescan waits for one.
    pub fn is_scan_pending(&self) -> bool {
        self.signals.scan_pending.load(Ordering::SeqCst) || self.signals.is_scanning()
    }

    pub fn watcher(&self) -> WatcherStatus {
        match self.signals.watcher().status() {
            _ if !self.watch => WatcherStatus::Disabled,
            WatcherStatus::Starting if self.signals.cancelled.load(Ordering::Acquire) => {
                WatcherStatus::Stopped
            }
            status => status,
        }
    }
}

impl ScanSignals {
    pub(crate) fn index_key(&self) -> u64 {
        self.index
    }

    pub(crate) fn watcher(&self) -> Published {
        Published::from_u8(self.watcher.load(Ordering::Acquire))
    }

    // The ready flag is true exactly while the status is ready.
    pub(crate) fn publish_watcher(&self, status: Published) {
        self.watcher.store(status as u8, Ordering::Release);
        let ready = status == Published::Ready;
        self.watcher_ready.store(ready, Ordering::Release);
    }

    // A scan walks, or a rescan requested explicitly has not started yet: as when the caller
    // started it, a request counts from the moment it is made.
    pub(crate) fn is_scanning(&self) -> bool {
        self.scanning.load(Ordering::Acquire) || self.requests_in_flight.load(Ordering::SeqCst) > 0
    }
}

// One explicit rescan request in flight, from the moment it is made until its rescan starts or
// the request is dropped, also by a panic: in-flight counting by a guard the request carries.
pub(crate) struct RequestTicket(Arc<AtomicUsize>);

impl RequestTicket {
    pub(crate) fn new(in_flight: &Arc<AtomicUsize>) -> Self {
        in_flight.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(in_flight))
    }
}

impl Drop for RequestTicket {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Which optional phases a scan should run.
#[derive(Clone, Default, Debug)]
pub(crate) struct ScanConfig {
    pub(crate) warmup: bool,
    pub(crate) content_indexing: bool,
    pub(crate) watch: bool,
    pub(crate) auto_cache_budget: bool,
    pub(crate) install_watcher: bool,
    pub(crate) follow_symlinks: bool,
    pub(crate) ignore_options: FilePickerIgnoreOptions,
    pub(crate) enable_fs_root_scanning: bool,
    pub(crate) enable_home_dir_scanning: bool,
}

impl ScanConfig {
    pub(crate) fn of_rescan(picker: &FilePicker) -> Self {
        Self {
            warmup: picker.has_mmap_cache(),
            content_indexing: picker.has_content_indexing(),
            watch: picker.has_watcher(),
            auto_cache_budget: !picker.has_explicit_cache_budget(),
            // The watcher outlives every rescan of its index.
            install_watcher: false,
            follow_symlinks: picker.follows_symlinks(),
            ignore_options: picker.ignore_options().clone(),
            enable_fs_root_scanning: picker.fs_root_scanning_enabled(),
            enable_home_dir_scanning: picker.home_dir_scanning_enabled(),
        }
    }

    pub(crate) fn watcher_options(&self, mode: FFFMode) -> WatcherOptions {
        WatcherOptions {
            mode,
            follow_symlinks: self.follow_symlinks,
            ignore_options: self.ignore_options.clone(),
            enable_fs_root_scanning: self.enable_fs_root_scanning,
            enable_home_dir_scanning: self.enable_home_dir_scanning,
        }
    }
}

// The walk of one scan, which the index's loop starts and the pool runs: it only walks and
// sends its result back, never waiting for the loop.
pub(crate) struct ScanJob {
    pub(crate) id: u64,
    pub(crate) shared_picker: SharedFilePicker,
    pub(crate) shared_frecency: SharedFrecency,
    pub(crate) base_path: PathBuf,
    pub(crate) mode: FFFMode,
    pub(crate) index: u64,
    pub(crate) config: ScanConfig,
    pub(crate) git: Option<ScmWorkspace>,
    pub(crate) dir_hook: Option<DirHook>,
    /// Walker-maintained counter backing `get_scan_progress` on the UI
    /// side. Reset to 0 at scan start, incremented per-file by the
    /// walker. Shared `Arc` so the UI polls the same atomic.
    pub(crate) scanned_files_counter: Arc<AtomicUsize>,
    pub(crate) trace_span: tracing::Span,
}

impl ScanJob {
    /// Run the walk on `BACKGROUND_THREAD_POOL`. Returns immediately.
    ///
    /// Routed through the pool — and not a fresh `std::thread::spawn` — so the
    /// orchestrator inherits rayon's QoS pin (USER_INITIATED). Without that
    /// pin, an interactive nvim's USER_INTERACTIVE main thread spawns a child
    /// at lower QoS, the walker's Zig worker pool inherits the demotion, and
    /// the kernel drifts those workers onto E-cores. On chromium that turns a
    /// ~800 ms walk into ~3 s.
    pub(crate) fn spawn(self) {
        let span = self.trace_span.clone();
        BACKGROUND_THREAD_POOL.spawn(move || {
            let _g = span.enter();
            self.walk();
        });
    }

    fn walk(self) {
        self.scanned_files_counter.store(0, Ordering::Relaxed);
        #[cfg(test)]
        tests::reached(tests::Stage::Walking, &self.base_path);
        let walked = FileSync::walk_filesystem(
            &self.base_path,
            self.git,
            &self.scanned_files_counter,
            &self.shared_frecency,
            self.mode,
            crate::walk::WalkPolicy {
                follow_symlinks: self.config.follow_symlinks,
                ignore: &self.config.ignore_options,
            },
            self.dir_hook.as_ref(),
        );
        #[cfg(test)]
        let walked = match tests::fails_walk(&self.base_path) {
            true => Err(crate::error::Error::ThreadPanic),
            false => walked,
        };
        if let Err(e) = &walked {
            error!(?e, "scan walk failed");
        }
        #[cfg(test)]
        tests::reached(tests::Stage::Walked, &self.base_path);
        let walked = Box::new(walked);
        let (index, scan) = (self.index, self.id);
        self.shared_picker.send(Message::Walked {
            index,
            scan,
            walked,
        });
    }
}

// The content index and binary sniffing of a published scan, on the pool. The content index goes
// back to the loop, which installs it; the end of the phase ends the scan.
pub(crate) struct PostScan {
    pub(crate) shared_picker: SharedFilePicker,
    pub(crate) index: u64,
    pub(crate) scan: u64,
    pub(crate) signals: ScanSignals,
    pub(crate) config: ScanConfig,
    pub(crate) snapshot: Option<crate::file_picker::PostScanUnsafeSnapshot>,
    pub(crate) known_text: Vec<u64>,
    pub(crate) trace_span: tracing::Span,
}

impl PostScan {
    pub(crate) fn spawn(self) {
        let span = self.trace_span.clone();
        BACKGROUND_THREAD_POOL.spawn(move || {
            let _g = span.enter();
            self.run();
        });
    }

    fn run(self) {
        let Self {
            shared_picker,
            index,
            scan,
            signals,
            config,
            snapshot,
            known_text,
            trace_span: _,
        } = self;
        if let Some(snapshot) = snapshot.filter(|_| !signals.cancelled.load(Ordering::Acquire)) {
            run_post_scan(
                &shared_picker,
                index,
                scan,
                &signals,
                &config,
                &snapshot,
                &known_text,
            );
        }
        // The snapshot is dropped before the scan ends.
        shared_picker.send(Message::PostScanned { index, scan });
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
    index: u64,
    scan: u64,
    signals: &ScanSignals,
    config: &ScanConfig,
    unsafe_snapshot: &crate::file_picker::PostScanUnsafeSnapshot,
    known_text: &[u64],
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
    if config.content_indexing {
        let indexable_count = unsafe_snapshot.indexable_count.min(files.len());
        let (indexable_files, non_indexable_files) = files.split_at(indexable_count);
        let pool = indexing_pool();
        let bigram = build_bigram_index(indexable_files, &unsafe_snapshot.base_path, arena, pool);
        #[cfg(test)]
        tests::reached(tests::Stage::ContentIndexed, &unsafe_snapshot.base_path);
        let bigram = Box::new(bigram);
        shared_picker.send(Message::ContentIndexed {
            index,
            scan,
            bigram,
        });
        crate::index::release_thread_buffers(pool);
        crate::file_picker::hint_allocator_collect(pool);

        // Bigram only sniffs files <= MAX_INDEXABLE_FILE_SIZE; large
        // unknown-extension binaries slip past it and would otherwise be
        // grep-able as text. Cheap header sniff catches those.
        if !signals.cancelled.load(Ordering::Acquire) {
            sniff_binary_for_non_indexable(
                non_indexable_files,
                indexable_count,
                known_text,
                &unsafe_snapshot.base_path,
                arena,
                &signals.cancelled,
            );
        }
    } else {
        // this potentially a long running as we are not parallelizing it but it's okay
        sniff_binary_for_non_indexable(
            files,
            0,
            known_text,
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::file_picker::{FilePicker, FilePickerOptions};
    use crate::grep::{GrepSearchOptions, parse_grep_query};
    use crate::rescan_stats::RescanReason;
    use crate::scope::SearchScope;
    use crate::watch::{MAX_BATCH_PATHS, WatchEventKind};
    use notify::event::{CreateKind, EventKind};
    use notify_debouncer_full::DebouncedEvent;
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Stage {
        Installing,
        Claimed,
        Walking,
        Walked,
        Rechecking,
        Prepared,
        ContentIndexed,
        Finished,
    }

    type Hook = Box<dyn FnOnce() + Send>;

    // Each hook runs once, when the next scan of its base path reaches its stage.
    static HOOKS: Mutex<Vec<(PathBuf, Stage, Hook)>> = Mutex::new(Vec::new());
    static REACHED: Mutex<Vec<(PathBuf, Stage, Instant)>> = Mutex::new(Vec::new());

    // The hook runs before the stage is recorded.
    pub(crate) fn reached(stage: Stage, base_path: &Path) {
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
            .push((base_path.to_path_buf(), stage, Instant::now()));
    }

    static REFUSED_WATCHERS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    // The watcher of the next index of `root` cannot be installed.
    fn refuse_watcher(root: &Path) {
        REFUSED_WATCHERS.lock().unwrap().push(root.to_path_buf());
    }

    // Holds the next scan of `root` at `stage` until the gate opens; `entered` once it is held.
    fn hold(root: &Path, stage: Stage) -> (Arc<AtomicBool>, Arc<AtomicBool>) {
        let gate = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(AtomicBool::new(false));
        let (open, inside) = (Arc::clone(&gate), Arc::clone(&entered));
        on(root, stage, move || {
            inside.store(true, Ordering::SeqCst);
            poll_for(Duration::from_secs(30), || open.load(Ordering::SeqCst));
        });
        (gate, entered)
    }

    static SLOW_PUBLICATIONS: Mutex<Vec<(PathBuf, Duration, Arc<AtomicBool>)>> =
        Mutex::new(Vec::new());

    // The next publication of `root`'s watcher status takes `delay`; the flag is set as it starts.
    fn slow_publication(root: &Path, delay: Duration) -> Arc<AtomicBool> {
        let started = Arc::new(AtomicBool::new(false));
        let entry = (root.to_path_buf(), delay, Arc::clone(&started));
        SLOW_PUBLICATIONS.lock().unwrap().push(entry);
        started
    }

    pub(crate) fn publishing(root: &Path) {
        let slow = {
            let mut slow = SLOW_PUBLICATIONS.lock().unwrap();
            let position = slow.iter().position(|(path, ..)| path == root);
            position.map(|position| slow.remove(position))
        };
        if let Some((_, delay, started)) = slow {
            started.store(true, Ordering::SeqCst);
            std::thread::sleep(delay);
        }
    }

    static FAILING_WALKS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    // The next walk of `root` fails.
    fn fail_walk(root: &Path) {
        FAILING_WALKS.lock().unwrap().push(root.to_path_buf());
    }

    pub(crate) fn fails_walk(root: &Path) -> bool {
        let mut failing = FAILING_WALKS.lock().unwrap();
        let position = failing.iter().position(|path| path == root);
        position.map(|position| failing.remove(position)).is_some()
    }

    static OWED_ALONE: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    static FORGETTING_REBUILDS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    // A status of `root` was published with a replacement owed and no rebuild pending.
    pub(crate) fn owed_alone(root: &Path) {
        OWED_ALONE.lock().unwrap().push(root.to_path_buf());
    }

    fn times_owed_alone(root: &Path) -> usize {
        (OWED_ALONE.lock().unwrap().iter())
            .filter(|path| *path == root)
            .count()
    }

    // The next loss of `root`'s watcher leaves no rebuild pending.
    fn forget_rebuild(root: &Path) {
        FORGETTING_REBUILDS.lock().unwrap().push(root.to_path_buf());
    }

    pub(crate) fn forgets_rebuild(root: &Path) -> bool {
        let mut forgetting = FORGETTING_REBUILDS.lock().unwrap();
        let position = forgetting.iter().position(|path| path == root);
        position
            .map(|position| forgetting.remove(position))
            .is_some()
    }

    pub(crate) fn refuses_watcher(root: &Path) -> bool {
        let mut refused = REFUSED_WATCHERS.lock().unwrap();
        let position = refused.iter().position(|path| path == root);
        position.map(|position| refused.remove(position)).is_some()
    }

    static COMMITS: Mutex<Vec<(PathBuf, Option<String>)>> = Mutex::new(Vec::new());
    static RECORDED: Mutex<Vec<(PathBuf, PathBuf)>> = Mutex::new(Vec::new());

    // Keeps the paths a scan of `root` recorded in its changed-path set.
    pub(crate) fn recorded(root: &Path, paths: Vec<PathBuf>) {
        let mut recorded = RECORDED.lock().unwrap();
        recorded.extend(paths.into_iter().map(|path| (root.to_path_buf(), path)));
    }

    fn recorded_paths(root: &Path) -> Vec<PathBuf> {
        (RECORDED.lock().unwrap().iter())
            .filter(|(at, _)| at == root)
            .map(|(_, path)| path.clone())
            .collect()
    }

    // Records the thread that committed a scan of `base_path`.
    pub(crate) fn committed(base_path: &Path) {
        let thread = std::thread::current().name().map(str::to_owned);
        COMMITS
            .lock()
            .unwrap()
            .push((base_path.to_path_buf(), thread));
    }

    fn committing_threads(root: &Path) -> Vec<Option<String>> {
        (COMMITS.lock().unwrap().iter())
            .filter(|(path, _)| path == root)
            .map(|(_, thread)| thread.clone())
            .collect()
    }

    fn most_concurrent_walks(root: &Path) -> usize {
        let mut walking = 0usize;
        let mut most = 0;
        for (path, stage, _) in REACHED.lock().unwrap().iter() {
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

    pub(crate) fn on(root: &Path, stage: Stage, hook: impl FnOnce() + Send + 'static) {
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
            .filter(|(path, at, _)| path == root && *at == stage)
            .count()
    }

    // When scans of `root` reached `stage`, in order.
    fn when_reached(root: &Path, stage: Stage) -> Vec<Instant> {
        (REACHED.lock().unwrap().iter())
            .filter(|(path, at, _)| path == root && *at == stage)
            .map(|(_, _, when)| *when)
            .collect()
    }

    fn poll(done: impl FnMut() -> bool) -> bool {
        poll_for(Duration::from_secs(20), done)
    }

    pub(crate) fn poll_for(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
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

    // Waits until every scan that walked `root` has finished and none is starting. No status
    // of `root` was published with a replacement owed and no rebuild pending.
    fn settle(shared: &SharedFilePicker, root: &Path) {
        eventually("the scans settle", || {
            let walks = times_reached(root, Stage::Walked);
            let pending = (shared.read().unwrap().as_ref())
                .is_some_and(|picker| picker.background_status().is_scan_pending());
            walks > 0
                && times_reached(root, Stage::Finished) == walks
                && shared.wait_for_indexing_complete(Duration::ZERO)
                && !pending
        });
        assert_eq!(times_owed_alone(root), 0, "a replacement owed alone");
    }

    fn start(shared: &SharedFilePicker, root: &Path, content_indexing: bool) -> SharedFrecency {
        start_with_interval(shared, root, content_indexing, None)
    }

    fn start_with_interval(
        shared: &SharedFilePicker,
        root: &Path,
        content_indexing: bool,
        rescan_interval: Option<Duration>,
    ) -> SharedFrecency {
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
                rescan_cooldown: rescan_interval,
                ..Default::default()
            },
        )
        .unwrap();
        frecency
    }

    fn stop_watcher(shared: &SharedFilePicker) {
        let mut guard = shared.write().unwrap();
        guard.as_mut().unwrap().stop_background_monitor();
    }

    // A stopped index reads stopped, holds no watcher, takes no subscription and owes no rebuild.
    fn assert_stopped(shared: &SharedFilePicker) {
        {
            let guard = shared.read().unwrap();
            let picker = guard.as_ref().unwrap();
            let status = picker.background_status().watcher();
            assert_eq!(status, crate::WatcherStatus::Stopped);
            assert!(
                picker.background_watcher.is_none(),
                "a watcher was installed"
            );
            assert!(!picker.watcher_signal().load(Ordering::SeqCst));
        }
        assert!(shared.watch("", Default::default(), |_, _| {}).is_err());
        assert_eq!(shared.next_rescan_due(), None);
    }

    // Moves in more new files than the index takes in without a rescan, and reports them
    // as a program would.
    fn report_many_files(shared: &SharedFilePicker, root: &Path, dir: &str) -> usize {
        let count = crate::constants::MAX_OVERFLOW_FILES + 64;
        let staging = tempfile::tempdir_in(root.parent().unwrap()).unwrap();
        for i in 0..count {
            std::fs::write(staging.path().join(format!("f{i}.rs")), "").unwrap();
        }
        std::fs::rename(staging.path(), root.join(dir)).unwrap();
        let paths = (0..count)
            .map(|i| root.join(format!("{dir}/f{i}.rs")))
            .collect();
        let _ = shared.apply_reported_changes(&SharedFrecency::default(), paths);
        count
    }

    // An interval no test outlasts.
    const NO_END: Duration = Duration::from_secs(24 * 60 * 60);

    fn start_unwatched(
        shared: &SharedFilePicker,
        root: &Path,
        rescan_interval: Duration,
    ) -> SharedFrecency {
        let frecency = SharedFrecency::default();
        FilePicker::new_with_shared_state(
            shared.clone(),
            frecency.clone(),
            FilePickerOptions {
                base_path: root.to_string_lossy().into_owned(),
                enable_content_indexing: true,
                watch: false,
                rescan_cooldown: Some(rescan_interval),
                ..Default::default()
            },
        )
        .unwrap();
        frecency
    }

    // Drops the last handle and checks that no thread keeps the picker alive.
    fn drops_cleanly(shared: SharedFilePicker) {
        let weak = shared.weaken();
        drop(shared);
        eventually("the picker is dropped", || weak.upgrade().is_none());
    }

    fn live_files_under(shared: &SharedFilePicker, dir: &str) -> usize {
        let prefix = format!("{dir}/");
        live_files(shared)
            .iter()
            .filter(|file| file.starts_with(&prefix))
            .count()
    }

    fn created(paths: impl IntoIterator<Item = PathBuf>) -> Vec<DebouncedEvent> {
        paths
            .into_iter()
            .map(|path| {
                let event = notify::Event::new(EventKind::Create(CreateKind::File)).add_path(path);
                DebouncedEvent::new(event, Instant::now())
            })
            .collect()
    }

    fn removed(path: PathBuf) -> Vec<DebouncedEvent> {
        let event = notify::Event::new(EventKind::Remove(notify::event::RemoveKind::File));
        vec![DebouncedEvent::new(event.add_path(path), Instant::now())]
    }

    // Hands a batch to the index as the watcher does, without waiting for it.
    fn deliver(shared: &SharedFilePicker, events: Vec<DebouncedEvent>) {
        let _ = shared.submit_events(None, events);
    }

    // Runs `work` on the index's writer, as a job.
    fn run_on_writer(
        shared: &SharedFilePicker,
        work: impl FnOnce(&SharedFilePicker) + Send + 'static,
    ) {
        let index = shared.read().unwrap().as_ref().unwrap().signals.index_key();
        shared.submit(index, Box::new(work));
    }

    // Keeps the index's writer busy until the returned flag is set.
    fn occupy_writer(shared: &SharedFilePicker, root: &Path) -> Arc<AtomicBool> {
        let release = Arc::new(AtomicBool::new(false));
        let waiting = Arc::clone(&release);
        let entered = Arc::new(AtomicBool::new(false));
        let busy = Arc::clone(&entered);
        crate::watch::on_applying(root, move || {
            busy.store(true, Ordering::Release);
            poll_for(Duration::from_secs(30), || waiting.load(Ordering::Acquire));
        });
        deliver(shared, created([root.join("occupied.rs")]));
        eventually("the writer is busy", || entered.load(Ordering::Acquire));
        release
    }

    // A repository beside the indexed tree: only the watcher announces a linked worktree of it
    // added into the tree.
    fn repository_beside() -> (tempfile::TempDir, PathBuf) {
        let (tmp, beside) = canonical_tempdir();
        let main = beside.join("main");
        std::fs::create_dir(&main).unwrap();
        std::fs::write(main.join("m.rs"), "fn m() {}\n").unwrap();
        git_init(&main);
        git(&main, &["add", "."]);
        git(&main, &["commit", "-q", "-m", "init"]);
        (tmp, main)
    }

    fn add_worktree(main: &Path, at: &Path) {
        let branch = at.file_name().unwrap().to_string_lossy().into_owned();
        git(
            main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &branch,
                &at.to_string_lossy(),
            ],
        );
    }

    // Collects the work trees repository listeners hear of.
    fn hear_repositories(
        shared: &SharedFilePicker,
    ) -> (crate::ListenerSubscription, Arc<Mutex<Vec<PathBuf>>>) {
        let heard: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
        let sink = Arc::clone(&heard);
        let id = shared.subscribe_repository_state(move |work_tree| {
            sink.lock().unwrap().push(work_tree.components().collect());
        });
        (id, heard)
    }

    fn git_init(root: &Path) {
        let init = std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(init.status.success());
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
            // Requested, it counts as running until it has run.
            report.store(
                queued && status.is_scanning() && status.is_scan_pending(),
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
    fn a_change_reported_to_an_unwatched_index_while_it_scans_is_applied_by_the_scan() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let frecency = start_unwatched(&shared, &root, NO_END);
        settle(&shared, &root);

        // A file the walk did not see is reported while the rescan walks.
        let (reporter, reported) = (shared.clone(), root.join("reported.rs"));
        let answer: Arc<Mutex<Option<crate::ReportedChanges>>> = Arc::default();
        let answered = Arc::clone(&answer);
        on(&root, Stage::Walked, move || {
            let _ = std::fs::write(&reported, "fn reported() {}\n");
            let report = on_a_thread(move || {
                reporter.apply_reported_changes(&SharedFrecency::default(), vec![reported])
            });
            *answered.lock().unwrap() = report.recv_timeout(Duration::from_secs(10)).ok();
        });
        shared.trigger_full_rescan_async(&frecency).unwrap();
        eventually("the rescan ran", || {
            times_reached(&root, Stage::Walked) == 2
        });
        settle(&shared, &root);
        assert_eq!(*answer.lock().unwrap(), Some(crate::ReportedChanges::Held));
        assert_eq!(live_files(&shared), ["kept.rs", "reported.rs"]);
    }

    // A build writes into an ignored directory and an editor saves sources while a walk runs,
    // more paths than one batch takes: no second full scan follows, and no ignored path counts.
    fn steady_writes_during_a_long_walk(rescan: bool) {
        let (_tmp, root) = canonical_tempdir();
        git_init(&root);
        std::fs::write(root.join(".gitignore"), "build/\n").unwrap();
        std::fs::create_dir_all(root.join("build")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/kept.rs"), "fn kept() {}\n").unwrap();
        let writes = root.clone();
        let (wrote, written) = std::sync::mpsc::channel();
        let busy_walk = move || {
            for round in 0..14 {
                for i in 0..400 {
                    let _ = std::fs::write(writes.join(format!("build/o{round}_{i}.o")), "x");
                }
                let _ = std::fs::write(writes.join(format!("src/s{round}.rs")), "fn s() {}\n");
                std::thread::sleep(Duration::from_millis(60));
            }
            let _ = wrote.send(());
        };
        // The writes are the test's own and slow down with the machine's disk load: the product's
        // time is counted from their end.
        let after_the_writes = || {
            let limit = Duration::from_secs(300);
            written
                .recv_timeout(limit)
                .expect("the walk never reached its hook, or the test's writes did not end");
        };

        let shared = SharedFilePicker::default();
        if rescan {
            let frecency = start(&shared, &root, false);
            assert!(shared.wait_for_watcher(Duration::from_secs(20)));
            settle(&shared, &root);
            on(&root, Stage::Walking, busy_walk);
            shared.trigger_full_rescan_async(&frecency).unwrap();
            after_the_writes();
            eventually("the rescan walks", || {
                times_reached(&root, Stage::Walked) == 2
            });
        } else {
            on(&root, Stage::Walking, busy_walk);
            start(&shared, &root, false);
            after_the_writes();
            assert!(shared.wait_for_watcher(Duration::from_secs(20)));
        }
        settle(&shared, &root);
        let sources = (std::fs::read_dir(root.join("src")).unwrap()).count();
        eventually("the saved sources are indexed", || {
            live_files_under(&shared, "src") == sources
        });
        let built = (std::fs::read_dir(root.join("build")).unwrap()).count();
        assert!(built > MAX_BATCH_PATHS, "only {built} files built");
        assert_eq!(times_reached(&root, Stage::Walked), 1 + usize::from(rescan));
        let stats = shared.rescan_stats();
        assert_eq!(stats.watcher_triggered(), 0, "{stats}");
        assert_eq!(live_files_under(&shared, "build"), 0);
        let recorded = recorded_paths(&root);
        assert!(
            recorded
                .iter()
                .any(|path| path.starts_with(root.join("src"))),
            "{recorded:?}"
        );
        // The ignored directory itself is for the reconcile to judge; nothing below it counts.
        let build = root.join("build");
        assert!(
            !(recorded.iter()).any(|path| path.starts_with(&build) && *path != build),
            "ignored paths entered the set: {recorded:?}"
        );
    }

    #[test]
    fn steady_writes_during_a_long_first_walk_end_without_a_second_full_scan() {
        steady_writes_during_a_long_walk(false);
    }

    #[test]
    fn steady_writes_during_a_long_rescan_end_without_another_full_scan() {
        steady_writes_during_a_long_walk(true);
    }

    #[test]
    fn changes_during_the_first_walk_are_filtered_with_the_repository_s_rules() {
        let (_tmp, root) = canonical_tempdir();
        git_init(&root);
        std::fs::write(root.join("devenv.nix"), "a").unwrap();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let (watcher_side, changes) = (shared.clone(), root.clone());
        on(&root, Stage::Walked, move || {
            // Names the non-git heuristic takes for a virtual environment.
            let _ = std::fs::write(changes.join("devenv.nix"), "a longer content");
            let _ = std::fs::write(changes.join("devenv.lock"), "x");
            let paths = [changes.join("devenv.nix"), changes.join("devenv.lock")];
            deliver(&watcher_side, created(paths));
        });
        start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(10)));
        settle(&shared, &root);
        assert_eq!(indexed_size(&shared, &root.join("devenv.nix")), Some(16));
        assert!(live_files(&shared).contains(&"devenv.lock".to_owned()));
    }

    #[test]
    fn a_hold_beyond_one_batch_reconciles_what_changed_without_a_rescan() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::create_dir_all(root.join("src/gone")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/pkg0")).unwrap();
        for file in [
            "src/removed.rs",
            "src/gone/old.rs",
            "kept.rs",
            "top_removed.rs",
        ] {
            std::fs::write(root.join(file), "fn f() {}\n").unwrap();
        }
        let shared = SharedFilePicker::default();
        let watcher_side = shared.clone();
        let changes = root.clone();
        on(&root, Stage::Walked, move || {
            let _ = std::fs::write(changes.join("src/added.rs"), "fn added() {}\n");
            let _ = std::fs::remove_file(changes.join("src/removed.rs"));
            let _ = std::fs::remove_dir_all(changes.join("src/gone"));
            let _ = std::fs::create_dir_all(changes.join("lib/new"));
            let _ = std::fs::write(changes.join("lib/new/nested.rs"), "fn nested() {}\n");
            let _ = std::fs::write(changes.join("top.rs"), "fn top() {}\n");
            let _ = std::fs::remove_file(changes.join("top_removed.rs"));
            let _ = std::fs::write(changes.join("node_modules/pkg0/index.js"), "");
            // An install in an ignored directory reports far more paths than one batch takes.
            let installed = (0..=MAX_BATCH_PATHS)
                .map(|i| changes.join(format!("node_modules/pkg{i}/index.js")));
            let real = [
                "src/added.rs",
                "src/removed.rs",
                "src/gone",
                "src/gone/old.rs",
                "lib/new",
                "lib/new/nested.rs",
                "top.rs",
                "top_removed.rs",
            ]
            .map(|rel| changes.join(rel));
            deliver(&watcher_side, created(installed.chain(real)));
        });

        start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(10)));
        let expected = ["kept.rs", "lib/new/nested.rs", "src/added.rs", "top.rs"];
        eventually("the changes are indexed", || {
            live_files(&shared) == expected
        });
        settle(&shared, &root);
        assert_eq!(times_reached(&root, Stage::Walked), 1);
        assert_eq!(shared.rescan_stats().total, 0);
        assert_eq!(live_files(&shared), expected);
    }

    #[test]
    fn lost_events_during_a_scan_are_logged_once_and_classified_as_live_ones() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("elsewhere")).unwrap();
        std::fs::write(root.join("src/kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let (watcher_side, changes) = (shared.clone(), root.clone());
        on(&root, Stage::Walked, move || {
            let _ = std::fs::write(changes.join("src/unreported.rs"), "fn u() {}\n");
            // A symlink the index does not follow holds nothing it indexes.
            #[cfg(unix)]
            let _ = std::os::unix::fs::symlink(changes.join("elsewhere"), changes.join("link"));
            let lost = |path: PathBuf| {
                let event = notify::Event::new(EventKind::Other)
                    .set_flag(notify::event::Flag::Rescan)
                    .add_path(path);
                DebouncedEvent::new(event, Instant::now())
            };
            deliver(
                &watcher_side,
                vec![lost(changes.join("src")), lost(changes.join("link"))],
            );
        });
        start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(10)));
        settle(&shared, &root);
        eventually("the lost subtree is checked again", || {
            live_files(&shared).contains(&"src/unreported.rs".to_owned())
        });
        assert_eq!(crate::watch::recrawls_logged(&root.join("src")), 1);
        assert!(
            !live_files(&shared)
                .iter()
                .any(|file| file.starts_with("link"))
        );
        assert_eq!(shared.rescan_stats().total, 0);
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@e")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@e")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn a_linked_worktree_s_exclude_file_in_its_common_dir_applies_without_a_rescan() {
        let (_main_tmp, main) = canonical_tempdir();
        let (_tmp, parent) = canonical_tempdir();
        git_init(&main);
        std::fs::write(main.join("a.rs"), "fn a() {}\n").unwrap();
        git(&main, &["add", "."]);
        git(&main, &["commit", "-m", "first"]);
        let root = parent.join("wt");
        git(&main, &["worktree", "add", root.to_str().unwrap()]);
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/guide.md"), "guide\n").unwrap();
        let shared = SharedFilePicker::default();
        start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(10)));
        settle(&shared, &root);
        // A listener follows the refs, which the common dir outside the tree holds.
        let _refs = shared.subscribe_ref_changes(&root, |_| {});
        assert!(live_files(&shared).contains(&"docs/guide.md".to_owned()));

        let exclude = main.join(".git/info/exclude");
        std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        // The watcher follows the common dir's `info` directory.
        std::fs::write(&exclude, "docs/\n").unwrap();
        eventually("the exclude rule applies", || {
            !live_files(&shared).contains(&"docs/guide.md".to_owned())
        });
        assert!(live_files(&shared).contains(&"a.rs".to_owned()));
        assert_eq!(shared.rescan_stats().total, 0);
    }

    #[test]
    fn a_first_publication_that_panics_leaves_the_watcher_ready_once_recovered() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        // The first scan's publication fails halfway, on the index's writer.
        let changes = root.clone();
        let watcher_side = shared.clone();
        on(&root, Stage::Walked, move || {
            deliver(&watcher_side, created([changes.join("kept.rs")]));
        });
        on(&root, Stage::Rechecking, || {
            panic!("a publication failed halfway")
        });
        start(&shared, &root, false);
        eventually("the recovery rescan walks", || {
            times_reached(&root, Stage::Walked) == 2
        });
        settle(&shared, &root);
        assert!(shared.wait_for_watcher(Duration::from_secs(10)));
        let status = shared.read().unwrap().as_ref().unwrap().background_status();
        assert_eq!(status.watcher(), WatcherStatus::Ready);
        assert_eq!(live_files(&shared), ["kept.rs"]);
        assert_eq!(times_owed_alone(&root), 0);
    }

    #[test]
    fn a_hold_announces_the_git_state_it_recorded() {
        let (_tmp, root) = canonical_tempdir();
        git_init(&root);
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let heard: Arc<Mutex<Vec<(&str, PathBuf)>>> = Arc::default();
        let (state, refs) = (Arc::clone(&heard), Arc::clone(&heard));
        let _state = shared.subscribe_repository_state(move |work_tree| {
            state
                .lock()
                .unwrap()
                .push(("state", work_tree.to_path_buf()));
        });
        let _refs = shared.subscribe_ref_changes(&root, move |work_tree| {
            refs.lock().unwrap().push(("refs", work_tree.to_path_buf()));
        });
        let watcher_side = shared.clone();
        let changes = root.clone();
        let cleared = Arc::clone(&heard);
        on(&root, Stage::Walked, move || {
            // A commit's index and branch updates arrive among an install's paths.
            let installed =
                (0..=MAX_BATCH_PATHS).map(|i| changes.join(format!("deps/pkg{i}/index.js")));
            let git = [".git/index", ".git/refs/heads/main", ".git/logs/HEAD"]
                .map(|rel| changes.join(rel));
            deliver(&watcher_side, created(installed.chain(git)));
            cleared.lock().unwrap().clear();
        });

        start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(10)));
        let wanted = [("refs", root.clone()), ("state", root.clone())];
        eventually("the repository is announced", || {
            let heard = heard.lock().unwrap();
            wanted.iter().all(|wanted| heard.contains(wanted))
        });
        settle(&shared, &root);
        assert_eq!(shared.rescan_stats().total, 0);
    }

    #[test]
    fn a_removal_reported_before_a_rescan_never_undoes_what_the_rescan_found() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("x.rs"), "fn x() {}\n").unwrap();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let frecency = start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(10)));
        settle(&shared, &root);

        // While the first rescan's recorded changes are checked again, a removal of x.rs
        // arrives, which x.rs, there again, outdates before it is applied.
        let (watcher_side, changes) = (shared.clone(), root.clone());
        on(&root, Stage::Walked, move || {
            deliver(&watcher_side, created([changes.join("kept.rs")]));
        });
        // The removal is slow to apply, and a second rescan walks meanwhile.
        let taken = Arc::new(AtomicBool::new(false));
        let (watcher_side, changes) = (shared.clone(), root.clone());
        let report = Arc::clone(&taken);
        on(&root, Stage::Rechecking, move || {
            deliver(&watcher_side, removed(changes.join("x.rs")));
            let observed = changes.clone();
            crate::watch::on_applying(&changes, move || {
                report.store(true, Ordering::Release);
                poll_for(Duration::from_secs(3), || {
                    times_reached(&observed, Stage::Walked) >= 3
                });
            });
        });
        shared.trigger_full_rescan_async(&frecency).unwrap();
        eventually("the removal is being applied", || {
            taken.load(Ordering::Acquire)
        });
        shared.trigger_full_rescan_async(&frecency).unwrap();
        eventually("the second rescan ends", || {
            times_reached(&root, Stage::Finished) == 3
        });
        settle(&shared, &root);
        let status = shared.read().unwrap().as_ref().unwrap().background_status();
        eventually("the index is idle", || !status.is_scan_pending());
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(live_files(&shared), ["kept.rs", "x.rs"]);
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

    #[test]
    fn a_listener_reporting_changes_neither_deadlocks_nor_is_lost() {
        let (_tmp, root) = canonical_tempdir();
        git_init(&root);
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let frecency = start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(10)));
        settle(&shared, &root);

        // The listener reports a file of its own whenever it hears of the repository.
        let (reporter, reported) = (shared.weaken(), root.join("reported.rs"));
        std::fs::write(&reported, "fn reported() {}\n").unwrap();
        let answers: Arc<Mutex<Vec<crate::ReportedChanges>>> = Arc::default();
        let answered = Arc::clone(&answers);
        let reporter_frecency = frecency.clone();
        let _state = shared.subscribe_repository_state(move |_| {
            if let Some(picker) = reporter.upgrade() {
                let paths = vec![reported.clone()];
                let answer = picker.apply_reported_changes(&reporter_frecency, paths);
                answered.lock().unwrap().push(answer);
            }
        });
        deliver(&shared, created([root.join(".git/index")]));
        eventually("the listener's change is applied", || {
            live_files(&shared).contains(&"reported.rs".to_owned())
        });
        assert_eq!(
            *answers.lock().unwrap(),
            [crate::ReportedChanges::Applied],
            "a listener waits for its changes like any program"
        );

        std::fs::write(root.join("later.rs"), "fn later() {}\n").unwrap();
        let (reporter, later) = (shared.clone(), vec![root.join("later.rs")]);
        let applied = on_a_thread(move || reporter.apply_reported_changes(&frecency, later));
        assert_eq!(
            applied.recv_timeout(Duration::from_secs(10)),
            Ok(crate::ReportedChanges::Applied)
        );
        assert!(live_files(&shared).contains(&"later.rs".to_owned()));
    }

    #[test]
    fn scans_of_other_indexes_go_on_while_writers_are_busy() {
        let threads = crate::parallelism::BACKGROUND_THREAD_POOL.current_num_threads();
        let mut busy = Vec::new();
        for _ in 0..threads + 2 {
            let (tmp, root) = canonical_tempdir();
            std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
            let shared = SharedFilePicker::default();
            let frecency = start_unwatched(&shared, &root, NO_END);
            settle(&shared, &root);
            // A rescan walks, then waits to publish behind the busy writer.
            shared.trigger_full_rescan_async(&frecency).unwrap();
            eventually("the rescan walks", || {
                times_reached(&root, Stage::Walking) == 2
            });
            let release = occupy_writer(&shared, &root);
            busy.push((tmp, root, shared, release));
        }
        eventually("the rescans walk", || {
            (busy.iter())
                .filter(|(_, root, _, _)| times_reached(root, Stage::Walked) == 2)
                .count()
                == busy.len()
        });

        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("b.rs"), "fn b() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let started = Instant::now();
        start(&shared, &root, false);
        let ready = shared.wait_for_watcher(Duration::from_secs(5));
        let waited = started.elapsed();
        for (_, _, _, release) in &busy {
            release.store(true, Ordering::Release);
        }
        for (_, root, shared, _) in &busy {
            settle(shared, root);
            assert_eq!(times_reached(root, Stage::Finished), 2);
        }
        assert!(
            ready,
            "an unrelated index waited {waited:?} for a pool thread"
        );
    }

    #[test]
    fn the_watcher_s_changes_merge_while_the_writer_is_busy() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::create_dir(root.join("src")).unwrap();
        let shared = SharedFilePicker::default();
        start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(10)));
        settle(&shared, &root);
        let index = shared.read().unwrap().as_ref().unwrap().signals.index_key();
        let release = occupy_writer(&shared, &root);
        let before = crate::watch::jobs_queued(index);
        // Changes keep arriving, batch after batch, while the writer is busy.
        for i in 0..60 {
            std::fs::write(root.join(format!("src/f{i}.rs")), "fn f() {}\n").unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
        let queued_while_busy = crate::watch::jobs_queued(index) - before;
        release.store(true, Ordering::Release);
        eventually("every change is indexed", || {
            live_files_under(&shared, "src") == 60
        });
        // The watcher waits for the one batch it handed over; the rest merges meanwhile.
        assert!(
            queued_while_busy <= 1,
            "{queued_while_busy} batches queued while the writer was busy"
        );
    }

    #[test]
    fn a_writer_that_cannot_start_drops_its_job_outside_its_lock() {
        // Dropping the job asks the writer for a wake-up, as a released scan claim does.
        struct WakesTheWriter(SharedFilePicker);
        impl Drop for WakesTheWriter {
            fn drop(&mut self) {
                self.0.wake_writer();
            }
        }
        let shared = SharedFilePicker::default();
        let handle = shared.clone();
        let finished = on_a_thread(move || {
            crate::watch::refuse_next_spawn();
            let probe = WakesTheWriter(handle.clone());
            handle.submit(1, Box::new(move |_| drop(probe)));
        });
        assert!(
            finished.recv_timeout(Duration::from_secs(10)).is_ok(),
            "submitting deadlocked"
        );
    }

    #[test]
    fn a_cancelled_index_drops_its_pending_rescan_and_its_writer_waits() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let frecency = start_unwatched(&shared, &root, NO_END);
        settle(&shared, &root);
        let reason = RescanReason::OverflowCapacity;
        assert!(
            !shared
                .trigger_full_rescan_with_reason(&frecency, reason)
                .unwrap()
        );

        // A program cancels the index while others still hold it, and asks for a rescan.
        shared.cancel();
        shared.trigger_full_rescan_async(&frecency).unwrap();
        let status = shared.read().unwrap().as_ref().unwrap().background_status();
        assert!(!status.is_scan_pending());
        assert_eq!(shared.next_rescan_due(), None);
        std::thread::sleep(Duration::from_millis(100));
        let before = crate::watch::writer_loops(&root);
        std::thread::sleep(Duration::from_millis(500));
        let loops = crate::watch::writer_loops(&root) - before;
        assert!(loops < 20, "the writer went round {loops} times in 500 ms");
        assert_eq!(times_reached(&root, Stage::Walked), 1);
    }

    #[test]
    fn live_changes_leave_the_scan_readiness_alone() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(10)));
        settle(&shared, &root);
        // A live change is being applied: no scan runs or waits.
        let release = occupy_writer(&shared, &root);
        let status = shared.read().unwrap().as_ref().unwrap().background_status();
        let pending = status.is_scan_pending();
        let watcher = shared.wait_for_watcher(Duration::from_millis(300));
        let indexed = shared.wait_for_indexing_complete(Duration::from_millis(300));
        release.store(true, Ordering::Release);
        settle(&shared, &root);
        assert_eq!((pending, watcher, indexed), (false, true, true));
    }

    // As when the caller started it, a requested rescan counts as running from the moment it is
    // requested until it has run, also while the loop is busy.
    #[test]
    fn a_requested_rescan_counts_as_scanning_until_it_has_run() {
        let (_tmp, root) = canonical_tempdir();
        for i in 0..50 {
            std::fs::write(root.join(format!("f{i}.rs")), "fn f() {}\n").unwrap();
        }
        let shared = SharedFilePicker::default();
        let frecency = start_unwatched(&shared, &root, NO_END);
        settle(&shared, &root);
        for _ in 0..10 {
            shared.trigger_full_rescan_async(&frecency).unwrap();
            assert!(shared.read().unwrap().as_ref().unwrap().is_scan_active());
            settle(&shared, &root);
        }
        let release = occupy_writer(&shared, &root);
        shared.trigger_full_rescan_async(&frecency).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let status = shared.read().unwrap().as_ref().unwrap().background_status();
        let scanning = status.is_scanning() && status.is_scan_pending();
        release.store(true, Ordering::Release);
        settle(&shared, &root);
        assert!(
            scanning,
            "a rescan the busy loop has not taken yet counts as running"
        );
    }

    // A rescan requested while the post-scan phase runs starts right after it; waiting for the
    // scan waits for that one too.
    #[test]
    fn waiting_for_the_scan_waits_for_a_rescan_requested_during_post_scan() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let frecency = start(&shared, &root, true);
        settle(&shared, &root);
        let (entered, release) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let (hook_entered, hook_release) = (entered.clone(), release.clone());
        on(&root, Stage::ContentIndexed, move || {
            hook_entered.store(true, Ordering::SeqCst);
            poll(|| hook_release.load(Ordering::SeqCst));
        });
        shared.trigger_full_rescan_async(&frecency).unwrap();
        eventually("the post-scan phase runs", || {
            entered.load(Ordering::SeqCst)
        });
        shared.trigger_full_rescan_async(&frecency).unwrap();
        // The loop takes the request while the post-scan phase runs.
        std::thread::sleep(Duration::from_millis(300));
        let returned = shared.wait_for_scan(Duration::ZERO);
        release.store(true, Ordering::SeqCst);
        eventually("the requested rescan walks", || {
            times_reached(&root, Stage::Walked) == 3
        });
        settle(&shared, &root);
        assert!(
            !returned,
            "the wait returned before the requested rescan started"
        );
    }

    // A panic starting a requested rescan leaves no request counted: the recovery rescan runs and
    // the index is ready again.
    #[test]
    fn a_panic_starting_an_explicit_rescan_leaves_readiness_recoverable() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let frecency = start(&shared, &root, false);
        settle(&shared, &root);
        on(&root, Stage::Claimed, || {
            panic!("starting a rescan failed halfway")
        });
        shared.trigger_full_rescan_async(&frecency).unwrap();
        eventually("the recovery rescan walks", || {
            times_reached(&root, Stage::Walked) == 2
        });
        assert!(shared.wait_for_scan(Duration::from_secs(10)));
        settle(&shared, &root);
        let status = shared.read().unwrap().as_ref().unwrap().background_status();
        assert!(!status.is_scan_pending() && !status.is_scanning());
        assert_eq!(times_owed_alone(&root), 0);
    }

    // The recovery rescan panics where the first attempt did: readiness returns, and the
    // recovery waits the interval from the start that panicked.
    #[test]
    fn a_recovery_that_panics_again_waits_the_interval() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let frecency = start_unwatched(&shared, &root, NO_END);
        settle(&shared, &root);
        on(&root, Stage::Claimed, || {
            panic!("starting the rescan failed halfway")
        });
        on(&root, Stage::Claimed, || {
            panic!("starting the recovery failed halfway")
        });
        shared.trigger_full_rescan_async(&frecency).unwrap();
        assert!(shared.wait_for_scan(Duration::from_secs(10)));
        let status = shared.read().unwrap().as_ref().unwrap().background_status();
        assert!(!status.is_scanning() && !status.is_scan_pending());
        let due = shared.next_rescan_due();
        assert!(due.is_some_and(|due| due > Instant::now() + Duration::from_secs(60)));
        assert_eq!(times_reached(&root, Stage::Walked), 1);
        assert_eq!(times_owed_alone(&root), 0);
    }

    // A turn that panics while the initial scan waits for its watcher keeps that scan, which
    // walks once the watcher reports in: the index follows the tree.
    #[test]
    fn a_recovery_before_the_watcher_reports_in_keeps_the_initial_scan() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::create_dir_all(root.join("sub/deeper")).unwrap();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        std::fs::write(root.join("sub/deeper/a.rs"), "fn a() {}\n").unwrap();
        crate::watch::watch_per_directory_for(&root);
        crate::watch::slow_stream_starts_for(&root, Duration::from_secs(2));
        let shared = SharedFilePicker::default();
        start(&shared, &root, false);
        crate::watch::on_applying(&root, || panic!("a batch failed halfway"));
        deliver(&shared, created([root.join("kept.rs")]));
        assert!(shared.wait_for_watcher(Duration::from_secs(20)));
        settle(&shared, &root);
        assert_eq!(times_reached(&root, Stage::Walked), 1);
        std::fs::write(root.join("sub/deeper/late.rs"), "fn late() {}\n").unwrap();
        eventually("a change in a subdirectory is seen", || {
            live_files(&shared).contains(&"sub/deeper/late.rs".to_owned())
        });
        assert_eq!(times_owed_alone(&root), 0);
    }

    // A task of the watcher's thread that panics leaves what its streams watch unknown: the
    // watcher stops, readiness says so, and the index is rebuilt with a new one.
    #[test]
    fn a_watcher_task_that_panics_has_the_index_rebuilt_with_a_new_watcher() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(20)));
        settle(&shared, &root);
        crate::watch::panicking_task_for(&root);
        // Following refs is a task of the watcher's thread.
        let _refs = shared.subscribe_ref_changes(&root, |_| {});
        eventually("the index is rebuilt", || {
            times_reached(&root, Stage::Walked) == 2
        });
        assert!(shared.wait_for_watcher(Duration::from_secs(20)));
        settle(&shared, &root);
        std::fs::write(root.join("after.rs"), "fn after() {}\n").unwrap();
        eventually("a later change is seen", || {
            live_files(&shared).contains(&"after.rs".to_owned())
        });
        assert_eq!(times_owed_alone(&root), 0);
    }

    // A watcher that keeps failing rebuilds the index at once, then once per interval, as a
    // supervisor's restart limit has it: no two rebuilds start closer than the interval.
    #[test]
    fn a_watcher_that_keeps_failing_rebuilds_once_per_interval() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let interval = Duration::from_millis(800);
        let shared = SharedFilePicker::default();
        start_with_interval(&shared, &root, false, Some(interval));
        assert!(shared.wait_for_watcher(Duration::from_secs(20)));
        settle(&shared, &root);
        for _ in 0..40 {
            crate::watch::panicking_task_for(&root);
        }
        let lost = Instant::now();
        // Following refs is a task of the watcher's thread, as is each new watcher's first one.
        let _refs = shared.subscribe_ref_changes(&root, |_| {});
        std::thread::sleep(interval * 5 + interval / 2);
        let started = when_reached(&root, Stage::Claimed);
        assert!(started.len() >= 4, "rebuilds started at {started:?}");
        assert!(started[0].saturating_duration_since(lost) < interval / 2);
        let early = Duration::from_millis(20);
        for pair in started.windows(2) {
            let gap = pair[1].saturating_duration_since(pair[0]);
            assert!(gap + early >= interval, "rebuilds {gap:?} apart");
        }
        assert_eq!(times_owed_alone(&root), 0);
    }

    // A rescan requested while a rebuild runs starts right after it, and counts until it does,
    // also when the rebuild's new watcher is lost and the next rebuild waits the interval.
    #[test]
    fn an_explicit_rescan_keeps_its_turn_when_a_recovery_waits() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        start_with_interval(&shared, &root, false, Some(Duration::from_secs(60)));
        assert!(shared.wait_for_watcher(Duration::from_secs(20)));
        settle(&shared, &root);
        let gate = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(AtomicBool::new(false));
        {
            let (gate, entered) = (Arc::clone(&gate), Arc::clone(&entered));
            on(&root, Stage::Walking, move || {
                entered.store(true, Ordering::SeqCst);
                poll_for(Duration::from_secs(30), || gate.load(Ordering::SeqCst));
            });
        }
        crate::watch::panicking_task_for(&root);
        drop(shared.subscribe_ref_changes(&root, |_| {}));
        eventually("the rebuild walks", || entered.load(Ordering::SeqCst));
        let status = shared.read().unwrap().as_ref().unwrap().background_status();
        shared
            .trigger_full_rescan_async(&SharedFrecency::default())
            .unwrap();
        // The rebuild's new watcher is lost as well.
        crate::watch::panicking_task_for(&root);
        drop(shared.subscribe_ref_changes(&root, |_| {}));
        std::thread::sleep(Duration::from_millis(300));
        assert!(status.is_scanning() && status.is_scan_pending());
        let signals = shared.read().unwrap().as_ref().unwrap().scan_signals();
        assert_eq!(signals.requests_in_flight.load(Ordering::SeqCst), 1);
        gate.store(true, Ordering::SeqCst);
        eventually("the requested rescan starts", || {
            times_reached(&root, Stage::Claimed) == 2
        });
        assert_eq!(times_owed_alone(&root), 0);
    }

    // Turns that panic more often than the interval join the waiting rebuild: it starts when
    // it was due, an interval after the last one started.
    #[test]
    fn turns_that_keep_panicking_never_postpone_the_rebuild() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let interval = Duration::from_secs(1);
        let shared = SharedFilePicker::default();
        start_with_interval(&shared, &root, false, Some(interval));
        assert!(shared.wait_for_watcher(Duration::from_secs(20)));
        settle(&shared, &root);
        let panics = Instant::now();
        while panics.elapsed() < interval * 9 / 10 {
            crate::watch::on_applying(&root, || panic!("a batch failed halfway"));
            deliver(&shared, created([root.join("kept.rs")]));
            std::thread::sleep(interval / 8);
        }
        std::thread::sleep((interval * 3 / 2).saturating_sub(panics.elapsed()));
        let started = when_reached(&root, Stage::Claimed);
        assert!(started.len() >= 2, "rebuilds started at {started:?}");
        let gap = started[1].saturating_duration_since(started[0]);
        assert!(gap < interval + interval / 4, "the rebuild waited {gap:?}");
        assert_eq!(times_owed_alone(&root), 0);
    }

    // A rebuild after a loss that installs no watcher and whose publication panics leaves the
    // index unwatched: it reads failed and takes no subscriptions, also once rebuilt again.
    #[test]
    fn a_rebuild_without_a_watcher_whose_publication_panics_reads_failed() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        start_with_interval(&shared, &root, false, Some(Duration::from_millis(500)));
        assert!(shared.wait_for_watcher(Duration::from_secs(20)));
        settle(&shared, &root);
        refuse_watcher(&root);
        on(&root, Stage::Rechecking, || {
            panic!("the publication failed halfway")
        });
        crate::watch::panicking_task_for(&root);
        drop(shared.subscribe_ref_changes(&root, |_| {}));
        eventually("rebuilt again", || times_reached(&root, Stage::Walked) == 3);
        settle(&shared, &root);
        let status = shared.read().unwrap().as_ref().unwrap().background_status();
        assert_eq!(status.watcher(), crate::WatcherStatus::Failed);
        let watch = shared.watch("", crate::watch::WatchOptions::default(), |_, _| {});
        assert!(watch.is_err());
        let asked = Instant::now();
        assert!(!shared.wait_for_watcher(Duration::from_secs(5)));
        assert!(asked.elapsed() < Duration::from_secs(1));
        assert_eq!(times_owed_alone(&root), 0);
    }

    // A rebuild after a loss whose walk fails keeps its new watcher, which reports, and is
    // owed again: it runs once the interval has passed.
    #[test]
    fn a_rebuild_whose_walk_fails_keeps_its_watcher_and_runs_again() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let (_beside, main) = repository_beside();
        start_with_interval(&shared, &root, false, Some(Duration::from_secs(3)));
        assert!(shared.wait_for_watcher(Duration::from_secs(20)));
        settle(&shared, &root);
        let (_state, heard) = hear_repositories(&shared);
        fail_walk(&root);
        crate::watch::panicking_task_for(&root);
        drop(shared.subscribe_ref_changes(&root, |_| {}));
        eventually("the rebuild's walk ends", || {
            times_reached(&root, Stage::Walked) == 2
        });
        settle(&shared, &root);
        let status = shared.read().unwrap().as_ref().unwrap().background_status();
        assert_eq!(status.watcher(), crate::WatcherStatus::Ready);
        // Its new watcher tells listeners before the rebuild runs again.
        let added = root.join("added");
        add_worktree(&main, &added);
        let soon = poll_for(Duration::from_millis(1500), || {
            heard.lock().unwrap().contains(&added)
        });
        assert!(soon && times_reached(&root, Stage::Walked) == 2);
        eventually("rebuilt again", || times_reached(&root, Stage::Walked) == 3);
        settle(&shared, &root);
        assert_eq!(status.watcher(), crate::WatcherStatus::Ready);
        std::fs::write(root.join("after.rs"), "fn after() {}\n").unwrap();
        eventually("a later change is seen", || {
            live_files(&shared).contains(&"after.rs".to_owned())
        });
        assert_eq!(times_owed_alone(&root), 0);
    }

    // A lost watcher whose replacement waits its interval reads starting, never failed: watch
    // subscriptions stay open and are taken, as the rebuild's rescan event covers the wait.
    #[test]
    fn a_replacement_that_waits_its_interval_reads_starting_and_takes_subscriptions() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let interval = Duration::from_secs(2);
        let shared = SharedFilePicker::default();
        start_with_interval(&shared, &root, false, Some(interval));
        assert!(shared.wait_for_watcher(Duration::from_secs(20)));
        settle(&shared, &root);
        let open = shared.watch("", Default::default(), |_, _| {}).unwrap();
        let status = shared.read().unwrap().as_ref().unwrap().background_status();
        let failed = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let sampler = {
            let (failed, done, status) = (failed.clone(), done.clone(), status.clone());
            std::thread::spawn(move || {
                while !done.load(Ordering::Acquire) {
                    if status.watcher() == crate::WatcherStatus::Failed {
                        failed.store(true, Ordering::Release);
                    }
                    std::thread::sleep(Duration::from_micros(200));
                }
            })
        };
        // The rebuild's new watcher is lost too: the next rebuild waits the interval.
        crate::watch::panicking_task_for(&root);
        crate::watch::panicking_task_for(&root);
        drop(shared.subscribe_ref_changes(&root, |_| {}));
        eventually("the next rebuild waits", || {
            let due = shared.next_rescan_due();
            due.is_some_and(|due| due > Instant::now() + interval / 4)
        });
        assert_eq!(status.watcher(), crate::WatcherStatus::Starting);
        assert!(shared.is_watch_active(open));
        let taken = shared.watch("", Default::default(), |_, _| {});
        assert!(taken.is_ok(), "{:?}", taken.err());
        assert!(shared.wait_for_watcher(Duration::from_secs(20)));
        assert!(times_reached(&root, Stage::Walked) >= 3);
        done.store(true, Ordering::Release);
        sampler.join().unwrap();
        assert!(
            !failed.load(Ordering::Acquire),
            "read failed while replaced"
        );
        assert!(shared.is_watch_active(open));
        assert_eq!(times_owed_alone(&root), 0);
    }

    // A stop while the loop publishes the status is ordered with it by the picker's lock: the
    // stopped watcher reads stopped.
    #[test]
    fn a_stop_while_the_status_is_published_reads_stopped() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(20)));
        settle(&shared, &root);
        let publishing = slow_publication(&root, Duration::from_millis(400));
        shared.wake_writer();
        eventually("the status is being published", || {
            publishing.load(Ordering::SeqCst)
        });
        shared
            .write()
            .unwrap()
            .as_mut()
            .unwrap()
            .stop_background_monitor();
        std::thread::sleep(Duration::from_millis(800));
        let guard = shared.read().unwrap();
        let picker = guard.as_ref().unwrap();
        assert_eq!(
            picker.background_status().watcher(),
            crate::WatcherStatus::Stopped
        );
        assert!(!picker.watcher_signal().load(Ordering::SeqCst));
    }

    // Subscriptions opened again and again while a lost watcher is replaced are never refused.
    #[test]
    fn subscriptions_are_never_refused_while_a_watcher_is_replaced() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(20)));
        settle(&shared, &root);
        let done = Arc::new(AtomicBool::new(false));
        let opener = {
            let (shared, done) = (shared.clone(), done.clone());
            std::thread::spawn(move || {
                let mut refused = 0;
                while !done.load(Ordering::Acquire) {
                    match shared.watch("", Default::default(), |_, _| {}) {
                        Ok(id) => drop(shared.unwatch(id)),
                        Err(_) => refused += 1,
                    }
                }
                refused
            })
        };
        crate::watch::panicking_task_for(&root);
        drop(shared.subscribe_ref_changes(&root, |_| {}));
        eventually("rebuilt", || times_reached(&root, Stage::Walked) == 2);
        assert!(shared.wait_for_watcher(Duration::from_secs(20)));
        settle(&shared, &root);
        done.store(true, Ordering::Release);
        assert_eq!(opener.join().unwrap(), 0, "subscriptions refused");
        assert_eq!(times_owed_alone(&root), 0);
    }

    // An initial scan whose watcher could not be installed reads starting until it ends, then
    // failed.
    #[test]
    fn an_initial_scan_whose_watcher_failed_reads_starting_until_it_ends() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        refuse_watcher(&root);
        let (gate, entered) = hold(&root, Stage::Walking);
        let shared = SharedFilePicker::default();
        start(&shared, &root, false);
        eventually("the initial walk runs", || entered.load(Ordering::SeqCst));
        let status = shared.read().unwrap().as_ref().unwrap().background_status();
        let during = status.watcher();
        gate.store(true, Ordering::SeqCst);
        settle(&shared, &root);
        assert_eq!(during, crate::WatcherStatus::Starting);
        assert_eq!(status.watcher(), crate::WatcherStatus::Failed);
    }

    // Lost events in the git dir of a repository outside the tree announce its linked worktree
    // once to repository and to ref listeners.
    #[test]
    fn lost_events_outside_the_tree_announce_a_worktree_once() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let (_beside, main) = repository_beside();
        let worktree = root.join("wt");
        add_worktree(&main, &worktree);
        crate::watch::watch_per_directory_for(&root);
        let shared = SharedFilePicker::default();
        start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(20)));
        settle(&shared, &root);
        let (_state, heard) = hear_repositories(&shared);
        let refs: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
        let sink = Arc::clone(&refs);
        let _refs = shared.subscribe_ref_changes(&root, move |work_tree| {
            sink.lock().unwrap().push(work_tree.components().collect());
        });
        std::thread::sleep(Duration::from_millis(800));
        shared.watch_registry().wait_for_dispatch();
        heard.lock().unwrap().clear();
        refs.lock().unwrap().clear();
        let lost = notify::Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);
        let lost = lost.add_path(main.join(".git"));
        deliver(&shared, vec![DebouncedEvent::new(lost, Instant::now())]);
        eventually("the worktree is announced", || {
            refs.lock().unwrap().contains(&worktree)
        });
        std::thread::sleep(Duration::from_millis(1000));
        shared.watch_registry().wait_for_dispatch();
        for (listeners, heard) in [("repository", &heard), ("ref", &refs)] {
            let heard = heard.lock().unwrap().clone();
            let times = (heard.iter()).filter(|known| **known == worktree).count();
            assert_eq!(times, 1, "{listeners}: {heard:?}");
        }
    }

    // A rebuild's quiet watcher installed before its publication panicked tells listeners at
    // once, not only after the rebuild that waits its interval.
    #[test]
    fn a_watcher_installed_before_a_publication_panicked_announces_at_once() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let (_beside, main) = repository_beside();
        let shared = SharedFilePicker::default();
        start_with_interval(&shared, &root, false, Some(Duration::from_secs(4)));
        assert!(shared.wait_for_watcher(Duration::from_secs(20)));
        settle(&shared, &root);
        let (_state, heard) = hear_repositories(&shared);
        on(&root, Stage::Rechecking, || {
            panic!("the publication failed halfway")
        });
        crate::watch::panicking_task_for(&root);
        drop(shared.subscribe_ref_changes(&root, |_| {}));
        eventually("the rebuild walks", || {
            times_reached(&root, Stage::Walked) == 2
        });
        settle(&shared, &root);
        let added = root.join("added");
        add_worktree(&main, &added);
        let soon = poll_for(Duration::from_millis(1500), || {
            heard.lock().unwrap().contains(&added)
        });
        assert!(soon && times_reached(&root, Stage::Walked) == 2);
        assert_eq!(times_owed_alone(&root), 0);
    }

    // A stopped watcher stays stopped through a later rescan.
    #[test]
    fn a_stopped_watcher_reads_stopped_after_a_rescan() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let frecency = start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(20)));
        settle(&shared, &root);
        let (status, signal) = {
            let mut guard = shared.write().unwrap();
            let picker = guard.as_mut().unwrap();
            picker.stop_background_monitor();
            (picker.background_status(), picker.watcher_signal())
        };
        assert_eq!(status.watcher(), crate::WatcherStatus::Stopped);
        shared.trigger_full_rescan_async(&frecency).unwrap();
        eventually("rescanned", || times_reached(&root, Stage::Walked) == 2);
        settle(&shared, &root);
        assert_eq!(status.watcher(), crate::WatcherStatus::Stopped);
        assert!(!signal.load(Ordering::SeqCst));
        assert!(shared.watch("", Default::default(), |_, _| {}).is_err());
    }

    // A stop while a lost watcher's replacement waits its interval holds: no rebuild stays owed
    // only to replace it, none installs a watcher, and a rescan asked for still runs.
    #[test]
    fn a_stop_while_a_replacement_waits_installs_no_watcher() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let interval = Duration::from_secs(2);
        let shared = SharedFilePicker::default();
        let frecency = start_with_interval(&shared, &root, false, Some(interval));
        assert!(shared.wait_for_watcher(Duration::from_secs(20)));
        settle(&shared, &root);
        // The rebuild's new watcher is lost too: the next rebuild waits the interval.
        crate::watch::panicking_task_for(&root);
        crate::watch::panicking_task_for(&root);
        drop(shared.subscribe_ref_changes(&root, |_| {}));
        eventually("the next rebuild waits", || {
            let due = shared.next_rescan_due();
            due.is_some_and(|due| due > Instant::now() + interval / 4)
        });
        settle(&shared, &root);
        let walks = times_reached(&root, Stage::Walked);
        stop_watcher(&shared);
        assert_stopped(&shared);
        std::thread::sleep(interval + Duration::from_millis(500));
        assert_eq!(times_reached(&root, Stage::Walked), walks);
        assert_stopped(&shared);
        shared.trigger_full_rescan_async(&frecency).unwrap();
        eventually("rescanned", || {
            times_reached(&root, Stage::Walked) == walks + 1
        });
        settle(&shared, &root);
        assert_stopped(&shared);
    }

    // A stop while a rebuild installs its new watcher drops that watcher once it reports in.
    #[test]
    fn a_stop_while_a_rebuild_installs_its_watcher_installs_none() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(20)));
        settle(&shared, &root);
        crate::watch::slow_stream_starts_for(&root, Duration::from_millis(1500));
        crate::watch::panicking_task_for(&root);
        drop(shared.subscribe_ref_changes(&root, |_| {}));
        eventually("the rebuild installs its watcher", || {
            times_reached(&root, Stage::Claimed) == 1
        });
        std::thread::sleep(Duration::from_millis(300));
        stop_watcher(&shared);
        assert_stopped(&shared);
        settle(&shared, &root);
        assert_eq!(times_reached(&root, Stage::Walked), 2);
        assert_stopped(&shared);
    }

    // A stop while the initial scan installs its watcher drops that watcher once it reports in.
    #[test]
    fn a_stop_during_the_initial_install_installs_no_watcher() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        crate::watch::slow_stream_starts_for(&root, Duration::from_millis(1500));
        let shared = SharedFilePicker::default();
        start(&shared, &root, false);
        eventually("the initial scan installs its watcher", || {
            times_reached(&root, Stage::Installing) == 1
        });
        std::thread::sleep(Duration::from_millis(300));
        stop_watcher(&shared);
        assert_stopped(&shared);
        settle(&shared, &root);
        assert_eq!(times_reached(&root, Stage::Walked), 1);
        assert_stopped(&shared);
    }

    // A repair still runs after a stop, and one whose walk failed is retried as a repair one
    // interval later; neither installs a watcher.
    #[test]
    fn a_repair_whose_walk_failed_after_a_stop_is_retried() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let interval = Duration::from_secs(1);
        let shared = SharedFilePicker::default();
        let frecency = start_with_interval(&shared, &root, false, Some(interval));
        assert!(shared.wait_for_watcher(Duration::from_secs(20)));
        settle(&shared, &root);
        stop_watcher(&shared);
        let walks = times_reached(&root, Stage::Walking);
        let failing = root.clone();
        on(&root, Stage::Rechecking, move || {
            fail_walk(&failing);
            panic!("the rescan's publication failed");
        });
        shared.trigger_full_rescan_async(&frecency).unwrap();
        // The rescan, its repair, whose walk fails, and the repair's retry.
        eventually("the repair is retried", || {
            times_reached(&root, Stage::Walking) == walks + 3
        });
        settle(&shared, &root);
        std::thread::sleep(interval + Duration::from_millis(500));
        assert_eq!(times_reached(&root, Stage::Walking), walks + 3);
        assert_stopped(&shared);
    }

    // A replacement owed with no rebuild pending is counted where the status is published.
    #[test]
    fn a_replacement_owed_with_no_rebuild_pending_is_counted() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(20)));
        settle(&shared, &root);
        forget_rebuild(&root);
        crate::watch::panicking_task_for(&root);
        drop(shared.subscribe_ref_changes(&root, |_| {}));
        eventually("the breach is counted", || times_owed_alone(&root) > 0);
    }

    // A watch subscription during an index's first walk is refused, as before it reported in.
    #[test]
    fn a_subscription_during_the_first_walk_is_refused() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let (gate, entered) = hold(&root, Stage::Walking);
        let shared = SharedFilePicker::default();
        start(&shared, &root, false);
        eventually("the first walk runs", || entered.load(Ordering::SeqCst));
        let watch = shared.watch("", Default::default(), |_, _| {});
        gate.store(true, Ordering::SeqCst);
        assert!(matches!(watch, Err(crate::error::Error::WatcherNotReady)));
        settle(&shared, &root);
    }

    // The post-scan phase counts as a pending scan.
    #[test]
    fn the_post_scan_phase_is_a_pending_scan() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let observed = shared.clone();
        let held = Arc::new(AtomicBool::new(false));
        let report = Arc::clone(&held);
        on(&root, Stage::ContentIndexed, move || {
            let guard = observed.read().unwrap();
            let status = guard.as_ref().unwrap().background_status();
            report.store(status.is_scan_pending(), Ordering::Release);
        });
        start(&shared, &root, true);
        eventually("the post-scan ran", || {
            times_reached(&root, Stage::ContentIndexed) == 1
        });
        settle(&shared, &root);
        assert!(
            held.load(Ordering::Acquire),
            "the post-scan phase was not pending"
        );
    }

    // A cancelled index reads no pending scan and a stopped watcher, though a rebuild waited to
    // start right after the running scan.
    #[test]
    fn a_cancelled_index_waits_for_no_rebuild() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let frecency = start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(20)));
        settle(&shared, &root);
        let (gate, entered) = hold(&root, Stage::Walking);
        shared.trigger_full_rescan_async(&frecency).unwrap();
        eventually("the rescan walks", || entered.load(Ordering::SeqCst));
        // Lost while the rescan walks: its rebuild waits to start right after it.
        crate::watch::panicking_task_for(&root);
        drop(shared.subscribe_ref_changes(&root, |_| {}));
        let status = shared.read().unwrap().as_ref().unwrap().background_status();
        eventually("the watcher is lost", || {
            (shared.read().unwrap().as_ref()).is_some_and(|p| p.background_watcher.is_none())
        });
        shared.read().unwrap().as_ref().unwrap().cancel();
        gate.store(true, Ordering::SeqCst);
        eventually("nothing is pending", || !status.is_scan_pending());
        assert_eq!(status.watcher(), crate::WatcherStatus::Stopped);
        assert_eq!(times_owed_alone(&root), 0);
    }

    // A new watcher watches every repository its walk meets: the git state of a nested one is
    // followed again after the rebuild that replaced a lost watcher.
    #[test]
    fn a_replacement_watcher_follows_nested_repositories() {
        let (_tmp, root) = canonical_tempdir();
        let sub = root.join("sub");
        std::fs::create_dir(&sub).unwrap();
        git_init(&sub);
        std::fs::write(sub.join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(sub.join("b.rs"), "fn b() {}\n").unwrap();
        crate::watch::watch_per_directory_for(&root);
        let shared = SharedFilePicker::default();
        start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(20)));
        settle(&shared, &root);
        let heard: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
        let sink = Arc::clone(&heard);
        let _state = shared.subscribe_repository_state(move |work_tree| {
            sink.lock().unwrap().push(work_tree.to_path_buf());
        });
        crate::watch::panicking_task_for(&root);
        let _refs = shared.subscribe_ref_changes(&root, |_| {});
        eventually("the index is rebuilt", || {
            times_reached(&root, Stage::Walked) == 2
        });
        assert!(shared.wait_for_watcher(Duration::from_secs(20)));
        settle(&shared, &root);
        std::thread::sleep(Duration::from_millis(300));
        heard.lock().unwrap().clear();
        git(&sub, &["add", "b.rs"]);
        eventually("the nested repository's git state is heard", || {
            heard.lock().unwrap().contains(&sub)
        });
        assert_eq!(times_owed_alone(&root), 0);
    }

    // A watcher lost while the initial scan walks is being replaced, never failed: readiness
    // goes from starting to ready.
    #[test]
    fn a_watcher_lost_during_the_initial_walk_is_never_reported_failed() {
        let (_tmp, root) = canonical_tempdir();
        for d in 0..20 {
            let dir = root.join(format!("d{d}"));
            std::fs::create_dir(&dir).unwrap();
            for f in 0..20 {
                std::fs::write(dir.join(format!("f{f}.rs")), "fn f() {}\n").unwrap();
            }
        }
        // The first task is the one each new watcher gets once installed.
        crate::watch::panicking_task_for(&root);
        let shared = SharedFilePicker::default();
        start(&shared, &root, false);
        let status = shared.read().unwrap().as_ref().unwrap().background_status();
        let failed = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let sampler = {
            let (failed, done, status) = (failed.clone(), done.clone(), status.clone());
            std::thread::spawn(move || {
                while !done.load(Ordering::Acquire) {
                    if status.watcher() == crate::WatcherStatus::Failed {
                        failed.store(true, Ordering::Release);
                    }
                    std::thread::sleep(Duration::from_micros(200));
                }
            })
        };
        assert!(shared.wait_for_watcher(Duration::from_secs(20)));
        eventually("the index is rebuilt", || {
            times_reached(&root, Stage::Walked) == 2
        });
        settle(&shared, &root);
        done.store(true, Ordering::Release);
        sampler.join().unwrap();
        assert!(
            !failed.load(Ordering::Acquire),
            "reported failed while replaced"
        );
        assert_eq!(status.watcher(), crate::WatcherStatus::Ready);
        assert_eq!(times_owed_alone(&root), 0);
    }

    // Watch subscriptions are accepted while a lost watcher is replaced: the rebuild's rescan
    // event covers what changed meanwhile.
    #[test]
    fn watch_subscriptions_are_accepted_while_a_watcher_is_replaced() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(20)));
        settle(&shared, &root);
        let opened = Arc::new(Mutex::new(None));
        let (result, subscriber, within) = (opened.clone(), shared.clone(), root.clone());
        on(&root, Stage::Walking, move || {
            let options = crate::watch::WatchOptions::default;
            let watch = subscriber.watch("", options(), |_, _| {}).map(|_| ());
            let inside = (subscriber.watch_within(&within, "", options(), |_, _| {})).map(|_| ());
            *result.lock().unwrap() = Some((watch.is_ok(), inside.is_ok()));
        });
        crate::watch::panicking_task_for(&root);
        let _refs = shared.subscribe_ref_changes(&root, |_| {});
        eventually("the index is rebuilt", || {
            times_reached(&root, Stage::Walked) == 2
        });
        settle(&shared, &root);
        assert_eq!(*opened.lock().unwrap(), Some((true, true)));
        assert_eq!(times_owed_alone(&root), 0);
    }

    // Nothing watches git state between a lost watcher and its replacement: once the new one is
    // in place, repository and ref listeners hear of the repositories they follow.
    #[test]
    fn listeners_hear_of_git_changes_made_while_a_watcher_was_replaced() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        git_init(&root);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "init"]);
        let shared = SharedFilePicker::default();
        start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(20)));
        settle(&shared, &root);
        let refs: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
        let state: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
        let sink = refs.clone();
        let _refs = shared.subscribe_ref_changes(&root, move |work_tree| {
            sink.lock().unwrap().push(work_tree.to_path_buf());
        });
        let sink = state.clone();
        let _state = shared.subscribe_repository_state(move |work_tree| {
            sink.lock().unwrap().push(work_tree.to_path_buf());
        });
        std::thread::sleep(Duration::from_millis(300));
        let gap = root.clone();
        on(&root, Stage::Claimed, move || {
            git(&gap, &["branch", "gap"]);
            std::fs::write(gap.join("staged.rs"), "fn staged() {}\n").unwrap();
            git(&gap, &["add", "staged.rs"]);
        });
        crate::watch::panicking_task_for(&root);
        refs.lock().unwrap().clear();
        state.lock().unwrap().clear();
        let _again = shared.subscribe_ref_changes(&root, |_| {});
        eventually("the index is rebuilt", || {
            times_reached(&root, Stage::Walked) == 2
        });
        settle(&shared, &root);
        eventually("the listeners hear of the repository", || {
            refs.lock().unwrap().contains(&root) && state.lock().unwrap().contains(&root)
        });
        assert_eq!(times_owed_alone(&root), 0);
    }

    // A rebuild's quiet walk tells nothing; its announcement and ref sync name once each repository
    // it follows. Listeners hear each once, natively at least once: FSEvents may deliver setup late.
    fn a_rebuild_tells_each_repository_once(watch_with: fn(&Path), native: bool) {
        let (_tmp, root) = canonical_tempdir();
        let (_outside, beside) = canonical_tempdir();
        let main = beside.join("main");
        std::fs::create_dir(&main).unwrap();
        std::fs::write(main.join("m.rs"), "fn m() {}\n").unwrap();
        let sub = root.join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        std::fs::write(sub.join("a.rs"), "fn a() {}\n").unwrap();
        for repo in [&main, &root, &sub] {
            git_init(repo);
            git(repo, &["add", "."]);
            git(repo, &["commit", "-q", "-m", "init"]);
        }
        let worktree = root.join("wt");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "wt",
                &worktree.to_string_lossy(),
            ],
        );
        watch_with(&root);
        let shared = SharedFilePicker::default();
        let created = Instant::now();
        FilePicker::new_with_shared_state(
            shared.clone(),
            SharedFrecency::default(),
            FilePickerOptions {
                base_path: root.to_string_lossy().into_owned(),
                watch: true,
                ..Default::default()
            },
        )
        .unwrap();
        // A native watcher's three setup starts, of the tree's stream, its own git state and a
        // walked repository's, wait for the daemon to register them: that time is apart.
        let limit = Duration::from_secs(30);
        let ready = || shared.wait_for_watcher(Duration::ZERO);
        let watched = crate::watch::within_product_time(&root, created, limit, 3, ready);
        assert!(watched, "the watcher is not ready");
        settle(&shared, &root);
        let (_state, heard) = hear_repositories(&shared);
        let refs: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
        let sink = Arc::clone(&refs);
        let _refs = shared.subscribe_ref_changes(&root, move |work_tree| {
            sink.lock().unwrap().push(work_tree.components().collect());
        });
        std::thread::sleep(Duration::from_millis(800));
        shared.watch_registry().wait_for_dispatch();
        heard.lock().unwrap().clear();
        refs.lock().unwrap().clear();
        let since = crate::watch::tasks_started();
        let starts = crate::watch::stream_starts(&root);
        crate::watch::panicking_task_for(&root);
        let lost = Instant::now();
        drop(shared.subscribe_ref_changes(&root, |_| {}));
        // A native watcher's thread starts its streams: it asks for the first within 10 s.
        if native {
            let started = poll_for(Duration::from_secs(10), || {
                crate::watch::stream_starts(&root) > starts
            });
            assert!(started, "the new watcher started no stream within 10 s");
        }
        let rebuilt = crate::watch::within_product_time(&root, lost, limit, 3, || {
            times_reached(&root, Stage::Walked) == 2 && shared.wait_for_watcher(Duration::ZERO)
        });
        assert!(rebuilt, "timed out waiting until the index is rebuilt");
        settle(&shared, &root);
        let repos = [&root, &sub, &worktree];
        let settled = Instant::now();
        let heard_all = crate::watch::within_product_time(&root, settled, limit, 0, || {
            let (heard, refs) = (heard.lock().unwrap(), refs.lock().unwrap());
            (repos.iter()).all(|repo| heard.contains(*repo) && refs.contains(*repo))
        });
        assert!(
            heard_all,
            "timed out waiting until each repository is heard"
        );
        std::thread::sleep(Duration::from_millis(1500));
        shared.watch_registry().wait_for_dispatch();

        let tasks = crate::watch::tasks_told(&root, since);
        let announcing: Vec<_> = (tasks.iter())
            .filter(|task| task.kind == "Announce")
            .collect();
        let kinds: Vec<_> = (tasks.iter()).map(|task| (task.task, task.kind)).collect();
        assert_eq!(announcing.len(), 1, "announcements: {kinds:?}");
        let announced = announcing[0];
        let new_watcher = |task: &&crate::watch::TaskTold| task.watcher == announced.watcher;
        let synced = (tasks.iter().filter(new_watcher))
            .find(|task| task.task > announced.task && task.kind == "SyncRefs naming")
            .expect("the announcement's ref sync");
        for repo in repos {
            let times = |told: &[PathBuf]| (told.iter()).filter(|known| *known == repo).count();
            let (repositories, named) = (&announced.repositories, &synced.refs);
            assert_eq!(
                times(repositories),
                1,
                "{} in {repositories:?}",
                repo.display()
            );
            assert_eq!(times(named), 1, "{} in {named:?}", repo.display());
        }
        for walked in (tasks.iter().filter(new_watcher)).filter(|task| task.task < announced.task) {
            assert!(
                walked.repositories.is_empty() && walked.refs.is_empty(),
                "the quiet {} task {} told {:?} and {:?}",
                walked.kind,
                walked.task,
                walked.repositories,
                walked.refs
            );
        }
        for (listeners, heard) in [("repository", &heard), ("ref", &refs)] {
            let heard = heard.lock().unwrap().clone();
            for repo in repos {
                let times = (heard.iter()).filter(|known| *known == repo).count();
                let expected = if native { times >= 1 } else { times == 1 };
                assert!(expected, "{listeners}: {} in {heard:?}", repo.display());
            }
        }
        // The new watcher is quiet no more (polled and per-directory variants): a worktree added
        // later is announced.
        let later = root.join("later");
        heard_later(&shared, &heard, &root, &main, &later, native);
        assert_eq!(times_owed_alone(&root), 0);
    }

    #[test]
    fn listeners_hear_once_of_each_repository_after_a_rebuild_polled() {
        a_rebuild_tells_each_repository_once(crate::watch::poll_watcher_for, false);
    }

    #[test]
    fn listeners_hear_once_of_each_repository_after_a_rebuild_per_directory() {
        a_rebuild_tells_each_repository_once(crate::watch::watch_per_directory_for, false);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_rebuild_tells_each_repository_once_natively() {
        a_rebuild_tells_each_repository_once(crate::watch::native_watcher_for, true);
    }

    // A watcher whose setup panics fails as any other: the initial scan goes on unwatched.
    #[test]
    fn a_watcher_setup_that_panics_fails_as_any_other() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        crate::watch::panicking_stream_starts_for(&root);
        let shared = SharedFilePicker::default();
        start(&shared, &root, false);
        assert!(shared.wait_for_scan(Duration::from_secs(10)));
        settle(&shared, &root);
        let status = shared.read().unwrap().as_ref().unwrap().background_status();
        assert_eq!(status.watcher(), crate::WatcherStatus::Failed);
        assert_eq!(live_files(&shared), ["kept.rs"]);
    }

    // Past the capacity a reported edit of a file the index holds is still applied, as the call
    // answers: only the new files wait for the rescan.
    #[test]
    fn a_reported_edit_beside_a_directory_past_the_capacity_is_applied() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let frecency = start_unwatched(&shared, &root, NO_END);
        settle(&shared, &root);
        let edited = "fn kept_and_much_longer_now() {}\n";
        std::fs::write(root.join("kept.rs"), edited).unwrap();
        let count = crate::constants::MAX_OVERFLOW_FILES + 64;
        let staging = tempfile::tempdir_in(root.parent().unwrap()).unwrap();
        for i in 0..count {
            std::fs::write(staging.path().join(format!("f{i}.rs")), "").unwrap();
        }
        std::fs::rename(staging.path(), root.join("big")).unwrap();
        let reported = vec![root.join("kept.rs"), root.join("big")];
        let answer = shared.apply_reported_changes(&frecency, reported);
        assert_eq!(answer, crate::ReportedChanges::Applied);
        let size = (shared.read().unwrap().as_ref())
            .and_then(|picker| Some(picker.get_file_by_path(root.join("kept.rs"))?.size));
        assert_eq!(size, Some(edited.len() as u64));
        assert_eq!(live_files(&shared), ["kept.rs"]);
    }

    // More new files than the index takes are left unread to the full rescan.
    #[test]
    fn a_directory_moved_in_past_the_capacity_is_left_unread_to_a_rescan() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        start_unwatched(&shared, &root, NO_END);
        settle(&shared, &root);
        report_many_files(&shared, &root, "big");
        assert_eq!(times_reached(&root, Stage::Prepared), 0);
        assert_eq!(live_files(&shared), ["kept.rs"]);
        assert!(
            shared.next_rescan_due().is_some(),
            "a full rescan is pending"
        );
        let (started, remover) = (Instant::now(), shared.clone());
        let removed = on_a_thread(move || remover.remove_index());
        let removed = removed.recv_timeout(Duration::from_secs(30));
        let waited = started.elapsed();
        assert!(
            removed.is_ok() && waited < Duration::from_secs(2),
            "the removal waited {waited:?}"
        );
    }

    // Cancelling the index stops a reconcile between the files it reads.
    #[test]
    fn a_cancelled_index_reads_no_more_files() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let frecency = start_unwatched(&shared, &root, NO_END);
        settle(&shared, &root);
        let staging = tempfile::tempdir_in(root.parent().unwrap()).unwrap();
        for i in 0..200 {
            std::fs::write(staging.path().join(format!("f{i}.rs")), "").unwrap();
        }
        std::fs::rename(staging.path(), root.join("new")).unwrap();
        let canceller = shared.weaken();
        on(&root, Stage::Prepared, move || {
            if let Some(shared) = canceller.upgrade() {
                shared.cancel();
            }
        });
        let _ = shared.apply_reported_changes(&frecency, vec![root.join("new")]);
        assert_eq!(times_reached(&root, Stage::Prepared), 1);
        assert_eq!(live_files(&shared), ["kept.rs"]);
    }

    // A pool thread busy elsewhere never holds up another index's post-scan.
    #[test]
    fn a_busy_pool_thread_never_holds_up_another_index_s_post_scan() {
        let (_tmp, busy) = canonical_tempdir();
        let (_other_tmp, other) = canonical_tempdir();
        std::fs::write(busy.join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(other.join("b.rs"), "fn b() {}\n").unwrap();
        let release = Arc::new(AtomicBool::new(false));
        let waiting = release.clone();
        on(&busy, Stage::Walking, move || {
            poll_for(Duration::from_secs(30), || waiting.load(Ordering::SeqCst));
        });
        let busy_shared = SharedFilePicker::default();
        start_unwatched(&busy_shared, &busy, NO_END);
        eventually("the busy walk holds its thread", || {
            HOOKS
                .lock()
                .unwrap()
                .iter()
                .all(|(path, _, _)| *path != busy)
        });
        let shared = SharedFilePicker::default();
        start_unwatched(&shared, &other, NO_END);
        let indexed = shared.wait_for_indexing_complete(Duration::from_secs(10));
        release.store(true, Ordering::SeqCst);
        settle(&busy_shared, &busy);
        assert!(
            indexed,
            "the post-scan waited for a pool thread busy elsewhere"
        );
    }

    // A watcher whose streams start slowly delays no reported change, replacement or removal:
    // the streams start on the watcher's own thread.
    #[test]
    fn a_slow_stream_start_holds_up_nothing_on_the_loop() {
        let (_tmp, root) = canonical_tempdir();
        let (_other_tmp, other) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        std::fs::write(other.join("b.rs"), "fn b() {}\n").unwrap();
        crate::watch::slow_stream_starts_for(&root, Duration::from_secs(4));
        let shared = SharedFilePicker::default();
        let frecency = start(&shared, &root, false);
        let (quick, limit) = (Duration::from_millis(1500), Duration::from_secs(30));
        let (started, reporter) = (Instant::now(), shared.clone());
        let kept = vec![root.join("kept.rs")];
        let reported = on_a_thread(move || reporter.apply_reported_changes(&frecency, kept));
        let (reported, waited) = (reported.recv_timeout(limit), started.elapsed());
        assert!(
            reported.is_ok() && waited < quick,
            "a reported change waited {waited:?}"
        );
        let (started, replacer) = (Instant::now(), shared.clone());
        let replaced = on_a_thread(move || drop(start_unwatched(&replacer, &other, NO_END)));
        let (replaced, waited) = (replaced.recv_timeout(limit), started.elapsed());
        assert!(
            replaced.is_ok() && waited < quick,
            "the replacement waited {waited:?}"
        );
        let (started, remover) = (Instant::now(), shared.clone());
        let removed = on_a_thread(move || remover.remove_index());
        let (removed, waited) = (removed.recv_timeout(limit), started.elapsed());
        assert!(
            removed.is_ok() && waited < quick,
            "the removal waited {waited:?}"
        );
    }

    // A directory a full rescan's walk lists, renamed or removed before the loop takes its events,
    // leaves the published index: its paths, gone by then, are never dropped unread.
    fn a_directory_gone_during_a_rescan_leaves_the_index(kind: EventKind) {
        let (_tmp, root) = canonical_tempdir();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let frecency = start_unwatched(&shared, &root, NO_END);
        settle(&shared, &root);
        std::fs::create_dir(root.join("src/new")).unwrap();
        std::fs::write(root.join("src/new/a.rs"), "fn a() {}\n").unwrap();
        let (observed, moving, reporter) = (shared.clone(), root.clone(), frecency.clone());
        on(&root, Stage::Walked, move || {
            std::fs::rename(moving.join("src/new"), moving.join("src/moved")).unwrap();
            let event = |kind: EventKind, path: PathBuf| {
                DebouncedEvent::new(notify::Event::new(kind).add_path(path), Instant::now())
            };
            observed.feed(
                Some(reporter),
                vec![
                    event(
                        EventKind::Create(CreateKind::Folder),
                        moving.join("src/new"),
                    ),
                    event(kind, moving.join("src/new")),
                    event(kind, moving.join("src/moved")),
                ],
            );
        });
        shared.trigger_full_rescan_async(&frecency).unwrap();
        settle(&shared, &root);
        let files = live_files(&shared);
        assert!(files.contains(&"src/moved/a.rs".to_owned()), "{files:?}");
        assert!(!files.contains(&"src/new/a.rs".to_owned()), "{files:?}");
    }

    #[test]
    fn a_directory_renamed_during_a_rescan_leaves_the_index() {
        let renamed = notify::event::ModifyKind::Name(notify::event::RenameMode::Any);
        a_directory_gone_during_a_rescan_leaves_the_index(EventKind::Modify(renamed));
    }

    #[test]
    fn a_directory_removed_during_a_rescan_leaves_the_index() {
        let removed = notify::event::RemoveKind::Any;
        a_directory_gone_during_a_rescan_leaves_the_index(EventKind::Remove(removed));
    }

    // The loop's timer starts a rescan the watcher asked for; a panic there leaves the loop
    // running, rebuilds the index and lets later changes in.
    #[test]
    fn a_panic_in_a_rescan_the_timer_starts_leaves_the_loop_running() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let frecency = start_unwatched(&shared, &root, Duration::from_millis(1500));
        settle(&shared, &root);
        on(&root, Stage::Claimed, || {
            panic!("starting the timed rescan failed halfway")
        });
        let started = shared
            .trigger_full_rescan_with_reason(&frecency, RescanReason::OverflowCapacity)
            .unwrap();
        assert!(!started, "a watcher's rescan waits for its interval");
        eventually("the recovery rescan walks", || {
            times_reached(&root, Stage::Walked) == 2
        });
        assert!(shared.wait_for_scan(Duration::from_secs(10)));
        settle(&shared, &root);
        std::fs::write(root.join("later.rs"), "fn later() {}\n").unwrap();
        let (reporter, later) = (shared.clone(), vec![root.join("later.rs")]);
        let applied = on_a_thread(move || reporter.apply_reported_changes(&frecency, later));
        assert_eq!(
            applied.recv_timeout(Duration::from_secs(10)),
            Ok(crate::ReportedChanges::Applied)
        );
        assert!(live_files(&shared).contains(&"later.rs".to_owned()));
        assert_eq!(times_owed_alone(&root), 0);
    }

    // The walk of an index replaced at the same root registers a nested repository into its own
    // registry: the new index still watches that repository's git state.
    #[test]
    fn a_replaced_index_s_walk_registers_no_git_dir_into_its_successor() {
        let (_tmp, root) = canonical_tempdir();
        let inner = root.join("inner");
        std::fs::create_dir(&inner).unwrap();
        git_init(&inner);
        std::fs::write(inner.join("f.rs"), "fn f() {}\n").unwrap();
        crate::watch::watch_per_directory_for(&root);
        let (first, second) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let (hold_first, hold_second) = (first.clone(), second.clone());
        // The old walk waits until the new index is published; the new scan starts on its loop,
        // holding no pool thread a walk's allocator hint waits for, once the old walk is done.
        on(&root, Stage::Walking, move || {
            poll_for(Duration::from_secs(30), || {
                hold_first.load(Ordering::SeqCst)
            });
        });
        on(&root, Stage::Installing, || {});
        on(&root, Stage::Installing, move || {
            poll_for(Duration::from_secs(30), || {
                hold_second.load(Ordering::SeqCst)
            });
        });
        let shared = SharedFilePicker::default();
        let _old = start(&shared, &root, false);
        std::thread::sleep(Duration::from_millis(200));
        let _new = start(&shared, &root, false);
        first.store(true, Ordering::SeqCst);
        eventually("the old walk ends", || {
            times_reached(&root, Stage::Walked) == 1
        });
        second.store(true, Ordering::SeqCst);
        assert!(shared.wait_for_watcher(Duration::from_secs(10)));
        // The old index's scan never ends: its walk's result is dropped.
        eventually("the new scan settles", || {
            let pending = (shared.read().unwrap().as_ref())
                .is_some_and(|picker| picker.background_status().is_scan_pending());
            times_reached(&root, Stage::Walked) == 2
                && !pending
                && shared.wait_for_indexing_complete(Duration::ZERO)
        });
        let (sender, heard) = std::sync::mpsc::channel();
        let _state = shared.subscribe_repository_state(move |work_tree| {
            let _ = sender.send(work_tree.to_path_buf());
        });
        std::thread::sleep(Duration::from_millis(300));
        while heard.try_recv().is_ok() {}
        let add = std::process::Command::new("git")
            .args(["add", "f.rs"])
            .current_dir(&inner)
            .output()
            .unwrap();
        assert!(add.status.success());
        assert!(
            heard.recv_timeout(Duration::from_secs(5)).is_ok(),
            "the new index does not follow the nested repository's git state"
        );
    }

    // A FIFO among a moved-in repository's git files is never opened: the loop goes on.
    #[cfg(unix)]
    #[test]
    fn a_fifo_commondir_in_a_moved_in_repository_never_blocks_the_loop() {
        let (_tmp, root) = canonical_tempdir();
        let (_other_tmp, other) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        std::fs::write(other.join("b.rs"), "fn b() {}\n").unwrap();
        crate::watch::watch_per_directory_for(&root);
        let shared = SharedFilePicker::default();
        let frecency = start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(10)));
        settle(&shared, &root);
        let staging = tempfile::tempdir_in(root.parent().unwrap()).unwrap();
        let repo = staging.path().join("nested");
        std::fs::create_dir_all(&repo).unwrap();
        git_init(&repo);
        std::fs::write(repo.join("f.rs"), "fn f() {}\n").unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(repo.join(".git/commondir"))
            .status()
            .unwrap();
        assert!(made.success());
        std::fs::rename(&repo, root.join("nested")).unwrap();
        let (reporter, reported) = (shared.clone(), root.join("nested"));
        let answered =
            on_a_thread(move || reporter.apply_reported_changes(&frecency, vec![reported]));
        let applied = answered.recv_timeout(Duration::from_secs(5)).is_ok();
        let replacer = shared.clone();
        let replaced = on_a_thread(move || drop(start_unwatched(&replacer, &other, NO_END)));
        let replaced = replaced.recv_timeout(Duration::from_secs(5)).is_ok();
        // Unblocks a reader that opened it, so the test ends either way.
        use std::os::unix::fs::OpenOptionsExt;
        for _ in 0..40 {
            let mut writer = std::fs::OpenOptions::new();
            let fifo = root.join("nested/.git/commondir");
            drop(writer.write(true).custom_flags(libc::O_NONBLOCK).open(fifo));
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            applied && replaced,
            "a FIFO commondir blocked the index's loop"
        );
    }

    // A requested watcher that cannot be installed leaves the index unwatched, which its status
    // tells; scanning ends as usual.
    #[test]
    fn a_requested_watcher_that_cannot_be_installed_fails() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        refuse_watcher(&root);
        let shared = SharedFilePicker::default();
        start(&shared, &root, false);
        assert!(!shared.wait_for_watcher(Duration::from_secs(10)));
        settle(&shared, &root);
        let status = shared.read().unwrap().as_ref().unwrap().background_status();
        assert_eq!(status.watcher(), crate::WatcherStatus::Failed);
        assert_eq!(live_files(&shared), ["kept.rs"]);
    }

    // An index cancelled before its watcher is installed installs none: it is stopped, not
    // failed.
    #[test]
    fn cancelling_before_the_watcher_is_installed_stops_it() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let canceller = shared.weaken();
        on(&root, Stage::Installing, move || {
            if let Some(shared) = canceller.upgrade() {
                shared.cancel();
            }
        });
        start(&shared, &root, false);
        assert!(!shared.wait_for_watcher(Duration::from_secs(10)));
        let status = shared.read().unwrap().as_ref().unwrap().background_status();
        assert_eq!(status.watcher(), crate::WatcherStatus::Stopped);
        assert!(
            shared
                .read()
                .unwrap()
                .as_ref()
                .unwrap()
                .background_watcher
                .is_none()
        );
    }

    // Removing the index goes through its loop: it waits for the loop's work at hand, and the
    // handle takes a new index after it.
    #[test]
    fn removing_the_index_goes_through_its_loop() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        start_unwatched(&shared, &root, NO_END);
        settle(&shared, &root);
        let release = occupy_writer(&shared, &root);
        let remover = shared.clone();
        let removed = on_a_thread(move || remover.remove_index());
        assert!(removed.recv_timeout(Duration::from_millis(300)).is_err());
        assert!(
            shared.read().unwrap().is_some(),
            "removed while its loop was busy"
        );
        release.store(true, Ordering::Release);
        assert!(removed.recv_timeout(Duration::from_secs(10)).is_ok());
        assert!(shared.read().unwrap().is_none());
        start_unwatched(&shared, &root, NO_END);
        settle(&shared, &root);
        assert_eq!(live_files(&shared), ["kept.rs"]);
    }

    #[test]
    fn a_write_that_panics_leaves_the_writer_running_and_rescans_at_once() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let frecency = start_unwatched(&shared, &root, NO_END);
        settle(&shared, &root);
        // An explicit rescan's end starts an interval no test outlasts.
        shared.trigger_full_rescan_async(&frecency).unwrap();
        eventually("the rescan walks", || {
            times_reached(&root, Stage::Walked) == 2
        });
        settle(&shared, &root);
        let before = shared.rescan_stats();

        run_on_writer(&shared, |_| panic!("a write failed halfway"));
        eventually("the recovery rescan walks", || {
            times_reached(&root, Stage::Walked) == 3
        });
        settle(&shared, &root);
        #[cfg(rescan_stats)]
        assert_eq!(
            (shared.rescan_stats().since(&before)).count(RescanReason::IndexUpdateRejected),
            1
        );
        let _ = before;

        // The writer goes on with the next change.
        std::fs::write(root.join("later.rs"), "fn later() {}\n").unwrap();
        let (reporter, later) = (shared.clone(), vec![root.join("later.rs")]);
        let applied = on_a_thread(move || reporter.apply_reported_changes(&frecency, later));
        assert_eq!(
            applied.recv_timeout(Duration::from_secs(10)),
            Ok(crate::ReportedChanges::Applied)
        );
        assert!(live_files(&shared).contains(&"later.rs".to_owned()));
        assert_eq!(times_owed_alone(&root), 0);
    }

    #[test]
    fn the_index_s_writes_run_one_at_a_time_on_its_writer() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::create_dir(root.join("src")).unwrap();
        let shared = SharedFilePicker::default();
        let frecency = start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(10)));
        // Batches, reports and rescans arrive together from several threads.
        let writers: Vec<_> = (0..3)
            .map(|thread| {
                let (shared, root) = (shared.clone(), root.clone());
                on_a_thread(move || {
                    for i in 0..20 {
                        let path = root.join(format!("src/t{thread}_{i}.rs"));
                        let _ = std::fs::write(&path, "fn f() {}\n");
                        match i % 2 {
                            0 => deliver(&shared, created([path])),
                            _ => {
                                let frecency = SharedFrecency::default();
                                let _ = shared.apply_reported_changes(&frecency, vec![path]);
                            }
                        }
                    }
                })
            })
            .collect();
        for _ in 0..3 {
            shared.trigger_full_rescan_async(&frecency).unwrap();
        }
        for writer in writers {
            let done = writer.recv_timeout(Duration::from_secs(60));
            assert_eq!(done, Ok(()), "a writing thread did not finish");
        }
        settle(&shared, &root);
        // One batch at least reaches an idle index, whatever the rescans held.
        let last = root.join("src/last.rs");
        std::fs::write(&last, "fn f() {}\n").unwrap();
        deliver(&shared, created([last]));
        eventually("every change is indexed", || {
            live_files(&shared).len() == 61
        });
        assert_eq!(crate::watch::most_at_once(&root), 1);
        let committers = committing_threads(&root);
        assert!(!committers.is_empty());
        assert!(
            (committers.iter()).all(|thread| thread.as_deref() == Some("fff-index-writer")),
            "{committers:?}"
        );
    }

    #[test]
    fn a_replaced_index_s_pending_writes_never_touch_its_successor() {
        let (_tmp, root) = canonical_tempdir();
        let old_root = root.join("sub");
        std::fs::create_dir(&old_root).unwrap();
        std::fs::write(old_root.join("keep.rs"), "fn keep() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        start_unwatched(&shared, &old_root, NO_END);
        settle(&shared, &old_root);

        // A write for the old index is applied while the index is replaced: only the loop
        // replaces an index, so it applies the write to the old one first.
        let release = Arc::new(AtomicBool::new(false));
        let waiting = Arc::clone(&release);
        crate::watch::on_applying(&old_root, move || {
            poll_for(Duration::from_secs(5), || waiting.load(Ordering::Acquire));
        });
        deliver(&shared, removed(old_root.join("keep.rs")));
        let replacer = shared.clone();
        let new_root = root.clone();
        let replacing = on_a_thread(move || drop(start_unwatched(&replacer, &new_root, NO_END)));
        std::thread::sleep(Duration::from_millis(200));
        release.store(true, Ordering::Release);
        let replaced = replacing.recv_timeout(Duration::from_secs(30));
        assert_eq!(replaced, Ok(()), "the replacement did not finish");
        settle(&shared, &root);

        // The old index's removal of a file that exists would show in the new index.
        assert_eq!(live_files(&shared), ["sub/keep.rs"]);
    }

    #[test]
    fn a_watched_index_is_freed_once_dropped() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let frecency = start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(10)));
        std::fs::write(root.join("later.rs"), "fn later() {}\n").unwrap();
        let applied = shared.apply_reported_changes(&frecency, vec![root.join("later.rs")]);
        assert_eq!(applied, crate::ReportedChanges::Applied);
        settle(&shared, &root);
        drops_cleanly(shared);
    }

    #[test]
    fn a_change_reported_while_busy_is_applied_once_the_index_is_idle() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let frecency = start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(10)));
        settle(&shared, &root);

        // A program reports a file while a rescan walks.
        let (reporter, reported) = (shared.clone(), root.join("reported.rs"));
        let answer: Arc<Mutex<Option<crate::ReportedChanges>>> = Arc::default();
        let answered = Arc::clone(&answer);
        on(&root, Stage::Walked, move || {
            let _ = std::fs::write(&reported, "fn reported() {}\n");
            let report = on_a_thread(move || {
                reporter.apply_reported_changes(&SharedFrecency::default(), vec![reported])
            });
            *answered.lock().unwrap() = report.recv_timeout(Duration::from_secs(10)).ok();
        });
        shared.trigger_full_rescan_async(&frecency).unwrap();

        // As the service decides it is idle.
        let status = shared.read().unwrap().as_ref().unwrap().background_status();
        eventually("the rescan ran", || {
            times_reached(&root, Stage::Walked) == 2
        });
        eventually("the index is idle", || {
            !status.is_scanning() && !status.is_scan_pending()
        });
        assert_eq!(*answer.lock().unwrap(), Some(crate::ReportedChanges::Held));
        assert!(live_files(&shared).contains(&"reported.rs".to_owned()));
    }

    #[test]
    fn full_rescan_requests_during_a_scan_run_once_an_interval_after_it_ends() {
        const INTERVAL: Duration = Duration::from_millis(1500);
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let requester = shared.clone();
        let asked = Arc::new(AtomicUsize::new(0));
        let report = Arc::clone(&asked);
        // The watcher asks for a full rescan again and again while the first scan walks.
        on(&root, Stage::Walked, move || {
            for _ in 0..5 {
                let frecency = SharedFrecency::default();
                let reason = RescanReason::OverflowCapacity;
                if requester
                    .trigger_full_rescan_with_reason(&frecency, reason)
                    .is_ok()
                {
                    report.fetch_add(1, Ordering::AcqRel);
                }
            }
        });
        start_with_interval(&shared, &root, false, Some(INTERVAL));
        eventually("the requested rescan walks", || {
            times_reached(&root, Stage::Walked) == 2
        });
        settle(&shared, &root);
        // Nothing else is pending: no third scan follows.
        std::thread::sleep(INTERVAL + Duration::from_millis(500));
        assert_eq!(times_reached(&root, Stage::Walked), 2);
        assert_eq!(asked.load(Ordering::Acquire), 5);
        let ended = when_reached(&root, Stage::Finished)[0];
        let started = when_reached(&root, Stage::Walking)[1];
        assert!(
            started >= ended + INTERVAL,
            "the rescan started {:?} after the scan ended",
            started.saturating_duration_since(ended)
        );
        #[cfg(rescan_stats)]
        {
            let stats = shared.rescan_stats();
            assert_eq!(stats.count(RescanReason::OverflowCapacity), 1, "{stats}");
            assert_eq!(stats.total, 1, "{stats}");
        }
    }

    #[test]
    fn an_explicit_rescan_requested_during_a_scan_runs_right_after_it() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let requester = shared.clone();
        let reported = Arc::new(Mutex::new(None));
        let report = Arc::clone(&reported);
        let heard: Arc<Mutex<Vec<crate::WatchEventKind>>> = Arc::default();
        let hear = Arc::clone(&heard);
        on(&root, Stage::ContentIndexed, move || {
            let listen = move |_: crate::WatchId, events: &[crate::WatchEvent]| {
                hear.lock()
                    .unwrap()
                    .extend(events.iter().map(|event| event.kind));
            };
            let subscribed = requester.watch("", Default::default(), listen).is_ok();
            let frecency = SharedFrecency::default();
            let reason = RescanReason::OverflowCapacity;
            let watcher = requester.trigger_full_rescan_with_reason(&frecency, reason);
            let explicit = requester.trigger_full_rescan_async(&frecency);
            *report.lock().unwrap() = Some((subscribed, watcher.ok(), explicit.is_ok()));
        });

        // Only the explicit request may run before an interval no test outlasts.
        start_with_interval(&shared, &root, true, Some(NO_END));
        eventually("the explicit rescan walks", || {
            times_reached(&root, Stage::Walked) == 2
        });
        settle(&shared, &root);
        assert_eq!(*reported.lock().unwrap(), Some((true, Some(false), true)));
        // The watcher's request joined the explicit rescan: subscribers hear of it.
        shared.watch_registry().wait_for_dispatch();
        let heard = heard.lock().unwrap().clone();
        assert!(heard.contains(&crate::WatchEventKind::Rescan), "{heard:?}");
        let ended = when_reached(&root, Stage::Finished)[0];
        let started = when_reached(&root, Stage::Walking)[1];
        assert!(started.saturating_duration_since(ended) < Duration::from_secs(5));
        // The explicit rescan began after the watcher's request, so it covers it.
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(shared.next_rescan_due(), None);
        assert_eq!(times_reached(&root, Stage::Walked), 2);
    }

    #[test]
    fn an_index_without_a_watcher_starts_a_pending_rescan_once_due() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        start_unwatched(&shared, &root, Duration::from_millis(300));
        settle(&shared, &root);

        // More new files than the overflow area takes: only a rescan takes them all in.
        let reported = report_many_files(&shared, &root, "late");
        eventually("the pending rescan walks", || {
            times_reached(&root, Stage::Walked) == 2
        });
        settle(&shared, &root);
        assert_eq!(live_files_under(&shared, "late"), reported);
        drops_cleanly(shared);
    }

    // Changes made while an explicit rescan walks reach the index; subscribers hear of them by
    // one `rescan` once the rebuilt index is in place, never while it is being built.
    fn changes_during_an_explicit_rescan(nested: bool) {
        let (_tmp, root) = canonical_tempdir();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let frecency = start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(10)));
        settle(&shared, &root);
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let _watch = shared
            .watch("", Default::default(), move |_, batch| {
                sink.lock().unwrap().extend_from_slice(batch)
            })
            .unwrap();
        let target = if nested { "src/new/a.rs" } else { "src/a.rs" };
        let (changes, during) = (root.join(target), Arc::clone(&events));
        let heard_during_walk = Arc::new(AtomicBool::new(false));
        let heard = Arc::clone(&heard_during_walk);
        on(&root, Stage::Walking, move || {
            let _ = std::fs::create_dir_all(changes.parent().unwrap());
            let _ = std::fs::write(&changes, "fn a() {}\n");
            std::thread::sleep(Duration::from_millis(500));
            let rescan = (during.lock().unwrap().iter())
                .any(|event: &crate::watch::WatchEvent| event.kind == WatchEventKind::Rescan);
            heard.store(rescan, Ordering::SeqCst);
        });
        shared.trigger_full_rescan_async(&frecency).unwrap();
        settle(&shared, &root);
        eventually("indexed", || {
            live_files(&shared).contains(&target.to_string())
        });
        shared.watch_registry().wait_for_dispatch();
        assert!(
            !heard_during_walk.load(Ordering::SeqCst),
            "rescan heard before publication"
        );
        let rescans = (events.lock().unwrap().iter())
            .filter(|event| event.kind == WatchEventKind::Rescan)
            .count();
        assert_eq!(rescans, 1);
    }

    #[test]
    fn a_file_created_during_an_explicit_rescan_reaches_subscribers_with_its_rescan() {
        changes_during_an_explicit_rescan(false);
    }

    #[test]
    fn a_directory_created_during_an_explicit_rescan_reaches_subscribers_with_its_rescan() {
        changes_during_an_explicit_rescan(true);
    }

    // An index rooted below its work tree follows the repository's exclude file with either
    // walker, from the first scan and once it changes, without a full rescan.
    fn an_exclude_file_below_the_work_tree(per_dir: bool) {
        let (_tmp, repo) = canonical_tempdir();
        git_init(&repo);
        let root = repo.join("sub");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        std::fs::write(root.join("secret.rs"), "fn secret() {}\n").unwrap();
        if per_dir {
            crate::watch::watch_per_directory_for(&root);
        }
        let shared = SharedFilePicker::default();
        start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(10)));
        settle(&shared, &root);
        assert_eq!(live_files(&shared), ["kept.rs", "secret.rs"]);
        std::thread::sleep(Duration::from_millis(2500));
        std::fs::create_dir_all(repo.join(".git/info")).unwrap();
        std::fs::write(repo.join(".git/info/exclude"), "secret.rs\n").unwrap();
        eventually("the exclude change applies", || {
            live_files(&shared) == ["kept.rs"]
        });
        assert_eq!(times_reached(&root, Stage::Walked), 1, "a full rescan ran");
    }

    #[test]
    fn an_exclude_file_applies_to_an_index_below_its_work_tree() {
        an_exclude_file_below_the_work_tree(false);
    }

    #[test]
    fn an_exclude_file_applies_to_an_index_below_its_work_tree_watched_per_directory() {
        an_exclude_file_below_the_work_tree(true);
    }

    #[test]
    fn the_first_scan_below_a_work_tree_follows_its_exclude_file() {
        let (_tmp, repo) = canonical_tempdir();
        git_init(&repo);
        let root = repo.join("sub");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        std::fs::write(root.join("secret.rs"), "fn secret() {}\n").unwrap();
        std::fs::create_dir_all(repo.join(".git/info")).unwrap();
        std::fs::write(repo.join(".git/info/exclude"), "secret.rs\n").unwrap();
        let shared = SharedFilePicker::default();
        start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(10)));
        settle(&shared, &root);
        assert_eq!(live_files(&shared), ["kept.rs"]);
    }

    // A listener that holds the last handle, dropped while a notification for its set waits
    // behind a slow one: the listener is dropped outside the registration lock.
    #[test]
    fn dropping_a_subscription_that_holds_the_last_handle_never_deadlocks() {
        let (_tmp, root) = canonical_tempdir();
        git_init(&root);
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let _frecency = start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(10)));
        settle(&shared, &root);
        let slow_entered = Arc::new(AtomicBool::new(false));
        let entered = slow_entered.clone();
        let _slow = shared.subscribe_ref_changes(&root, move |_| {
            entered.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(500));
        });
        let captured = shared.clone();
        let subscription = shared.subscribe_repository_state(move |_| {
            let _ = captured.wait_for_watcher(Duration::ZERO);
        });
        shared.notify_ref_change(&root);
        shared.notify_repository_change(&root);
        eventually("the slow listener runs", || {
            slow_entered.load(Ordering::SeqCst)
        });
        drop(shared);
        let dropped = on_a_thread(move || drop(subscription));
        assert!(
            dropped.recv_timeout(Duration::from_secs(5)).is_ok(),
            "dropping a listener subscription deadlocked with the dispatcher"
        );
    }

    // The loop subscribes a new directory's watches inline while the debouncer waits for the
    // loop to take its next batch: the debouncer releases its lock before calling back.
    #[test]
    fn subscribing_a_new_directory_on_the_loop_never_deadlocks_with_the_debouncer() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        crate::watch::watch_per_directory_for(&root);
        let shared = SharedFilePicker::default();
        start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(10)));
        settle(&shared, &root);
        for round in 0..5 {
            // The loop holds the batch of the new directories while the watcher reports more.
            let busy = root.clone();
            crate::watch::on_applying(&root, move || {
                for i in 0..20 {
                    let _ = std::fs::write(busy.join(format!("busy{i}.rs")), "fn b() {}\n");
                }
                std::thread::sleep(Duration::from_millis(300));
            });
            for i in 0..10 {
                let dir = root.join(format!("r{round}/d{i}/e"));
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
            }
            eventually("the new directories are indexed", || {
                live_files_under(&shared, &format!("r{round}")) == 10
            });
            // Their watches are in place: a file written later in one is seen.
            let later = format!("r{round}/d3/e/later.rs");
            std::fs::write(root.join(&later), "fn later() {}\n").unwrap();
            eventually("a later file in a new directory is indexed", || {
                live_files(&shared).contains(&later)
            });
        }
        drops_cleanly(shared);
    }

    #[test]
    fn a_new_index_starts_with_none_of_the_rescan_state_of_the_one_it_replaced() {
        let (_old_tmp, old_root) = canonical_tempdir();
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(old_root.join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(root.join("b.rs"), "fn b() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let frecency = start_unwatched(&shared, &old_root, NO_END);
        settle(&shared, &old_root);
        // The old index has a rescan pending for an interval no test outlasts.
        let reason = RescanReason::OverflowCapacity;
        let started = shared.trigger_full_rescan_with_reason(&frecency, reason);
        assert!(!started.unwrap());
        assert!(shared.next_rescan_due().is_some());

        let frecency = start_unwatched(&shared, &root, Duration::from_millis(200));
        settle(&shared, &root);
        assert_eq!(shared.next_rescan_due(), None);
        // The new index counts its interval from its own first scan.
        assert!(
            shared
                .trigger_full_rescan_with_reason(&frecency, reason)
                .is_ok()
        );
        eventually("the new index rescans", || {
            times_reached(&root, Stage::Walked) == 2
        });
        settle(&shared, &root);
        assert_eq!(times_reached(&old_root, Stage::Walked), 1);
    }

    // Git creates `info` from its templates, but a template-less `git init` does not.
    #[test]
    fn an_exclude_file_created_after_the_watcher_started_applies_without_a_rescan() {
        let (_tmp, root) = canonical_tempdir();
        git_init(&root);
        let _ = std::fs::remove_dir_all(root.join(".git/info"));
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        std::fs::write(root.join("secret.rs"), "fn secret() {}\n").unwrap();
        // As on Linux: the git dirs inside the tree have watches of their own.
        crate::watch::watch_per_directory_for(&root);
        let shared = SharedFilePicker::default();
        start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(10)));
        settle(&shared, &root);
        assert_eq!(live_files(&shared), ["kept.rs", "secret.rs"]);

        std::fs::create_dir(root.join(".git/info")).unwrap();
        std::fs::write(root.join(".git/info/exclude"), "secret.rs\n").unwrap();
        eventually("the new exclude file applies", || {
            live_files(&shared) == ["kept.rs"]
        });
        std::fs::write(root.join(".git/info/exclude"), "kept.rs\n").unwrap();
        eventually("its change applies", || {
            live_files(&shared) == ["secret.rs"]
        });
        assert_eq!(times_reached(&root, Stage::Walked), 1, "a full rescan ran");
    }

    // Git invalidates the untracked cache of the repository whose exclude file changed.
    #[test]
    fn a_nested_repository_s_exclude_file_applies_to_its_work_tree_without_a_rescan() {
        let (_tmp, root) = canonical_tempdir();
        git_init(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        git_init(&root.join("sub"));
        std::fs::create_dir_all(root.join("sub/.git/info")).unwrap();
        std::fs::write(root.join("sub/.git/info/exclude"), "").unwrap();
        std::fs::write(root.join("sub/kept.rs"), "fn kept() {}\n").unwrap();
        std::fs::write(root.join("sub/secret.rs"), "fn secret() {}\n").unwrap();
        // As on Linux: the git dirs inside the tree have watches of their own.
        crate::watch::watch_per_directory_for(&root);
        let shared = SharedFilePicker::default();
        start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(10)));
        settle(&shared, &root);
        assert_eq!(live_files(&shared), ["sub/kept.rs", "sub/secret.rs"]);

        std::fs::write(root.join("sub/.git/info/exclude"), "secret.rs\n").unwrap();
        eventually("the nested exclude file applies", || {
            live_files(&shared) == ["sub/kept.rs"]
        });
        assert_eq!(times_reached(&root, Stage::Walked), 1, "a full rescan ran");
    }

    // Walks of new directories run on threads the writer waits for; listeners never do.
    #[test]
    fn a_listener_that_reports_changes_as_repositories_arrive_keeps_the_writer_live() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("kept.rs"), "fn kept() {}\n").unwrap();
        crate::watch::watch_per_directory_for(&root);
        let shared = SharedFilePicker::default();
        let frecency = start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(10)));
        settle(&shared, &root);
        let (reporter, reported) = (shared.weaken(), root.join("reported.rs"));
        std::fs::write(&reported, "fn reported() {}\n").unwrap();
        let answers: Arc<Mutex<Vec<crate::ReportedChanges>>> = Arc::default();
        let answered = Arc::clone(&answers);
        let reporter_frecency = frecency.clone();
        let _state = shared.subscribe_repository_state(move |_| {
            if let Some(picker) = reporter.upgrade() {
                let paths = vec![reported.clone()];
                let answer = picker.apply_reported_changes(&reporter_frecency, paths);
                answered.lock().unwrap().push(answer);
            }
        });
        let staging = tempfile::tempdir_in(root.parent().unwrap()).unwrap();
        for i in 0..8 {
            let repo = staging.path().join(format!("a{i}/sub"));
            std::fs::create_dir_all(&repo).unwrap();
            git_init(&repo);
            std::fs::write(repo.join("f.rs"), "fn f() {}\n").unwrap();
        }
        std::fs::rename(staging.path(), root.join("nested")).unwrap();
        eventually("the listener hears of the repositories", || {
            answers.lock().unwrap().len() >= 8
        });

        std::fs::write(root.join("later.rs"), "fn later() {}\n").unwrap();
        let (reporter, later) = (shared.clone(), vec![root.join("later.rs")]);
        let answer = on_a_thread(move || reporter.apply_reported_changes(&frecency, later));
        let answer = answer.recv_timeout(Duration::from_secs(10));
        assert_eq!(answer.ok(), Some(crate::ReportedChanges::Applied));
        assert!(live_files(&shared).contains(&"reported.rs".to_owned()));
    }

    // Without a ref listener, the main repository's git dir is watched until `info` appears.
    #[test]
    fn a_linked_worktree_s_exclude_file_created_later_applies_without_a_ref_listener() {
        let (_main_tmp, main) = canonical_tempdir();
        let (_tmp, parent) = canonical_tempdir();
        git_init(&main);
        std::fs::write(main.join("a.rs"), "fn a() {}\n").unwrap();
        git(&main, &["add", "."]);
        git(&main, &["commit", "-m", "first"]);
        let root = parent.join("wt");
        git(&main, &["worktree", "add", root.to_str().unwrap()]);
        let _ = std::fs::remove_dir_all(main.join(".git/info"));
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/guide.md"), "guide\n").unwrap();
        crate::watch::watch_per_directory_for(&root);
        let shared = SharedFilePicker::default();
        start(&shared, &root, false);
        assert!(shared.wait_for_watcher(Duration::from_secs(10)));
        settle(&shared, &root);
        assert!(live_files(&shared).contains(&"docs/guide.md".to_owned()));

        std::fs::create_dir(main.join(".git/info")).unwrap();
        std::fs::write(main.join(".git/info/exclude"), "docs/\n").unwrap();
        eventually("the exclude rule applies", || {
            !live_files(&shared).contains(&"docs/guide.md".to_owned())
        });
        assert_eq!(times_reached(&root, Stage::Walked), 1, "a full rescan ran");
    }

    #[test]
    fn replacing_an_index_while_its_rescan_starts_never_deadlocks() {
        let (_old_tmp, old_root) = canonical_tempdir();
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(old_root.join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(root.join("b.rs"), "fn b() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let frecency = start_unwatched(&shared, &old_root, NO_END);
        settle(&shared, &old_root);
        let (requester, requested) = (shared.clone(), frecency.clone());
        std::thread::spawn(move || requester.trigger_full_rescan_async(&requested));
        std::thread::sleep(Duration::from_millis(100));
        let replacer = shared.clone();
        let replaced = on_a_thread(move || drop(start_unwatched(&replacer, &root, NO_END)));
        assert!(
            replaced.recv_timeout(Duration::from_secs(20)).is_ok(),
            "replacing the index deadlocked against its starting rescan"
        );
    }

    #[test]
    fn cancelling_an_index_while_its_rescan_starts_never_deadlocks() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let frecency = start_unwatched(&shared, &root, NO_END);
        settle(&shared, &root);
        let (requester, requested) = (shared.clone(), frecency.clone());
        std::thread::spawn(move || requester.trigger_full_rescan_async(&requested));
        std::thread::sleep(Duration::from_millis(100));
        // A program cancels under the read lock while another waits for the write lock.
        let canceller = shared.clone();
        let cancelled = on_a_thread(move || canceller.cancel());
        std::thread::sleep(Duration::from_millis(50));
        let writer = shared.clone();
        let written = on_a_thread(move || drop(writer.write().unwrap()));
        for finished in [cancelled, written] {
            assert!(
                finished.recv_timeout(Duration::from_secs(10)).is_ok(),
                "cancelling deadlocked against a starting rescan and a waiting writer"
            );
        }
    }

    #[test]
    fn a_pending_rescan_whose_interval_grew_starts_once_the_longer_one_ends() {
        let (_tmp, root) = canonical_tempdir();
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        let shared = SharedFilePicker::default();
        let frecency = start_unwatched(&shared, &root, Duration::from_millis(500));
        settle(&shared, &root);
        let reason = RescanReason::OverflowCapacity;
        let started = shared.trigger_full_rescan_with_reason(&frecency, reason);
        assert!(!started.unwrap());
        // As for a non-git index that grew past the large-index size: the writer, already
        // waiting for the shorter interval, wakes too early.
        std::thread::sleep(Duration::from_millis(150));
        let mut guard = shared.write().unwrap();
        guard
            .as_mut()
            .unwrap()
            .set_rescan_cooldown(Duration::from_millis(1500));
        drop(guard);
        eventually("the rescan starts once the longer interval ends", || {
            times_reached(&root, Stage::Walked) == 2
        });
        settle(&shared, &root);
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
            ignore_options: Default::default(),
            enable_fs_root_scanning: false,
            enable_home_dir_scanning: false,
        };
        run_post_scan(
            &shared,
            signals.index_key(),
            0,
            &signals,
            &config,
            &snapshot,
            &[],
        );
        drop(snapshot);
        drop(old);
        // The loop has taken the content index once it answers.
        let _ = shared.next_rescan_due();

        let guard = shared.read().unwrap();
        let picker = guard.as_ref().unwrap();
        let query = crate::grep::parse_grep_query("isolated_marker");
        let options = crate::grep::GrepSearchOptions {
            time_budget_ms: 0,
            ..Default::default()
        };
        assert_eq!(picker.grep(&query, &options).matches.len(), 1);
    }

    // Adds a worktree of `main` at `later`, heard by `root`'s repository listeners. Natively, once
    // the new streams deliver, it is reported, as their delivery may lag minutes, and heard in 10 s.
    fn heard_later(
        shared: &SharedFilePicker,
        heard: &Mutex<Vec<PathBuf>>,
        root: &Path,
        main: &Path,
        later: &Path,
        native: bool,
    ) {
        let told = || heard.lock().unwrap().iter().any(|known| known == later);
        #[cfg(target_os = "macos")]
        if native {
            let _control = crate::watch::until_delivering(root);
            add_worktree(main, later);
            let reported = Instant::now();
            let answer =
                shared.apply_reported_changes(&SharedFrecency::default(), vec![later.into()]);
            assert_eq!(answer, crate::ReportedChanges::Applied);
            let limit = Duration::from_secs(10);
            let heard = crate::watch::within_product_time(root, reported, limit, 1, told);
            assert!(heard, "not heard within 10 s of the report");
            return;
        }
        let _ = (shared, root, native);
        add_worktree(main, later);
        eventually("a worktree added later is heard", told);
    }

    // Runs `call` on a thread of its own, so that a test bounds its wait for the answer.
    pub(crate) fn on_a_thread<T: Send + 'static>(
        call: impl FnOnce() -> T + Send + 'static,
    ) -> std::sync::mpsc::Receiver<T> {
        let (sender, answer) = std::sync::mpsc::channel();
        std::thread::spawn(move || drop(sender.send(call())));
        answer
    }
}
