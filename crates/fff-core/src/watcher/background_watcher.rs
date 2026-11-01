use super::dir_walk::{DirWalk, ReplacedDirs};
use super::ref_watches::{RefChanges, RefWatches};
use crate::constants::MAX_OVERFLOW_FILES;
use crate::error::Error;
use crate::file_picker::{FFFMode, FilePicker, IndexUpdateError};
use crate::git::GitDiscovery;
use crate::git_status_worker::GitStatusWorker;
use crate::rescan_stats::RescanReason;
use crate::shared::{RescanRequest, SharedFilePicker, SharedFrecency};
use crate::sort_buffer::sort_with_buffer;
use crate::walk::DirHook;
use crate::watch::{RawWatchEvent, WatchEventKind};
use git2::Repository;
use notify::event::{AccessKind, AccessMode, ModifyKind, RemoveKind, RenameMode};
use notify::{Config, EventKind, EventKindMask, PathOp, RecursiveMode, Watcher, WatcherKind};
use notify_debouncer_full::{DebounceEventResult, DebouncedEvent, NoCache, new_debouncer_opt};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tracing::{Level, debug, error, info, warn};

// The OS-native backend; test and debug builds can poll instead (see `test_poll_interval`).
enum Debouncer {
    Native(notify_debouncer_full::Debouncer<notify::RecommendedWatcher, NoCache>),
    Poll(notify_debouncer_full::Debouncer<notify::PollWatcher, NoCache>),
}

impl Debouncer {
    fn watch(&mut self, path: &Path, mode: RecursiveMode) -> notify::Result<()> {
        match self {
            Debouncer::Native(debouncer) => debouncer.watch(path, mode),
            Debouncer::Poll(debouncer) => debouncer.watch(path, mode),
        }
    }

    // Applies every op in one update, going on past a failed one, and returns whether the
    // event stream restarted for it. FSEvents restarts its stream on every update, also for a
    // path it watches already, and the new stream starts from now.
    fn update(&mut self, mut ops: Vec<PathOp>) -> bool {
        let restarts = self.restarts_stream();
        if restarts && let Ok(watched) = self.watched_paths() {
            ops.retain(|op| match op {
                PathOp::Watch(path, config) => !watched
                    .iter()
                    .any(|(known, mode)| known == path && *mode == config.recursive_mode()),
                PathOp::Unwatch(_) => true,
            });
        }
        let restarted = restarts && !ops.is_empty();
        while !ops.is_empty() {
            let result = match self {
                Debouncer::Native(debouncer) => debouncer.update_paths(ops),
                Debouncer::Poll(debouncer) => debouncer.update_paths(ops),
            };
            let Err(error) = result else {
                break;
            };
            match &error.origin {
                Some(PathOp::Watch(path, _)) => {
                    warn!("Failed to watch {}: {}", path.display(), error.source)
                }
                // Its directory went, and the backend dropped the watch with it.
                Some(PathOp::Unwatch(path)) => {
                    debug!("Failed to unwatch {}: {}", path.display(), error.source)
                }
                None => warn!("Failed to update the watched paths: {}", error.source),
            }
            ops = error.remaining;
        }
        restarted
    }

    // FSEvents restarts its one stream for every change of the watched paths.
    fn restarts_stream(&self) -> bool {
        matches!(self, Debouncer::Native(_))
            && <notify::RecommendedWatcher as Watcher>::kind() == WatcherKind::Fsevent
    }

    fn watched_paths(&self) -> notify::Result<Vec<(PathBuf, RecursiveMode)>> {
        match self {
            Debouncer::Native(debouncer) => debouncer.watched_paths(),
            Debouncer::Poll(debouncer) => debouncer.watched_paths(),
        }
    }

    fn stop_nonblocking(self) {
        match self {
            Debouncer::Native(debouncer) => debouncer.stop_nonblocking(),
            Debouncer::Poll(debouncer) => debouncer.stop_nonblocking(),
        }
    }
}

/// Owns the file-system watcher and guarantees that all background threads
/// are fully joined before `stop()` / `Drop` returns.
pub struct BackgroundWatcher {
    watches: Arc<Watches>,
    owner_thread: Option<std::thread::JoinHandle<()>>,
    recursive: bool,
    picker: crate::shared::WeakFilePicker,
    base_path: PathBuf,
}

// What the watcher's threads share to change the watched paths.
pub(crate) struct Watches {
    streams: Mutex<Option<Streams>>,
    // Closed when the watcher stops, so the owner thread ends.
    tasks: Mutex<Option<mpsc::Sender<WatchTask>>>,
    refs: RefWatches,
    git_status: Arc<GitStatusWorker>,
    base_path: PathBuf,
    // Set when the index's first walk begins; what a stream misses before, the walk reads.
    walking: AtomicBool,
}

// The tree's stream, and on FSEvents one for the paths outside the tree. FSEvents restarts a
// stream from "now" whenever its paths change, so every change after the setup goes to the
// outside one, whose restart can lose only git state, never a file of the tree.
struct Streams {
    tree: Debouncer,
    outside: Option<Debouncer>,
}

impl Streams {
    fn stop_nonblocking(self) {
        self.tree.stop_nonblocking();
        if let Some(outside) = self.outside {
            outside.stop_nonblocking();
        }
    }
}

impl Watches {
    fn send(&self, task: WatchTask) -> bool {
        (self.tasks.lock().as_ref()).is_some_and(|tasks| tasks.send(task).is_ok())
    }

    // Applies `ops` in one update and returns whether the git state of the repositories
    // outside the tree must be announced again, as a stream restarted after the walk began.
    fn update(&self, streams: &mut Streams, ops: Vec<PathOp>) -> bool {
        match streams.outside.as_mut() {
            Some(outside) => outside.update(ops) && self.walking.load(Ordering::Acquire),
            None => streams.tree.update(ops) && self.walking.load(Ordering::Acquire),
        }
    }

    // What changed outside the tree while its stream restarted is lost: the own git status is
    // read again, ref stores and logs that appeared get their watches, and the repositories
    // whose git dir or common dir lies outside the tree are announced as changed.
    fn recover_outside(&self, picker: &SharedFilePicker) {
        let outside = |path: &Path| !path.starts_with(&self.base_path);
        let registered = picker.registered_git_dirs();
        let mut appeared = Vec::new();
        // Each update adds only paths not watched yet, so the candidates bound the rounds.
        let mut rounds = 0;
        let mut bound = None;
        loop {
            let mut guard = self.streams.lock();
            let Some(streams) = guard.as_mut() else {
                return;
            };
            let plan = self.refs.plan(picker, None);
            let git_dirs = (registered.iter())
                .filter(|known| known.work_tree.starts_with(&self.base_path))
                .map(|known| known.git_dir.clone())
                .chain(self.refs.own_git_dir());
            let logs: Vec<_> = (git_dirs.filter(|git_dir| outside(git_dir)))
                .map(|git_dir| git_dir.join("logs"))
                .collect();
            let bound = *bound.get_or_insert(plan.candidates + logs.len() + 1);
            let logs = (logs.into_iter())
                .filter(|logs| logs.is_dir())
                .map(PathOp::watch_non_recursive);
            let ops = plan.ops.into_iter().chain(logs).collect();
            appeared.extend(plan.appeared);
            rounds += 1;
            if !self.update(streams, ops) {
                break;
            }
            if rounds >= bound {
                warn!(
                    rounds,
                    "Watches outside the tree kept changing while their stream restarted"
                );
                break;
            }
        }
        // Read after the last restart, so the status cannot predate what it missed.
        if self.refs.own_git_dir_outside() {
            self.git_status.request_full_rescan();
        }
        let mut work_trees: Vec<PathBuf> = (registered.into_iter())
            .filter(|known| {
                known.work_tree.starts_with(&self.base_path)
                    && (outside(&known.git_dir) || outside(&known.common_dir))
            })
            .map(|known| known.work_tree.components().collect())
            .collect();
        work_trees.extend(self.refs.own_work_tree_outside());
        work_trees.sort_unstable();
        work_trees.dedup();
        for work_tree in &work_trees {
            picker.notify_repository_change(work_tree);
        }
        appeared.extend(work_trees);
        appeared.sort_unstable();
        appeared.dedup();
        for work_tree in appeared {
            picker.notify_ref_change(&work_tree);
        }
    }

    // Brings the ref watches in line with the ref listeners.
    pub(crate) fn sync_refs(&self, picker: &SharedFilePicker) {
        let (appeared, restarted) = {
            let mut guard = self.streams.lock();
            let Some(streams) = guard.as_mut() else {
                return;
            };
            let plan = self.refs.plan(picker, None);
            (plan.appeared, self.update(streams, plan.ops))
        };
        if restarted {
            self.recover_outside(picker);
        }
        for work_tree in appeared {
            picker.notify_ref_change(&work_tree);
        }
    }
}

enum WatchTask {
    /// This is requires a separate walk of the new directory copies or created within a scan
    /// window because it might contain subdirectories we have to walk, prune, and add to index
    IndexNewDir(PathBuf),
    /// A `.git` appeared in this directory, which the walk had already reached.
    NewRepository(PathBuf),
    /// This git dir gained its `logs`.
    NewGitLog(PathBuf),
    /// A throttled rescan was deferred; wake up to run it when due.
    RescanDeferred,
    /// A ref store or its format may have changed: the ref watches follow.
    SyncRefs,
}

const DEBOUNCE_TIMEOUT: Duration = Duration::from_millis(50);
/// More affected paths than this in one batch are recovered by a full rescan.
pub(crate) const MAX_BATCH_PATHS: usize = MAX_OVERFLOW_FILES * 4;
// Per-directory subscriptions stop after this many failures in a row.
const MAX_CONSECUTIVE_WATCH_FAILURES: usize = 16;
/// Minimum seconds between frecency tracks of the same file in AI mode.
/// Prevents score inflation from rapid burst edits by AI agents.
const AI_MODE_COOLDOWN_SECS: u64 = 5 * 60;

impl BackgroundWatcher {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        base_path: PathBuf,
        git: Option<GitDiscovery>,
        shared_picker: SharedFilePicker,
        shared_frecency: SharedFrecency,
        mode: FFFMode,
        enable_fs_root_scanning: bool,
        enable_home_dir_scanning: bool,
        git_status_worker: Arc<GitStatusWorker>,
        trace_span: tracing::Span,
    ) -> Result<Self, Error> {
        let git_workdir = git.map(|git| git.workdir);
        info!(
            "Initializing background watcher for path: {}, mode: {:?}",
            base_path.display(),
            mode,
        );

        // Root and home watching are error prone; consumers must opt in explicitly.
        crate::path_utils::validate_base_path(
            &base_path,
            enable_fs_root_scanning,
            enable_home_dir_scanning,
        )?;

        // macOS: always use a single recursive FSEvent stream.
        // Per-dir NonRecursive watches create one FSEvent stream per dir.
        // The per-process FSEvent cap is lower than expected in practice
        // (4096 per process, but FFF usually is running within code editors),
        // and each failed `watch()` after the cap blocks ~40 ms on kernel retry.
        // Yes we pay for filtering events on handler phase but it is usable
        //
        // Windows doesn't seem to have a hard cap, but in practice non recursive watching
        // does a way worse job and often looses events which is not an option for us.
        //
        // Linux keeps the per-dir NonRecursive strategy: inotify has no
        // kernel-level watcher recursion, so we have to manually watch every single interested
        // directory for watch events which is in practice stable and fast if system has enough
        // spare watcher (configurable by the user, usually 100k - 1m)
        let use_recursive = cfg!(any(target_os = "macos", target_os = "windows"));

        let (watch_tx, watch_rx) = mpsc::channel::<WatchTask>();
        let watch_tx_for_debouncer = watch_tx.clone();
        let poll_interval = test_poll_interval(&base_path);
        let fsevents = poll_interval.is_none()
            && <notify::RecommendedWatcher as Watcher>::kind() == WatcherKind::Fsevent;
        // Removing a watch restarts the FSEvents stream, so ref watches stay once added there.
        let refs = RefWatches::new(&base_path, use_recursive, git_workdir.as_deref(), fsevents);
        // The own git dir is watched from the start, so a walk reaching the work tree finds it
        // known, and watches it anew only once it was created again.
        if let Some(workdir) = &git_workdir
            && let Some(git_dir) = git_dir_of(workdir)
        {
            shared_picker.register_git_dir(&git_dir, workdir);
        }

        let owner_weak_picker = shared_picker.weaken();
        let hook_picker = shared_picker.weaken();
        let hook_base_path = base_path.clone();
        let owner_git_workdir = git_workdir.clone();
        let owner_git_worker = Arc::clone(&git_status_worker);
        let owner_frecency = shared_frecency.clone();
        let owner_base_path = base_path.clone();

        let handler = || {
            (
                base_path.clone(),
                git_workdir.clone(),
                shared_picker.clone(),
                shared_frecency.clone(),
                watch_tx_for_debouncer.clone(),
                Arc::clone(&git_status_worker),
            )
        };
        let mut tree = match poll_interval {
            None => {
                let (base, workdir, picker, frecency, tx, worker) = handler();
                Debouncer::Native(Self::create_debouncer(
                    base, workdir, picker, frecency, mode, tx, worker, None,
                )?)
            }
            Some(interval) => {
                info!(?interval, "File watcher uses the polling backend");
                let (base, workdir, picker, frecency, tx, worker) = handler();
                Debouncer::Poll(Self::create_debouncer(
                    base,
                    workdir,
                    picker,
                    frecency,
                    mode,
                    tx,
                    worker,
                    Some(interval),
                )?)
            }
        };
        let mut outside = match fsevents {
            true => {
                let (base, workdir, picker, frecency, tx, worker) = handler();
                Some(Debouncer::Native(Self::create_debouncer(
                    base, workdir, picker, frecency, mode, tx, worker, None,
                )?))
            }
            false => None,
        };

        if use_recursive {
            tree.watch(&base_path, RecursiveMode::Recursive)?;
            info!(
                "File watcher initialized with single recursive watch on {}",
                base_path.display(),
            );
        } else {
            // Scans subscribe every directory they walk through `dir_hook`.
            tree.watch(&base_path, RecursiveMode::NonRecursive)?;
            info!(
                "File watcher initialized with per-directory watches on {}",
                base_path.display(),
            );
        }
        // The .git directory is excluded from the file list but we still need to observe
        // changes that affect git status (staging, committing, branch switches, merges, etc).
        // The own git dir may lie outside the tree, as a linked worktree's does.
        let own_git = git_workdir
            .as_deref()
            .and_then(|workdir| Repository::open(workdir).ok())
            .map(|repo| git_dir_watches(repo.path()).collect::<Vec<_>>())
            .unwrap_or_default();
        match outside.as_mut() {
            Some(outside) => {
                let (inside, mut beyond): (Vec<_>, Vec<_>) =
                    (own_git.into_iter()).partition(|op| op.as_path().starts_with(&base_path));
                tree.update(inside);
                beyond.extend(refs.own_targets(&shared_picker));
                outside.update(beyond);
            }
            None => {
                tree.update(own_git);
            }
        }

        info!("Background file watcher initialized successfully");

        let watches = Arc::new(Watches {
            streams: Mutex::new(Some(Streams { tree, outside })),
            tasks: Mutex::new(Some(watch_tx)),
            refs,
            git_status: Arc::clone(&owner_git_worker),
            base_path: base_path.clone(),
            walking: AtomicBool::new(false),
        });
        let owner_watches = Arc::clone(&watches);

        let owner_span = trace_span.clone();
        let owner_thread = std::thread::Builder::new()
            .name("fff-watcher-own".into())
            .spawn(move || {
                let _g = owner_span.enter();
                // Never hold the picker alive while blocked on the channel.
                while let Some(due) = owner_weak_picker
                    .upgrade()
                    .map(|picker| picker.deferred_rescan_due())
                {
                    let task = match due {
                        Some(due) => match watch_rx
                            .recv_timeout(due.saturating_duration_since(Instant::now()))
                        {
                            Ok(task) => Some(task),
                            Err(mpsc::RecvTimeoutError::Timeout) => None,
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        },
                        None => match watch_rx.recv() {
                            Ok(task) => Some(task),
                            Err(_) => break,
                        },
                    };
                    // if the picker is dropped we do need to exit the loop
                    let Some(strong_picker) = owner_weak_picker.upgrade() else {
                        break;
                    };

                    if let Some(reason) = strong_picker.due_deferred_rescan() {
                        request_rescan(&strong_picker, &owner_frecency, &owner_base_path, reason);
                    }
                    let dir = match task {
                        Some(WatchTask::IndexNewDir(dir)) => dir,
                        Some(WatchTask::NewGitLog(git_dir)) => {
                            watch_new_log(
                                &owner_watches,
                                &strong_picker,
                                &git_dir,
                                &owner_git_workdir,
                                use_recursive && git_dir.starts_with(&owner_base_path),
                            );
                            continue;
                        }
                        Some(WatchTask::NewRepository(work_tree)) => {
                            DirSubscriptions::hook(
                                &owner_watches,
                                use_recursive,
                                &owner_weak_picker,
                                &owner_base_path,
                            )(&work_tree);
                            continue;
                        }
                        Some(WatchTask::SyncRefs) => {
                            owner_watches.refs.reread_storages();
                            owner_watches.sync_refs(&strong_picker);
                            continue;
                        }
                        Some(WatchTask::RescanDeferred) | None => continue,
                    };
                    #[cfg(debug_assertions)]
                    hold_dir_indexing();
                    if owner_watches.streams.lock().is_none() {
                        break;
                    }

                    let dir_hook = DirSubscriptions::hook(
                        &owner_watches,
                        use_recursive,
                        &owner_weak_picker,
                        &owner_base_path,
                    );
                    // Subscribe BEFORE walking so files created mid-walk are still reported.
                    dir_hook(&dir);
                    index_new_directory(
                        &dir,
                        &strong_picker,
                        &owner_frecency,
                        &owner_git_workdir,
                        &owner_git_worker,
                        Some(&dir_hook),
                    );

                    drop(strong_picker);
                }

                tracing::info!("Background watcher is stopped");
            })
            .expect("failed to spawn fff-watcher-owner thread");

        Ok(Self {
            watches,
            owner_thread: Some(owner_thread),
            recursive: use_recursive,
            picker: hook_picker,
            base_path: hook_base_path,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn create_debouncer<W: notify::Watcher>(
        base_path: PathBuf,
        git_workdir: Option<PathBuf>,
        shared_picker: SharedFilePicker,
        shared_frecency: SharedFrecency,
        mode: FFFMode,
        watch_tx: mpsc::Sender<WatchTask>,
        git_status_worker: Arc<GitStatusWorker>,
        poll_interval: Option<Duration>,
    ) -> Result<notify_debouncer_full::Debouncer<W, NoCache>, Error> {
        let config = poll_interval
            .map_or_else(Config::default, |interval| {
                Config::default().with_poll_interval(interval)
            })
            .with_follow_symlinks(false)
            // only the actual modification events, ignore the open syscals that we can generate by
            // our own grep calls and preview window rendering
            .with_event_kinds(EventKindMask::CORE);

        let git_workdir_for_handler = git_workdir.clone();
        let base_path_for_handler = base_path.clone();
        let file_picker = shared_picker.weaken();
        let debouncer = new_debouncer_opt(
            DEBOUNCE_TIMEOUT,
            Some(DEBOUNCE_TIMEOUT / 2), // tick rate for the event span
            {
                let mut announced_due = None;
                move |result: DebounceEventResult| match result {
                    Ok(events) => {
                        let Some(file_picker) = file_picker.upgrade() else {
                            return;
                        };
                        // A scan replaying held events handles them after its commit.
                        let Some(events) = file_picker.event_hold().defer(events) else {
                            return;
                        };

                        let new_dirs = handle_debounced_events(
                            mode,
                            events,
                            &base_path_for_handler,
                            &git_workdir_for_handler,
                            &file_picker,
                            &shared_frecency,
                            &git_status_worker,
                        );

                        // every new directory created has to be reflected in the picker state
                        for dir in new_dirs {
                            if let Err(e) = watch_tx.send(WatchTask::IndexNewDir(dir)) {
                                error!(?e, "Failed to send directory update error");
                            }
                        }

                        // The owner thread runs deferred rescans; wake it for a new deadline.
                        let due = file_picker.deferred_rescan_due();
                        if due.is_some()
                            && due != announced_due
                            && let Err(e) = watch_tx.send(WatchTask::RescanDeferred)
                        {
                            error!(?e, "Failed to schedule a deferred rescan");
                        }
                        announced_due = due;
                    }
                    Err(errors) => {
                        error!("File watcher errors: {:?}", errors);
                    }
                }
            },
            // There is an issue with recommended cache implementation on macos
            // it keeps track of all the files added to the watcher which is not a problem
            // for us because any rename to the file will anyway require the removing from the
            // ordedred index and adding it back with the new name
            NoCache::new(),
            config,
        )?;

        Ok(debouncer)
    }

    /// Signal the watcher to shut down without blocking on its worker
    /// threads. Safe to call from any context, including while holding
    /// the [`SharedFilePicker`] write lock.
    pub fn stop(&mut self) {
        self.watches.tasks.lock().take();
        if let Some(streams) = self.watches.streams.lock().take() {
            streams.stop_nonblocking();
        }

        self.owner_thread.take();

        info!("Background file watcher stop signaled");
    }

    /// Runs for each directory a walk reaches: subscribes it unless one recursive watch
    /// covers the tree, and watches the git state of a repository it holds.
    pub(crate) fn dir_hook(&self) -> Option<DirHook> {
        Some(DirSubscriptions::hook(
            &self.watches,
            self.recursive,
            &self.picker,
            &self.base_path,
        ))
    }

    // From now on a stream that restarts misses what the walk cannot read for it.
    pub(crate) fn walk_begins(&self) {
        self.watches.walking.store(true, Ordering::Release);
    }

    // Changes the watched paths off the picker lock, as the ref listeners need.
    pub(crate) fn watches(&self) -> Arc<Watches> {
        Arc::clone(&self.watches)
    }

    pub(crate) fn request_index_dir(&self, dir: PathBuf) -> bool {
        self.send(WatchTask::IndexNewDir(dir))
    }

    fn request_git_state(&self, change: GitStateChange) -> bool {
        self.send(match change {
            GitStateChange::Repository(work_tree) => WatchTask::NewRepository(work_tree),
            GitStateChange::Log(git_dir) => WatchTask::NewGitLog(git_dir),
        })
    }

    // The owner thread runs deferred rescans once due.
    pub(crate) fn wake_for_deferred_rescan(&self) -> bool {
        self.send(WatchTask::RescanDeferred)
    }

    fn send(&self, task: WatchTask) -> bool {
        self.watches.send(task)
    }
}

impl Drop for BackgroundWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

// Applies changes to `paths` reported by a program exactly as watcher events for them: a
// path that exists is added or updated unless the index ignores it, one that does not is
// removed, with any indexed files below it. While a scan runs, they wait with the scan's
// held events, as watcher events do. Returns false when they were held.
pub(crate) fn apply_reported_changes(
    shared_picker: &SharedFilePicker,
    shared_frecency: &SharedFrecency,
    paths: Vec<PathBuf>,
) -> bool {
    let Some((mode, base_path, git_workdir, git_status_worker, follow_symlinks)) =
        shared_picker.read().ok().and_then(|guard| {
            guard.as_ref().map(|picker| {
                (
                    picker.mode(),
                    picker.base_path().to_path_buf(),
                    picker.git_root().map(Path::to_path_buf),
                    Arc::clone(&picker.git_status_worker),
                    picker.follows_symlinks(),
                )
            })
        })
    else {
        return true;
    };
    let now = std::time::Instant::now();
    let events = paths
        .into_iter()
        .map(|path| {
            // An index that does not follow symlinks holds none, as its scan skips them: a
            // reported symlink, also one that replaced a file or directory, is a removal.
            let skipped_link = !follow_symlinks
                && std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink());
            let kind = if skipped_link {
                EventKind::Remove(notify::event::RemoveKind::Any)
            } else {
                EventKind::Modify(ModifyKind::Any)
            };
            DebouncedEvent::new(notify::Event::new(kind).add_path(path), now)
        })
        .collect();
    let Some(events) = shared_picker.event_hold().defer(events) else {
        return false;
    };
    let new_dirs = handle_debounced_events(
        mode,
        events,
        &base_path,
        &git_workdir,
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
    true
}

pub(crate) fn handle_debounced_events(
    mode: FFFMode,
    events: Vec<DebouncedEvent>,
    base_path: &Path,
    git_workdir: &Option<PathBuf>,
    shared_picker: &SharedFilePicker,
    shared_frecency: &SharedFrecency,
    git_status_worker: &Arc<GitStatusWorker>,
) -> Vec<PathBuf> {
    let events = drop_foreign_admin_files(shared_picker, base_path, events);
    if events.is_empty() {
        return Vec::new();
    }
    let mut git = GitChanges::default();
    let new_dirs = apply_debounced_events(
        mode,
        events,
        base_path,
        git_workdir,
        shared_picker,
        shared_frecency,
        git_status_worker,
        &mut git,
    );
    // Subscribers hear of git state changes whatever became of the batch's index update.
    for root in &git.roots {
        shared_picker.notify_repository_change(root);
    }
    for root in &git.ref_roots {
        shared_picker.notify_ref_change(root);
    }
    if (!git.watches.is_empty() || git.resync_refs)
        && let Ok(guard) = shared_picker.read()
        && let Some(watcher) = guard.as_ref().and_then(|p| p.background_watcher.as_ref())
    {
        for change in git.watches {
            watcher.request_git_state(change);
        }
        if git.resync_refs {
            watcher.send(WatchTask::SyncRefs);
        }
    }
    new_dirs
}

// Other worktrees' admin dirs reach the index only through its ref watches, and their busy
// files mean nothing to it: a batch of them costs one lock and a prefix check per path.
fn drop_foreign_admin_files(
    picker: &SharedFilePicker,
    base_path: &Path,
    events: Vec<DebouncedEvent>,
) -> Vec<DebouncedEvent> {
    let signals = picker.read().ok().and_then(|guard| {
        let watcher = guard.as_ref()?.background_watcher.as_ref()?;
        watcher.watches.refs.signals()
    });
    let Some(signals) = signals else {
        return events;
    };
    (events.into_iter())
        .filter(|event| {
            let paths = &event.event.paths;
            paths.is_empty()
                || !paths
                    .iter()
                    .all(|path| signals.foreign_admin_file(path, base_path))
        })
        .collect()
}

// What a batch tells of git state: the work trees whose state changed, those whose refs
// changed, the git dirs and logs that need watches of their own, and whether the ref
// watches need to follow a ref store.
#[derive(Default)]
struct GitChanges {
    roots: ahash::AHashSet<PathBuf>,
    ref_roots: ahash::AHashSet<PathBuf>,
    watches: Vec<GitStateChange>,
    resync_refs: bool,
}

enum GitStateChange {
    // A `.git` appeared in this directory, which the walk had already reached.
    Repository(PathBuf),
    // This git dir gained its `logs`.
    Log(PathBuf),
}

// Applies a batch and collects the work trees whose git state it changed.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(name = "fs_events", skip(events, shared_picker, shared_frecency, git_status_worker, git), level = Level::DEBUG)]
fn apply_debounced_events(
    mode: FFFMode,
    events: Vec<DebouncedEvent>,
    base_path: &Path,
    git_workdir: &Option<PathBuf>,
    shared_picker: &SharedFilePicker,
    shared_frecency: &SharedFrecency,
    git_status_worker: &Arc<GitStatusWorker>,
    git: &mut GitChanges,
) -> Vec<PathBuf> {
    // this will be called very often, we have to minimiy the lock time for file picker
    let repo = git_workdir.as_ref().and_then(|p| Repository::open(p).ok());
    // Prefer the walker's own ignore rules (zlob); grab a cheap Arc clone once
    // per batch so we don't hold the picker lock during filtering.
    let (walker_rules, follow_symlinks, ref_signals) = shared_picker
        .read()
        .ok()
        .and_then(|g| {
            g.as_ref().map(|p| {
                let watcher = p.background_watcher.as_ref();
                let ref_signals = watcher.and_then(|watcher| watcher.watches.refs.signals());
                (p.ignore_rules(), p.follows_symlinks(), ref_signals)
            })
        })
        .unwrap_or((None, false, None));
    let ref_only = |path: &Path| {
        (ref_signals.as_ref()).is_some_and(|signals| signals.ref_only(path, base_path))
    };
    let filter = IgnoreFilter::new(base_path, walker_rules, repo.as_ref());
    let mut need_full_git_rescan = false;
    let mut batch_overflow_attempted = false;
    let mut paths_to_remove = Vec::new();
    let mut dirs_to_remove: Vec<PathBuf> = Vec::new();
    let mut paths_to_add_or_modify = Vec::new();
    let mut new_dirs_to_watch = Vec::new();
    let mut dirs_to_reconcile: Vec<PathBuf> = Vec::new();
    let mut affected_paths_count = 0usize;
    let mut explicit_renames: Vec<(PathBuf, PathBuf)> = Vec::new();

    let watch_registry = shared_picker.watch_registry();
    let need_events_propagation = watch_registry.is_active();

    // Whether the index holds `path` as a directory, and as a live file.
    let indexed_as = |path: &Path| {
        shared_picker
            .read()
            .ok()
            .and_then(|guard| {
                guard.as_ref().map(|picker| {
                    let file = picker
                        .get_file_by_path(path)
                        .is_some_and(|file| !file.is_deleted());
                    (picker.knows_dir(path), file)
                })
            })
            .unwrap_or((false, false))
    };

    let try_trigger_full_rescan = |reason: RescanReason| -> bool {
        request_rescan(shared_picker, shared_frecency, base_path, reason)
    };

    // Git state changes are collected first, so a rescan this batch starts loses none.
    let changes = events.iter().filter(|event| {
        !matches!(
            event.event.kind,
            EventKind::Access(
                AccessKind::Read
                    | AccessKind::Open(_)
                    | AccessKind::Close(AccessMode::Read | AccessMode::Execute)
            )
        )
    });
    let paths = changes.flat_map(|event| {
        // A file's removal never takes a repository with it.
        let removed = matches!(
            event.event.kind,
            EventKind::Remove(
                notify::event::RemoveKind::Folder
                    | notify::event::RemoveKind::Any
                    | notify::event::RemoveKind::Other
            )
        );
        let kind = event.event.kind;
        event
            .event
            .paths
            .iter()
            .map(move |path| (path, kind, removed))
    });
    // Refs are looked at only while a listener follows a repository.
    let mut ref_changes = RefChanges::default();
    for (path, kind, removed) in paths {
        if let Some(signals) = &ref_signals
            && signals.may_hold(path, base_path)
        {
            signals.classify(path, kind, &mut ref_changes);
        }
        // A repository whose work tree or git dir went, or a directory above them, is
        // recorded anew when it returns. Linux reports a watched git dir's own removal too.
        if removed {
            shared_picker.forget_git_dirs_under(path);
        }
        if is_dotgit_change_affecting_status(path, &repo) {
            if let Some(workdir) = git_workdir {
                git.roots.insert(workdir.clone());
            }
        } else if (is_git_file(path) || !path.starts_with(base_path))
            && !ref_only(path)
            && let Some(root) = git_state_root(shared_picker, path)
            && git_workdir.as_deref() != Some(root.as_path())
            && root.starts_with(base_path)
            && !filter.is_ignored(&root)
        {
            // A repository nested in the index, which the walk and its watches reach.
            git.roots.insert(root);
        }
    }
    if let Some(signals) = &ref_signals {
        git.ref_roots.extend(
            (ref_changes.work_trees.into_iter())
                .filter(|work_tree| signals.is_own(work_tree) || !filter.is_ignored(work_tree)),
        );
        git.resync_refs = ref_changes.resync;
    }

    for debounced_event in &events {
        // It is very important to not react to the access errors because we inevitably
        // gonna trigger the sync by our own preview or other unnecessary noise
        if matches!(
            debounced_event.event.kind,
            EventKind::Access(
                AccessKind::Read
                    | AccessKind::Open(_)
                    | AccessKind::Close(AccessMode::Read | AccessMode::Execute)
            )
        ) {
            continue;
        }

        // When macOS FSEvents (or other backends) overflow their event buffer, the kernel
        // drops individual events and emits a rescan flag telling us to re-scan the subtree
        if debounced_event.event.need_rescan() {
            let small_and_known = debounced_event.event.paths.len() < 16 // this should be usually one event
                && debounced_event
                    .paths
                    .iter()
                    // but we are smart enough and not falling into the paths
                    .all(|p| !p.is_dir() && !filter.is_ignored(p));

            if !small_and_known && try_trigger_full_rescan(RescanReason::KernelEventLoss) {
                return Vec::new();
            }

            // Small batches and throttled rescans fall through: the listed
            // paths are still applied incrementally below.
        }

        tracing::debug!(event = ?debounced_event.event, "Processing FS event");

        // Backends that pair the halves for us hand over [from, to] directly.
        // The per-path pass below still classifies both, we only note the link.
        if let EventKind::Modify(ModifyKind::Name(RenameMode::Both)) = debounced_event.event.kind
            && let [from, to] = debounced_event.event.paths.as_slice()
        {
            explicit_renames.push((from.clone(), to.clone()));
        }

        for path in &debounced_event.event.paths {
            if matches!(
                path.file_name().and_then(|f| f.to_str()),
                Some(".ignore") | Some(".gitignore")
            ) {
                if path
                    .parent()
                    .is_some_and(|parent| filter.is_ignored(parent))
                {
                    continue;
                }

                info!(
                    "Detected change in ignore definition file: {}",
                    path.display()
                );

                if try_trigger_full_rescan(RescanReason::IgnoreFileChanged) {
                    return Vec::new();
                }

                // Throttled: fall through so the ignore file itself stays
                // indexed; the stale rules heal on the next admitted rescan.
            }

            if is_dotgit_change_affecting_status(path, &repo) {
                need_full_git_rescan = true;
            }

            let outside = !path.starts_with(base_path);
            if outside && ref_only(path) {
                continue;
            }
            // Bare repositories keep worktree git dirs outside of any `.git` component.
            let in_own_git_dir = repo
                .as_ref()
                .is_some_and(|repo| path.starts_with(repo.path()));
            if is_git_file(path) || outside || in_own_git_dir {
                // A repository created in a directory the walk already reached: its git
                // state gets watches of its own, as a walked one's does.
                if !outside
                    && path.file_name().is_some_and(|name| name == ".git")
                    && path.parent().is_some_and(|parent| {
                        git_workdir.as_deref() != Some(parent) && !filter.is_ignored(parent)
                    })
                    && path.symlink_metadata().is_ok()
                    && let Some(work_tree) = path.parent()
                {
                    git.watches
                        .push(GitStateChange::Repository(work_tree.to_path_buf()));
                }
                // A git dir's log appears with the first update of HEAD after `git init`,
                // after its git dir was watched: it needs a watch of its own.
                if path.file_name().is_some_and(|name| name == "logs")
                    && let Some(git_dir) = path.parent()
                    && is_git_dir(git_dir)
                    && path.is_dir()
                {
                    git.watches.push(GitStateChange::Log(git_dir.to_path_buf()));
                }
                continue;
            }

            // The event kind says which path changed, the filesystem what it is now.
            //
            // We cannot rely on the filesystem alone because a freshly created file might
            // not be visible yet, and not on the kind alone because macOS often sends
            // Modify(Name(Any)) instead of Remove and for both sides of a rename.
            let kind = debounced_event.event.kind;
            let is_removal = matches!(kind, EventKind::Remove(_));
            // A file removal drops at most that entry, so it needs no stat; on `rm -rf`
            // those are nearly all of the batch.
            let state = if matches!(kind, EventKind::Remove(RemoveKind::File)) {
                PathState::Gone
            } else {
                PathState::of(path, follow_symlinks)
            };

            match state {
                // A known directory removed, created or renamed may have been replaced: a
                // walk below reconciles its subtree with the disk.
                PathState::Dir { walkable: true }
                    if matches!(
                        kind,
                        EventKind::Remove(_)
                            | EventKind::Create(_)
                            | EventKind::Modify(ModifyKind::Name(_))
                    ) =>
                {
                    let (known_dir, known_file) = indexed_as(path);
                    if known_dir {
                        dirs_to_reconcile.push(path.to_path_buf());
                    } else {
                        // A file the directory replaced leaves with its entry.
                        if known_file {
                            paths_to_remove.push((path.as_path(), false));
                        }
                        if !filter.is_ignored(path) {
                            new_dirs_to_watch.push(path.to_path_buf());
                        }
                    }
                }
                PathState::Dir { .. } if !is_removal => {
                    if !filter.is_ignored(path) {
                        new_dirs_to_watch.push(path.to_path_buf());
                    }
                }
                // Directory-level remove: both fsevents and inotify deliver a single
                // `Remove(Folder)` for a whole tree (e.g. `git reset --hard` wiping a dir).
                _ if matches!(kind, EventKind::Remove(RemoveKind::Folder)) => {
                    dirs_to_remove.push(path.to_path_buf());
                }
                PathState::Gone | PathState::Dir { .. } => {
                    // best effort but doesn't require a stat and generally correct
                    let maybe_directory = !matches!(kind, EventKind::Remove(RemoveKind::File));
                    paths_to_remove.push((path.as_path(), maybe_directory));
                }
                // A directory a file replaced is not a file entry: it expands to its subtree.
                PathState::File if is_removal => {
                    let maybe_directory = !matches!(kind, EventKind::Remove(RemoveKind::File));
                    paths_to_remove.push((path.as_path(), maybe_directory));
                }
                PathState::File => {
                    // For additions/modifications, still filter gitignored files.
                    if !filter.is_ignored(path) {
                        paths_to_add_or_modify.push(path.as_path());
                    }
                }
            }
        }

        affected_paths_count += debounced_event.event.paths.len();
        if !batch_overflow_attempted && affected_paths_count > MAX_BATCH_PATHS {
            batch_overflow_attempted = true;
            warn!(
                ?affected_paths_count,
                max = MAX_BATCH_PATHS,
                "Too many affected paths in a single batch, triggering full rescan",
            );

            if try_trigger_full_rescan(RescanReason::EventBatchOverflow) {
                return Vec::new();
            }
        }
    }

    let replaced = ReplacedDirs::walk(
        dirs_to_reconcile,
        base_path,
        repo.is_some(),
        follow_symlinks,
        shared_picker,
    );
    // The walks supersede the batch's other changes inside them; those still name
    // rename candidates.
    let mut covered_removals = Vec::new();
    let mut covered_additions = Vec::new();
    if !replaced.is_empty() {
        paths_to_remove.retain(|&(path, may_be_dir)| {
            let covered = replaced.covers(path);
            if covered && replaced.drops(path, base_path) {
                covered_removals.push((path, may_be_dir));
            }
            !covered
        });
        paths_to_add_or_modify.retain(|path| {
            let covered = replaced.covers(path);
            if covered {
                covered_additions.push(*path);
            }
            !covered
        });
        dirs_to_remove.retain(|dir| !replaced.covers(dir));
        new_dirs_to_watch.retain(|dir| !replaced.covers(dir));
    }

    // Backends repeat a path within a batch; each new directory is walked once.
    new_dirs_to_watch.sort_unstable();
    new_dirs_to_watch.dedup();

    // It's important to get the allocated sort
    sort_with_buffer(paths_to_add_or_modify.as_mut_slice(), |a, b| {
        a.as_os_str().cmp(b.as_os_str())
    });
    paths_to_add_or_modify.dedup_by(|a, b| a.as_os_str().eq(b.as_os_str()));

    info!(
        "Event processing summary: {} to remove, {} dirs to remove, {} to add/modify, {} new dirs, {} dirs to reconcile",
        paths_to_remove.len(),
        dirs_to_remove.len(),
        paths_to_add_or_modify.len(),
        new_dirs_to_watch.len(),
        replaced.len(),
    );

    if paths_to_remove.is_empty()
        && dirs_to_remove.is_empty()
        && paths_to_add_or_modify.is_empty()
        && replaced.is_empty()
        && !need_full_git_rescan
    {
        debug!("No file index changes to apply");
        return new_dirs_to_watch;
    }

    let mut files_to_update_git_status = Vec::new();
    let mut index_update_rejected = false;
    let mut overflow_count = 0;
    let mut removed_from_dirs = Vec::new();
    // Destination path -> (kind, rename source). One entry per path, so a
    // remove+add pair recognised as a rename collapses into a single event.
    let mut watch_events: ahash::AHashMap<PathBuf, (WatchEventKind, Option<PathBuf>)> =
        ahash::AHashMap::new();
    // Destination -> source for every rename recognised in this batch.
    let mut renames: ahash::AHashMap<PathBuf, PathBuf> = ahash::AHashMap::new();

    if !paths_to_remove.is_empty()
        || !dirs_to_remove.is_empty()
        || !paths_to_add_or_modify.is_empty()
        || !replaced.is_empty()
    {
        debug!(
            "Applying file index changes: {} to remove, {} dirs to remove, {} to add/modify, {} dirs to reconcile",
            paths_to_remove.len(),
            dirs_to_remove.len(),
            paths_to_add_or_modify.len(),
            replaced.len(),
        );

        let Ok(mut guard) = shared_picker.write() else {
            error!("Failed to acquire file picker write lock");
            return new_dirs_to_watch;
        };
        let Some(ref mut picker) = *guard else {
            error!("File picker not initialized");
            return new_dirs_to_watch;
        };

        // Must run before the removals below, while the vanished files still
        // carry their indexed size/mtime.
        // Files a reconciliation removes or adds are rename candidates too.
        let rename_removals: Vec<(&Path, bool)> = paths_to_remove
            .iter()
            .chain(&covered_removals)
            .copied()
            .collect();
        let rename_additions: Vec<&Path> = paths_to_add_or_modify
            .iter()
            .chain(&covered_additions)
            .copied()
            .collect();
        renames.extend(detect_renames(
            picker,
            base_path,
            &rename_removals,
            &rename_additions,
            &explicit_renames,
        ));

        // Presence before this batch: a tombstoned path was removed by an earlier batch.
        let is_live = |picker: &FilePicker, path: &Path| {
            picker
                .get_file_by_path(path)
                .is_some_and(|file| !file.is_deleted())
        };
        let lives = |paths: &[&Path]| -> Vec<bool> {
            if need_events_propagation {
                paths.iter().map(|path| is_live(picker, path)).collect()
            } else {
                Vec::new()
            }
        };
        let live_before = lives(&paths_to_add_or_modify);
        let covered_live_before = lives(&covered_additions);

        for (path, may_be_dir) in &paths_to_remove {
            let was_live = need_events_propagation && is_live(picker, path);
            let removed = picker.remove_file_by_path(path);

            if removed {
                if was_live {
                    watch_events.insert(path.to_path_buf(), (WatchEventKind::Removed, None));
                }
            } else if *may_be_dir {
                // Not an indexed file: likely a dir renamed out of the tree
                // (no Remove(Folder) is emitted), expand it per indexed file.
                dirs_to_remove.push(path.to_path_buf());
            }
        }

        // Single index scan for all dirs; misses (never-indexed paths) are free.
        dirs_to_remove.sort_unstable();
        dirs_to_remove.dedup();
        if !dirs_to_remove.is_empty() {
            let dirs = dirs_to_remove.iter().map(PathBuf::as_path);
            if need_events_propagation {
                picker.remove_all_files_in_dirs_with_callback(dirs, |path| {
                    removed_from_dirs.push(path.to_path_buf());
                })
            } else {
                picker.remove_all_files_in_dirs(dirs)
            };
        }

        if need_events_propagation {
            for path in removed_from_dirs.drain(..) {
                watch_events.insert(path, (WatchEventKind::Removed, None));
            }
        }

        // In the same update as the batch's removals, so files kept by a replaced
        // directory never leave the index.
        let mut reconciled_events = Vec::new();
        if replaced
            .apply(
                picker,
                base_path,
                |path| {
                    if need_events_propagation {
                        watch_events.insert(path.to_path_buf(), (WatchEventKind::Removed, None));
                    }
                },
                &mut reconciled_events,
                &mut files_to_update_git_status,
            )
            .is_err()
        {
            index_update_rejected = true;
        }
        if need_events_propagation {
            for (path, kind) in reconciled_events {
                watch_events.insert(path, (kind, None));
            }
            // A file the batch names is a change even when the walk found it as indexed,
            // as when a scan read the directory after it was replaced.
            for (position, path) in covered_additions.iter().enumerate() {
                if index_update_rejected
                    || watch_events.contains_key(*path)
                    || !is_live(picker, path)
                {
                    continue;
                }
                let kind = if covered_live_before[position] {
                    WatchEventKind::Modified
                } else {
                    WatchEventKind::Created
                };
                watch_events.insert(path.to_path_buf(), (kind, None));
            }
        }

        files_to_update_git_status.reserve(paths_to_add_or_modify.len());
        for (position, path) in paths_to_add_or_modify.iter().enumerate() {
            if index_update_rejected {
                // The rescan picks up this path and every later one.
                break;
            }
            let existed = live_before.get(position).copied().unwrap_or(false);

            let applied = picker.apply_create_or_modify(path);
            if applied.is_ok() {
                files_to_update_git_status.push(path.to_path_buf());
                if need_events_propagation {
                    let kind = if existed {
                        WatchEventKind::Modified
                    } else {
                        WatchEventKind::Created
                    };
                    watch_events.insert(path.to_path_buf(), (kind, None));
                }
            } else if matches!(applied, Err(IndexUpdateError::CapacityExhausted)) {
                index_update_rejected = true;
            }
        }

        // A destination the picker rejected is not a rename: its source stays
        // Removed and no history is carried. For the rest, the move is one event.
        renames.retain(|to, _| is_live(picker, to));
        if need_events_propagation {
            for (to, from) in &renames {
                watch_events.remove(from);
                watch_events.insert(to.clone(), (WatchEventKind::Renamed, Some(from.clone())));
            }
        }

        overflow_count = picker.get_overflow_files().len();
    }

    info!(
        files_updated = files_to_update_git_status.len(),
        overflow_count, "File index changes applied",
    );

    let rescan_started = if index_update_rejected || overflow_count > MAX_OVERFLOW_FILES {
        let reason = if index_update_rejected {
            RescanReason::IndexUpdateRejected
        } else {
            RescanReason::OverflowCapacity
        };

        info!(%reason, "Watcher faced limit of index overflow. Triggering rescan");
        try_trigger_full_rescan(reason)
    } else {
        false
    };

    // When the rescan is throttled the incrementally applied changes are
    // still the freshest state we have — propagate them to subscribers.
    if !rescan_started && need_events_propagation {
        watch_registry.dispatch(
            base_path,
            watch_events
                .into_iter()
                .map(|(path, (kind, from))| RawWatchEvent {
                    path,
                    kind,
                    is_ignored: false,
                    from,
                })
                .collect(),
        );
    }

    // Carry the access history onto the new path. Copied, not moved: checking
    // out a revision where the old path exists again must still rank it.
    if !renames.is_empty() {
        let mut carried = Vec::new();
        if let Ok(frecency_guard) = shared_frecency.read()
            && let Some(ref frecency) = *frecency_guard
        {
            for (to, from) in &renames {
                match frecency.copy_history(from, to) {
                    Ok(true) => carried.push(to.clone()),
                    Ok(false) => {}
                    Err(e) => error!(
                        ?from,
                        ?to,
                        "Failed to carry frecency over a rename: {:?}",
                        e
                    ),
                }
            }
        }

        if !carried.is_empty() {
            info!(count = carried.len(), "Carried frecency across renames");
            if let Ok(mut picker_guard) = shared_picker.write()
                && let Some(ref mut picker) = *picker_guard
                && let Ok(frecency_guard) = shared_frecency.read()
                && let Some(ref frecency) = *frecency_guard
            {
                for path in &carried {
                    let _ = picker.update_single_file_frecency(path, frecency);
                }
            }
        }
    }

    // AI mode: auto-track frecency for all modified/created files.
    // Uses a 5-minute cooldown per file to prevent score inflation from rapid
    // burst edits (AI agents often edit the same file many times in minutes).
    // This runs after apply_changes so the picker write lock is released.
    if mode.is_ai() && !paths_to_add_or_modify.is_empty() {
        let mut tracked_count = 0usize;
        let shared_base = shared_picker.read().ok().and_then(|guard| {
            guard
                .as_ref()?
                .worktree_shared_base()
                .map(Path::to_path_buf)
        });
        if let Ok(frecency_guard) = shared_frecency.read()
            && let Some(ref frecency) = *frecency_guard
        {
            for path in &paths_to_add_or_modify {
                // Skip if this file was tracked less than 5 minutes ago
                let should_track = match frecency.seconds_since_last_access(path) {
                    Ok(Some(secs)) => secs >= AI_MODE_COOLDOWN_SECS,
                    Ok(None) => true, // Never tracked before
                    Err(_) => true,   // DB error, track anyway
                };
                if !should_track {
                    continue;
                }

                let shared = shared_base
                    .as_deref()
                    .and_then(|shared_base| crate::git::shared_twin(path, base_path, shared_base));
                if let Err(e) = frecency.track_access_shared(path, shared.as_deref()) {
                    error!("Failed to track frecency for {:?}: {:?}", path, e);
                } else {
                    tracked_count += 1;
                }
            }
            if tracked_count > 0 {
                info!("AI mode: tracked frecency for {} files", tracked_count);
            }
        }

        // Update in-memory frecency scores for tracked files
        if tracked_count > 0
            && let Ok(mut picker_guard) = shared_picker.write()
            && let Some(ref mut picker) = *picker_guard
            && let Ok(frecency_guard) = shared_frecency.read()
            && let Some(ref frecency) = *frecency_guard
        {
            for path in &paths_to_add_or_modify {
                let _ = picker.update_single_file_frecency(path, frecency);
            }
        }
    }

    // do not try to update the paths if we anyway going to rescan everything from scratch
    // no repo => no consumer thread, so don't accumulate paths nobody will drain
    if !index_update_rejected && repo.is_some() {
        if need_full_git_rescan {
            // A full git rescan re-reads every tracked path (including ones that just
            // went clean after a commit), so it already subsumes the per-path update.
            git_status_worker.request_full_rescan();
        } else if !files_to_update_git_status.is_empty() {
            git_status_worker.enqueue_paths(files_to_update_git_status);
        }
    }

    new_dirs_to_watch
}

// Test seam for out-of-process tests: while the file named by this variable exists,
// watchers of debug builds hold directories created after the scan unindexed. Events
// keep arriving; the held directories are indexed once the file is removed.
#[cfg(debug_assertions)]
fn hold_dir_indexing() {
    let Some(gate) = std::env::var_os("FFF_TEST_HOLD_DIR_INDEXING") else {
        return;
    };
    while Path::new(&gate).exists() {
        std::thread::sleep(Duration::from_millis(10));
    }
}

// Test seam: watchers use notify's polling backend instead of the OS event service, so
// tests check the event handling without depending on how promptly the OS delivers
// events. Tests set `FFF_TEST_POLL_WATCHER_MS` to an interval in milliseconds or
// register their roots with `poll_watcher_for`. Release builds outside tests always
// use the native backend.
#[cfg(any(test, debug_assertions))]
fn test_poll_interval(base_path: &Path) -> Option<Duration> {
    #[cfg(test)]
    if NATIVE_ROOTS.lock().iter().any(|root| root == base_path) {
        return None;
    }
    if POLLED_ROOTS.lock().iter().any(|root| root == base_path) {
        return Some(Duration::from_millis(25));
    }
    let interval = std::env::var("FFF_TEST_POLL_WATCHER_MS").ok()?;
    match interval.parse() {
        Ok(millis) => Some(Duration::from_millis(millis)),
        Err(e) => {
            warn!(?e, %interval, "Ignoring FFF_TEST_POLL_WATCHER_MS");
            None
        }
    }
}

#[cfg(not(any(test, debug_assertions)))]
fn test_poll_interval(_base_path: &Path) -> Option<Duration> {
    None
}

#[cfg(any(test, debug_assertions))]
static POLLED_ROOTS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

// Roots whose watchers tests of native backends start natively, whatever the variable says.
#[cfg(test)]
static NATIVE_ROOTS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Test seam: watchers started later for `root` in this process poll instead of
/// using OS events. Debug and test builds only.
#[cfg(any(test, debug_assertions))]
#[doc(hidden)]
pub fn poll_watcher_for(root: &Path) {
    let root = crate::path_utils::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    POLLED_ROOTS.lock().push(root);
}

/// Indexes a directory created after the scan.
pub(super) fn index_new_directory(
    dir: &Path,
    shared_picker: &SharedFilePicker,
    shared_frecency: &SharedFrecency,
    git_workdir: &Option<PathBuf>,
    git_status_worker: &Arc<GitStatusWorker>,
    dir_hook: Option<&DirHook>,
) {
    let repo = git_workdir.as_ref().and_then(|p| Repository::open(p).ok());
    let (base_path, follow_symlinks) = match shared_picker.read().ok().and_then(|g| {
        g.as_ref()
            .map(|p| (p.base_path().to_path_buf(), p.follows_symlinks()))
    }) {
        Some(state) => state,
        None => return,
    };

    if !dir.is_dir() {
        return;
    }

    let Some(walk) = DirWalk::new(dir, &base_path, repo.is_some(), follow_symlinks, dir_hook)
    else {
        return;
    };

    let mut indexed_files = Vec::new();
    // A directory already known can be walked again: its indexed files are
    // reported only when the walk changed them.
    let mut file_events = Vec::new();
    let update_rejected = {
        let Ok(mut guard) = shared_picker.write() else {
            return;
        };

        let Some(ref mut picker) = *guard else {
            return;
        };

        // Once capacity runs out the rescan covers the rest of the subtree.
        walk.apply(picker, &base_path, &mut file_events, &mut indexed_files)
            .is_err()
    };
    // The lock is released before the rescan is requested. A started rescan
    // replaces the accepted prefix, so its events are not replayed; a deferred
    // one leaves them to be reported now.
    let rescan_started = update_rejected
        && request_rescan(
            shared_picker,
            shared_frecency,
            &base_path,
            RescanReason::IndexUpdateRejected,
        );
    let added = indexed_files.len();

    let watch_registry = shared_picker.watch_registry();
    if !rescan_started && watch_registry.is_active() {
        let events = file_events
            .into_iter()
            .map(|(path, kind)| RawWatchEvent {
                path,
                kind,
                is_ignored: false,
                from: None,
            })
            .collect();

        watch_registry.dispatch(&base_path, events);
    }

    if !rescan_started && repo.is_some() {
        git_status_worker.enqueue_paths(indexed_files);
    }

    debug!(
        "Indexed new {} files from new directory {}",
        added,
        dir.display(),
    );
}

// Subscribes directories one at a time as a walk reaches them, and the git state of the
// repositories among them, which walks skip.
struct DirSubscriptions {
    watches: Arc<Watches>,
    recursive: bool,
    picker: crate::shared::WeakFilePicker,
    base_path: PathBuf,
    watched: AtomicUsize,
    consecutive_failures: AtomicUsize,
}

impl DirSubscriptions {
    fn hook(
        watches: &Arc<Watches>,
        recursive: bool,
        picker: &crate::shared::WeakFilePicker,
        base_path: &Path,
    ) -> DirHook {
        let subscriptions = Self {
            watches: Arc::clone(watches),
            recursive,
            picker: picker.clone(),
            base_path: base_path.to_path_buf(),
            watched: AtomicUsize::new(0),
            consecutive_failures: AtomicUsize::new(0),
        };
        Arc::new(move |dir: &Path| {
            if !subscriptions.recursive {
                subscriptions.subscribe(dir);
            }
            subscriptions.watch_git_state(dir);
        })
    }

    fn subscribe(&self, dir: &Path) {
        if self.consecutive_failures.load(Ordering::Relaxed) >= MAX_CONSECUTIVE_WATCH_FAILURES {
            return;
        }
        let mut guard = self.watches.streams.lock();
        let Some(streams) = guard.as_mut() else {
            return;
        };
        match streams.tree.watch(dir, RecursiveMode::NonRecursive) {
            Ok(()) => {
                self.watched.fetch_add(1, Ordering::Relaxed);
                self.consecutive_failures.store(0, Ordering::Relaxed);
            }
            Err(e) => {
                let failures = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
                if failures <= 4 {
                    warn!("Failed to watch directory {}: {}", dir.display(), e);
                }
                if failures == MAX_CONSECUTIVE_WATCH_FAILURES {
                    warn!(
                        consecutive_failures = failures,
                        watched = self.watched.load(Ordering::Relaxed),
                        "Giving up setting file watcher for all the directories. Check if your system has enough fs watchers limit."
                    );
                }
            }
        }
    }

    // Records the git dir of a repository whose work tree is `dir`, a `.git` directory or
    // the one a `.git` file names, as submodules and linked worktrees have, and watches it
    // unless the recursive watch of the tree already covers it. A git dir is watched when
    // it is new or another directory than the recorded one, as after a repository was
    // removed and created again, also within one batch: watching a path again is not free
    // everywhere, as the polling backend takes a new snapshot and loses what changed since
    // its last poll. Such a repository is announced once its watches are in place, since
    // `git init`, a clone or a worktree add and a first `git add` are quicker than a walk.
    // The first scan of an index has no subscribers to hear it. Its ref watches, while a ref
    // listener follows it, go in the same update.
    fn watch_git_state(&self, dir: &Path) {
        let Some(git_dir) = git_dir_of(dir) else {
            return;
        };
        let Some(picker) = self.picker.upgrade() else {
            return;
        };
        if !picker.register_git_dir(&git_dir, dir) {
            return;
        }
        let (appeared, restarted) = {
            let mut guard = self.watches.streams.lock();
            let Some(streams) = guard.as_mut() else {
                return;
            };
            let mut ops = Vec::new();
            if !self.recursive || !git_dir.starts_with(&self.base_path) {
                ops.extend(git_dir_watches(&git_dir));
            }
            // A repository created again lost the watches of its refs; a linked worktree
            // shares its main repository's.
            let renew = (super::common_dir_of(&git_dir) == git_dir).then_some(git_dir.as_path());
            let refs = self.watches.refs.plan(&picker, renew);
            ops.extend(refs.ops);
            (refs.appeared, self.watches.update(streams, ops))
        };
        if restarted {
            self.watches.recover_outside(&picker);
        }
        for work_tree in appeared {
            picker.notify_ref_change(&work_tree);
        }
        picker.notify_repository_change(dir);
    }
}

impl Drop for DirSubscriptions {
    fn drop(&mut self) {
        debug!(
            watched = *self.watched.get_mut(),
            "Directory watches registered"
        );
    }
}

struct IgnoreFilter<'a> {
    base_path: &'a Path,
    /// Reusable ignore rules from the last walk (zlob backend only).
    rules: Option<Arc<crate::walk::WalkIgnoreRules>>,
    /// libgit2 repo, consulted only when `rules` is `None`. Borrowed from the
    /// caller's repo (also used for git-status queries) to avoid re-opening.
    repo: Option<&'a Repository>,
}

impl<'a> IgnoreFilter<'a> {
    fn new(
        base_path: &'a Path,
        rules: Option<Arc<crate::walk::WalkIgnoreRules>>,
        repo: Option<&'a Repository>,
    ) -> Self {
        Self {
            base_path,
            rules,
            repo,
        }
    }

    /// Whether `path` (absolute) is ignored.
    fn is_ignored(&self, path: &Path) -> bool {
        if let Some(rules) = self.rules.as_ref() {
            let Ok(relative) = path.strip_prefix(self.base_path) else {
                return false;
            };
            // `IgnoreRules::is_ignored` enumerates every ancestor .gitignore
            // layer internally, so a leaf under an ignored directory (rule
            // `build/`, path `build/out.rs`) is caught in one call.
            return rules.is_ignored(relative);
        }
        match self.repo {
            Some(repo) => repo.is_path_ignored(path) == Ok(true),
            // No repo and no rules: the non-code-dir heuristic, applied to the
            // base-relative path so ancestors of the base (e.g. a temp dir
            // under AppData/Local on Windows) never match.
            None => crate::ignore::is_non_code_directory(
                path.strip_prefix(self.base_path).unwrap_or(path),
            ),
        }
    }
}

#[inline]
pub(crate) fn is_git_file(path: &Path) -> bool {
    // it could be in submodule
    path.components()
        .any(|component| component.as_os_str() == ".git")
}

fn is_dotgit_change_affecting_status(changed: &Path, repo: &Option<Repository>) -> bool {
    let Some(repo) = repo.as_ref() else {
        return false;
    };
    changed
        .strip_prefix(repo.path())
        .is_ok_and(affects_worktree_state)
}

// The work tree whose git state the change of `changed` affects, as `affects_worktree_state`
// decides: of a git dir the walk registered, or of the nearest git dir by its shape inside a
// `.git` directory, which is how the git dirs of linked worktrees and submodules sit in
// their main repository.
fn git_state_root(picker: &SharedFilePicker, changed: &Path) -> Option<PathBuf> {
    let name = changed.file_name()?.to_str()?;
    if !matches!(
        name,
        "index" | "index.lock" | "HEAD" | "MERGE_HEAD" | "CHERRY_PICK_HEAD" | "REVERT_HEAD"
    ) {
        return None;
    }
    let affects = |git_dir: &Path| {
        changed
            .strip_prefix(git_dir)
            .is_ok_and(affects_worktree_state)
    };
    // A git dir nested in a registered one, as a linked worktree's is, is found by shape.
    if let Some((git_dir, work_tree)) = picker.registered_git_dir(changed)
        && affects(&git_dir)
    {
        return Some(work_tree);
    }
    let dot_git = changed
        .ancestors()
        .find(|dir| dir.file_name().is_some_and(|name| name == ".git"))?;
    let git_dir = changed
        .ancestors()
        .skip(1)
        .take_while(|dir| dir.starts_with(dot_git))
        .find(|dir| is_git_dir(dir))?;
    if !affects(git_dir) {
        return None;
    }
    let work_tree = if git_dir == dot_git {
        git_dir.parent()?.to_path_buf()
    } else {
        let repo = Repository::open(git_dir).ok()?;
        crate::path_utils::canonicalize(repo.workdir()?).ok()?
    };
    picker.register_git_dir(git_dir, &work_tree);
    Some(work_tree)
}

fn affects_worktree_state(path_in_git_dir: &Path) -> bool {
    // Only react to changes that rewrite the worktree state: commits,
    // staging, checkouts, merges, conflict resolution. Ref-only updates
    // under refs/ (fetch, push, tag writes, pack-refs) do not change
    // which files are modified/untracked, so we deliberately skip them
    // watching refs/ recursively would cost one inotify watch per ref
    // namespace on repos with many branches/remotes.
    if path_in_git_dir == Path::new("index") || path_in_git_dir == Path::new("index.lock") {
        return true;
    }

    if path_in_git_dir == Path::new("HEAD") {
        return true;
    }

    // some of the git ops are not involving nethier index nor HEAD change, or sometimes
    // index updates can arrive too late after the change - that's why we track the log
    // the actual user action, once user
    if path_in_git_dir == Path::new("logs/HEAD") {
        return true;
    }

    // At the git dir's root only: a linked worktree's admin dir below it has its own.
    ["MERGE_HEAD", "CHERRY_PICK_HEAD", "REVERT_HEAD"]
        .iter()
        .any(|name| path_in_git_dir == Path::new(name))
}

// Watches the log a git dir gained, unless the tree's watch covers it, and announces its
// repository, whose HEAD moved before the log was watched.
fn watch_new_log(
    watches: &Watches,
    picker: &SharedFilePicker,
    git_dir: &Path,
    own_work_tree: &Option<PathBuf>,
    covered: bool,
) {
    let own = own_work_tree
        .as_ref()
        .filter(|work_tree| {
            Repository::open(work_tree).is_ok_and(|repo| {
                let canonical = crate::path_utils::canonicalize;
                canonical(repo.path()).ok() == canonical(git_dir).ok()
            })
        })
        .cloned();
    let work_tree = own.or_else(|| {
        picker
            .registered_git_dir(&git_dir.join("logs"))
            .filter(|(registered, _)| registered == git_dir)
            .map(|(_, work_tree)| work_tree)
    });
    let Some(work_tree) = work_tree else {
        return;
    };
    let logs = PathOp::watch_non_recursive(git_dir.join("logs"));
    let restarted = !covered
        && (watches.streams.lock().as_mut())
            .is_some_and(|streams| watches.update(streams, vec![logs]));
    if restarted {
        watches.recover_outside(picker);
    }
    picker.notify_repository_change(&work_tree);
}

// Whether `dir` is a git dir by its shape.
pub(crate) fn is_git_dir(dir: &Path) -> bool {
    dir.join("HEAD").is_file() && (dir.join("objects").is_dir() || dir.join("commondir").is_file())
}

// The git state of a repository lives in its git dir and its log.
fn git_dir_watches(git_dir: &Path) -> impl Iterator<Item = PathOp> {
    [git_dir.to_path_buf(), git_dir.join("logs")]
        .into_iter()
        .filter(|dir| dir.is_dir())
        .map(PathOp::watch_non_recursive)
}

// The git dir of a repository whose work tree is `dir`: its `.git` directory, or the one
// its `.git` file names.
pub(crate) fn git_dir_of(dir: &Path) -> Option<PathBuf> {
    let dot_git = dir.join(".git");
    let metadata = dot_git.symlink_metadata().ok()?;
    if metadata.is_dir() {
        return Some(dot_git);
    }
    if !metadata.is_file() {
        return None;
    }
    let content = std::fs::read_to_string(&dot_git).ok()?;
    let target = content.strip_prefix("gitdir:")?.trim();
    crate::path_utils::canonicalize(dir.join(target)).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FileIdentity {
    size: u64,
    mtime: u64,
}

// Returns destination -> source. Explicit pairs from the OS win outright; the
// rest is inferred from files that kept their exact size and mtime. Call while
// the removed paths are still indexed — that is the only source of their
// metadata once they are gone from disk.
fn detect_renames(
    picker: &crate::file_picker::FilePicker,
    base_path: &Path,
    paths_to_remove: &[(&Path, bool)],
    paths_to_add_or_modify: &[&Path],
    explicit: &[(PathBuf, PathBuf)],
) -> ahash::AHashMap<PathBuf, PathBuf> {
    // Subscribers only ever see paths relative to the base, so a move that
    // crosses the boundary stays a plain removal or creation.
    let mut renames: ahash::AHashMap<PathBuf, PathBuf> = explicit
        .iter()
        .filter(|(from, to)| from != to && from.starts_with(base_path) && to.starts_with(base_path))
        .map(|(from, to)| (to.clone(), from.clone()))
        .collect();

    if paths_to_remove.is_empty() || paths_to_add_or_modify.is_empty() {
        return renames;
    }

    let explicit_sources: ahash::AHashSet<&Path> = renames.values().map(PathBuf::as_path).collect();
    let removed: Vec<(PathBuf, FileIdentity)> = paths_to_remove
        .iter()
        .filter(|(path, _)| !explicit_sources.contains(path))
        .filter_map(|(path, _)| {
            let file = picker.get_file_by_path(path)?;
            Some((
                path.to_path_buf(),
                FileIdentity {
                    size: file.size,
                    mtime: file.modified,
                },
            ))
        })
        .collect();

    let added: Vec<(PathBuf, FileIdentity)> = paths_to_add_or_modify
        .iter()
        .filter(|path| !renames.contains_key(**path))
        // A path already live in the index was edited, not moved into place.
        .filter(|path| {
            picker
                .get_file_by_path(path)
                .is_none_or(|file| file.is_deleted())
        })
        .filter_map(|path| {
            let meta = std::fs::metadata(path).ok()?;
            let mtime = meta
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs();
            Some((
                path.to_path_buf(),
                FileIdentity {
                    size: meta.len(),
                    mtime,
                },
            ))
        })
        .collect();

    for (from, to) in pair_renames_by_identity(&removed, &added) {
        renames.insert(to, from);
    }

    renames
}

// Pair a removed with an added path when they are the only two files in the
// batch sharing a (size, mtime) — a rename preserves both exactly. Uniqueness
// on both sides is what stops bulk operations from cross-wiring histories.
fn pair_renames_by_identity(
    removed: &[(PathBuf, FileIdentity)],
    added: &[(PathBuf, FileIdentity)],
) -> Vec<(PathBuf, PathBuf)> {
    if removed.is_empty() || added.is_empty() {
        return Vec::new();
    }

    // `None` marks an identity claimed by more than one distinct path. The OS
    // repeats a path within a batch (FSEvents does routinely), and a repeat is
    // still one candidate, not a collision.
    fn index_by_identity(
        entries: &[(PathBuf, FileIdentity)],
    ) -> ahash::AHashMap<FileIdentity, Option<&Path>> {
        let mut by_identity: ahash::AHashMap<FileIdentity, Option<&Path>> =
            ahash::AHashMap::default();
        for (path, identity) in entries {
            by_identity
                .entry(*identity)
                .and_modify(|slot| {
                    if *slot != Some(path.as_path()) {
                        *slot = None;
                    }
                })
                .or_insert(Some(path.as_path()));
        }
        by_identity
    }

    let sources = index_by_identity(removed);
    let targets = index_by_identity(added);

    targets
        .into_iter()
        .filter_map(|(identity, target)| {
            let target = target?;
            let source = (*sources.get(&identity)?)?;
            Some((source.to_path_buf(), target.to_path_buf()))
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PathState {
    Gone,
    File,
    // `walkable` when the index's walk descends into it, a symlink only when following them.
    Dir { walkable: bool },
}

impl PathState {
    // One lstat, and a stat of the target only for a symlink.
    fn of(path: &Path, follow_symlinks: bool) -> Self {
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return Self::Gone;
        };
        if meta.is_dir() {
            return Self::Dir { walkable: true };
        }
        if !meta.file_type().is_symlink() {
            return Self::File;
        }
        match std::fs::metadata(path) {
            Ok(target) if target.is_dir() => Self::Dir {
                walkable: follow_symlinks,
            },
            Ok(_) => Self::File,
            Err(_) => Self::Gone,
        }
    }
}

/// Requests the rescan that recovers changes dropped by the watcher; a throttled
/// one stays deferred until the cooldown ends.
pub(crate) fn request_recovery_rescan(
    picker: &SharedFilePicker,
    frecency: &SharedFrecency,
    base_path: &Path,
    reason: RescanReason,
) {
    match picker.trigger_recovery_rescan(frecency, reason) {
        Ok(RescanRequest::Started) => {
            warn!(%reason, "Triggering full rescan");
            picker.watch_registry().dispatch_rescan(base_path);
        }
        Ok(RescanRequest::Deferred) => {
            if let Ok(guard) = picker.read()
                && let Some(watcher) = guard.as_ref().and_then(|p| p.background_watcher.as_ref())
            {
                watcher.wake_for_deferred_rescan();
            }
        }
        Ok(RescanRequest::Dropped) => {}
        Err(e) => error!(%reason, "Failed to trigger full rescan: {:?}", e),
    }
}

/// Returns `true` when the rescan started; throttled requests either stay
/// deferred or leave the incremental state as the freshest one.
fn request_rescan(
    picker: &SharedFilePicker,
    frecency: &SharedFrecency,
    base_path: &Path,
    reason: RescanReason,
) -> bool {
    match picker.trigger_full_rescan_with_reason(frecency, reason) {
        Ok(request) if request.started() => {
            warn!(%reason, "Triggering full rescan");
            picker.watch_registry().dispatch_rescan(base_path);
            true
        }
        Ok(_) => false,
        Err(e) => {
            error!(%reason, "Failed to trigger full rescan: {:?}", e);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_picker::{FilePicker, FilePickerOptions};
    use crate::watch::{WatchEvent, WatchOptions};
    use notify::Event;
    use notify::event::{CreateKind, DataChange, ModifyKind, RemoveKind};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    fn overflow_admission_is_bounded_without_rejecting_existing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let base = crate::path_utils::canonicalize(tmp.path()).unwrap();
        let mut picker = FilePicker::new(FilePickerOptions {
            base_path: base.to_string_lossy().into_owned(),
            watch: false,
            ..Default::default()
        })
        .unwrap();
        picker.collect_files().unwrap();

        for i in 0..MAX_OVERFLOW_FILES {
            let path = base.join(format!("file-{i}.txt"));
            std::fs::write(&path, "initial").unwrap();
            assert!(picker.handle_create_or_modify(&path).is_some());
        }
        let extra = base.join("extra.txt");
        std::fs::write(&extra, "extra").unwrap();
        assert!(picker.add_new_file(&extra).is_none());
        assert_eq!(picker.get_overflow_files().len(), MAX_OVERFLOW_FILES);

        let existing = base.join("file-0.txt");
        std::fs::write(&existing, "modified content").unwrap();
        assert!(picker.handle_create_or_modify(&existing).is_some());
        assert_eq!(picker.get_overflow_files().len(), MAX_OVERFLOW_FILES);
    }

    #[test]
    fn new_directory_overflow_rescans_every_file() {
        let tmp = tempfile::tempdir().unwrap();
        let base = crate::path_utils::canonicalize(tmp.path()).unwrap();
        let shared_picker = SharedFilePicker::default();
        let mut picker = FilePicker::new(FilePickerOptions {
            base_path: base.to_string_lossy().into_owned(),
            watch: false,
            ..Default::default()
        })
        .unwrap();
        picker.collect_files().unwrap();
        *shared_picker.write().unwrap() = Some(picker);
        shared_picker.rebase_watches(&base);
        let (sender, receiver) = mpsc::channel::<Vec<WatchEvent>>();
        shared_picker
            .watch_registry()
            .subscribe(
                &base,
                "**",
                WatchOptions::default(),
                Box::new(move |_, events| {
                    let _ = sender.send(events.to_vec());
                }),
            )
            .unwrap();

        let dir = base.join("new-directory");
        std::fs::create_dir(&dir).unwrap();
        for i in 0..MAX_OVERFLOW_FILES * 2 {
            std::fs::write(dir.join(format!("file-{i}.txt")), "content").unwrap();
        }
        index_new_directory(
            &dir,
            &shared_picker,
            &SharedFrecency::noop(),
            &None,
            &GitStatusWorker::new(),
            None,
        );
        {
            let guard = shared_picker.read().unwrap();
            assert!(guard.as_ref().unwrap().get_overflow_files().len() <= MAX_OVERFLOW_FILES);
        }
        let marker = base.join("after-overflow.txt");
        shared_picker.watch_registry().dispatch(
            &base,
            vec![RawWatchEvent {
                path: marker.clone(),
                kind: WatchEventKind::Removed,
                is_ignored: false,
                from: None,
            }],
        );
        let events = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, WatchEventKind::Rescan);
        assert_eq!(events[0].path, base);
        let next = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].kind, WatchEventKind::Removed);
        assert_eq!(next[0].path, marker);
        assert!(shared_picker.wait_for_indexing_complete(Duration::from_secs(10)));
        let guard = shared_picker.read().unwrap();
        let picker = guard.as_ref().unwrap();
        for i in 0..MAX_OVERFLOW_FILES * 2 {
            assert!(
                picker
                    .get_file_by_path(dir.join(format!("file-{i}.txt")))
                    .is_some()
            );
        }
        assert_eq!(picker.get_overflow_files().len(), 0);
    }

    #[test]
    fn replacement_batch_emits_one_modified_event() {
        let tmp = tempfile::tempdir().unwrap();
        let base = crate::path_utils::canonicalize(tmp.path()).unwrap();
        let path = base.join("file.txt");
        std::fs::write(&path, "before").unwrap();
        let (shared_picker, receiver) = subscribed_picker(&base);

        std::fs::write(&path, "after").unwrap();
        apply(
            &shared_picker,
            &base,
            vec![
                EventKind::Remove(RemoveKind::File),
                EventKind::Create(CreateKind::File),
                EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            ],
            &path,
        );

        let received = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].path, path);
        assert_eq!(received[0].kind, WatchEventKind::Modified);
    }

    // Events of one path split across batches report each applied transition once.
    #[test]
    fn replacement_split_across_batches_emits_removed_then_created() {
        let tmp = tempfile::tempdir().unwrap();
        let base = crate::path_utils::canonicalize(tmp.path()).unwrap();
        let path = base.join("file.txt");
        std::fs::write(&path, "before").unwrap();
        let (shared_picker, receiver) = subscribed_picker(&base);
        let next = || receiver.recv_timeout(Duration::from_secs(1)).unwrap();

        std::fs::remove_file(&path).unwrap();
        apply(
            &shared_picker,
            &base,
            vec![EventKind::Remove(RemoveKind::File)],
            &path,
        );
        assert_eq!(next()[0].kind, WatchEventKind::Removed);

        // A repeated removal of an already removed file is not a transition.
        apply(
            &shared_picker,
            &base,
            vec![EventKind::Remove(RemoveKind::File)],
            &path,
        );
        std::fs::write(&path, "after").unwrap();
        apply(
            &shared_picker,
            &base,
            vec![EventKind::Create(CreateKind::File)],
            &path,
        );
        let received = next();
        assert_eq!(received.len(), 1, "{received:?}");
        assert_eq!(received[0].kind, WatchEventKind::Created);
    }

    fn identity(size: u64, mtime: u64) -> FileIdentity {
        FileIdentity { size, mtime }
    }

    fn entry(path: &str, size: u64, mtime: u64) -> (PathBuf, FileIdentity) {
        (PathBuf::from(path), identity(size, mtime))
    }

    #[test]
    fn unique_size_and_mtime_pairs_as_a_rename() {
        let pairs = pair_renames_by_identity(
            &[entry("/w/old.rs", 120, 1_700_000_000)],
            &[entry("/w/new.rs", 120, 1_700_000_000)],
        );

        assert_eq!(
            pairs,
            vec![(PathBuf::from("/w/old.rs"), PathBuf::from("/w/new.rs"))]
        );
    }

    #[test]
    fn ambiguous_identities_are_never_paired() {
        let two_removed = pair_renames_by_identity(
            &[entry("/w/a.rs", 10, 5), entry("/w/b.rs", 10, 5)],
            &[entry("/w/c.rs", 10, 5)],
        );
        assert!(
            two_removed.is_empty(),
            "two removed files sharing an identity are ambiguous: {two_removed:?}"
        );

        let two_added = pair_renames_by_identity(
            &[entry("/w/a.rs", 10, 5)],
            &[entry("/w/b.rs", 10, 5), entry("/w/c.rs", 10, 5)],
        );
        assert!(
            two_added.is_empty(),
            "two added files sharing an identity are ambiguous: {two_added:?}"
        );
    }

    #[test]
    fn a_repeated_path_is_one_candidate_not_a_collision() {
        // FSEvents routinely reports the same path several times per batch.
        let pairs = pair_renames_by_identity(
            &[entry("/w/old.rs", 15, 99), entry("/w/old.rs", 15, 99)],
            &[entry("/w/new.rs", 15, 99), entry("/w/new.rs", 15, 99)],
        );

        assert_eq!(
            pairs,
            vec![(PathBuf::from("/w/old.rs"), PathBuf::from("/w/new.rs"))]
        );
    }

    #[test]
    fn empty_files_are_paired_only_when_unambiguous() {
        let unique = pair_renames_by_identity(&[entry("/w/a", 0, 7)], &[entry("/w/b", 0, 7)]);
        assert_eq!(
            unique,
            vec![(PathBuf::from("/w/a"), PathBuf::from("/w/b"))],
            "a lone empty file still has a usable identity"
        );

        let colliding = pair_renames_by_identity(
            &[entry("/w/a", 0, 7), entry("/w/b", 0, 7)],
            &[entry("/w/c", 0, 7), entry("/w/d", 0, 7)],
        );
        assert!(colliding.is_empty(), "{colliding:?}");
    }

    #[test]
    fn differing_metadata_is_not_a_rename() {
        // git checkout rewrites mtime, so branch switches must never pair.
        let fresh_mtime = pair_renames_by_identity(
            &[entry("/w/old.rs", 120, 1_700_000_000)],
            &[entry("/w/new.rs", 120, 1_700_000_900)],
        );
        assert!(fresh_mtime.is_empty(), "{fresh_mtime:?}");

        let other_size = pair_renames_by_identity(
            &[entry("/w/old.rs", 120, 1_700_000_000)],
            &[entry("/w/new.rs", 121, 1_700_000_000)],
        );
        assert!(other_size.is_empty(), "{other_size:?}");
    }

    #[test]
    fn independent_identities_pair_side_by_side() {
        let pairs = pair_renames_by_identity(
            &[entry("/w/a.rs", 10, 1), entry("/w/b.rs", 20, 2)],
            &[entry("/w/y.rs", 20, 2), entry("/w/x.rs", 10, 1)],
        );

        let mut pairs = pairs;
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                (PathBuf::from("/w/a.rs"), PathBuf::from("/w/x.rs")),
                (PathBuf::from("/w/b.rs"), PathBuf::from("/w/y.rs")),
            ]
        );
    }

    #[test]
    fn dotgit_status_filter_matches_worktree_state_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(tmp.path()).unwrap();
        let git_dir = repo.path().to_path_buf();
        let repo = Some(repo);

        // A linked worktree's operation in progress is its own, not the main checkout's.
        for nested in [
            "worktrees/wt/MERGE_HEAD",
            "worktrees/wt/CHERRY_PICK_HEAD",
            "x/REVERT_HEAD",
        ] {
            assert!(!affects_worktree_state(Path::new(nested)), "{nested}");
        }
        let affecting = ["index", "index.lock", "HEAD", "logs/HEAD", "MERGE_HEAD"];
        for p in affecting {
            assert!(
                is_dotgit_change_affecting_status(&git_dir.join(p), &repo),
                "{p} must trigger a git status rescan"
            );
        }

        // Ref-only updates (fetch/push/tags) and commit scratch files must not.
        let non_affecting = [
            "refs/heads/main",
            "refs/heads/main.lock",
            "logs/refs/remotes/origin/main",
            "COMMIT_EDITMSG",
            "packed-refs",
        ];
        for p in non_affecting {
            assert!(
                !is_dotgit_change_affecting_status(&git_dir.join(p), &repo),
                "{p} must NOT trigger a git status rescan"
            );
        }

        // Worktree paths outside .git never match.
        assert!(!is_dotgit_change_affecting_status(
            &tmp.path().join("src/main.rs"),
            &repo
        ));
        assert!(!is_dotgit_change_affecting_status(
            &git_dir.join("index"),
            &None
        ));
    }

    #[test]
    fn removed_repositories_are_forgotten_until_they_return() {
        let tmp = tempfile::tempdir().unwrap();
        let base = crate::path_utils::canonicalize(tmp.path()).unwrap();
        let inner = base.join("inner");
        let worktree = base.join("worktree");
        git2::Repository::init(&inner).unwrap();
        std::fs::create_dir(&worktree).unwrap();
        let (shared_picker, _events) = subscribed_picker(&base);
        let linked = base.join("main/.git/worktrees/worktree");
        assert!(shared_picker.register_git_dir(&inner.join(".git"), &inner));
        assert!(shared_picker.register_git_dir(&linked, &worktree));
        let removed = |path: &Path| {
            let event = Event::new(EventKind::Remove(notify::event::RemoveKind::Folder))
                .add_path(path.to_path_buf());
            handle_debounced_events(
                FFFMode::Neovim,
                vec![DebouncedEvent::new(event, Instant::now())],
                &base,
                &None,
                &shared_picker,
                &SharedFrecency::noop(),
                &GitStatusWorker::new(),
            );
        };
        // A repository's git dir that still exists is kept.
        removed(&inner.join(".git/index.lock"));
        assert!(
            shared_picker
                .registered_git_dir(&inner.join(".git/index"))
                .is_some()
        );
        std::fs::remove_dir_all(&inner).unwrap();
        removed(&inner);
        assert!(
            shared_picker
                .registered_git_dir(&inner.join(".git/index"))
                .is_none()
        );
        // A linked worktree goes with its work tree, wherever its git dir lies.
        std::fs::remove_dir_all(&worktree).unwrap();
        removed(&worktree);
        assert!(
            shared_picker
                .registered_git_dir(&linked.join("index"))
                .is_none()
        );
        // Once back, it is recorded, and so watched, again.
        git2::Repository::init(&inner).unwrap();
        assert!(shared_picker.register_git_dir(&inner.join(".git"), &inner));
        assert!(!shared_picker.register_git_dir(&inner.join(".git"), &inner));
        // Gone and back before any batch saw it go: another directory at the same path.
        std::fs::remove_dir_all(inner.join(".git")).unwrap();
        git2::Repository::init(&inner).unwrap();
        assert!(shared_picker.register_git_dir(&inner.join(".git"), &inner));

        // A removal event forgets it although the path is back, so a file system that hands
        // the new directory the old one's identity cannot hide the change; another event on a
        // path that exists does not.
        let changed = |path: &Path| {
            let event =
                Event::new(EventKind::Modify(notify::event::ModifyKind::Any)).add_path(path.into());
            handle_debounced_events(
                FFFMode::Neovim,
                vec![DebouncedEvent::new(event, Instant::now())],
                &base,
                &None,
                &shared_picker,
                &SharedFrecency::noop(),
                &GitStatusWorker::new(),
            );
        };
        changed(&inner);
        assert!(
            shared_picker
                .registered_git_dir(&inner.join(".git/index"))
                .is_some()
        );
        removed(&inner);
        assert!(
            shared_picker
                .registered_git_dir(&inner.join(".git/index"))
                .is_none()
        );
        assert!(shared_picker.register_git_dir(&inner.join(".git"), &inner));
        removed(&inner.join(".git"));
        assert!(
            shared_picker
                .registered_git_dir(&inner.join(".git/index"))
                .is_none()
        );
    }

    #[test]
    fn repository_listeners_hear_git_state_changes_only() {
        let tmp = tempfile::tempdir().unwrap();
        let base = crate::path_utils::canonicalize(tmp.path()).unwrap();
        let repo = git2::Repository::init(&base).unwrap();
        let workdir = Some(base.clone());
        std::fs::write(base.join("file.txt"), "a").unwrap();
        let (shared_picker, _events) = subscribed_picker(&base);
        let (sender, heard) = mpsc::channel::<PathBuf>();
        let _subscription = shared_picker
            .subscribe_repository_state(move |root| sender.send(root.to_path_buf()).unwrap());
        let handle = |path: PathBuf| {
            let event =
                Event::new(EventKind::Modify(notify::event::ModifyKind::Any)).add_path(path);
            handle_debounced_events(
                FFFMode::Neovim,
                vec![DebouncedEvent::new(event, Instant::now())],
                &base,
                &workdir,
                &shared_picker,
                &SharedFrecency::noop(),
                &GitStatusWorker::new(),
            );
        };
        handle(base.join("file.txt"));
        handle(repo.path().join("refs/heads/main"));
        assert!(
            heard.try_recv().is_err(),
            "a file or a ref change is no git state change"
        );
        handle(repo.path().join("index"));
        assert_eq!(heard.try_recv().unwrap(), base);

        // A repository nested in the index is told apart by its own root.
        let inner = base.join("inner");
        git2::Repository::init(&inner).unwrap();
        handle(inner.join(".git/refs/heads/main"));
        assert!(
            heard.try_recv().is_err(),
            "a ref change is no git state change"
        );
        handle(inner.join(".git/index"));
        assert_eq!(heard.try_recv().unwrap(), inner);
        assert!(
            heard.try_recv().is_err(),
            "the outer repository did not change"
        );

        // Git dirs inside `.git`: a linked worktree's and a submodule's, named by work tree.
        let signature = git2::Signature::now("fff", "fff@example.invalid").unwrap();
        let tree = repo
            .find_tree(repo.index().unwrap().write_tree().unwrap())
            .unwrap();
        repo.commit(Some("HEAD"), &signature, &signature, "init", &tree, &[])
            .unwrap();
        repo.worktree("wt", &base.join("wt"), None).unwrap();
        handle(repo.path().join("worktrees/wt/index"));
        assert_eq!(heard.try_recv().unwrap(), base.join("wt"));
        let mut options = git2::RepositoryInitOptions::new();
        options.workdir_path(&base.join("sub")).no_dotgit_dir(true);
        git2::Repository::init_opts(repo.path().join("modules/sub"), &options).unwrap();
        handle(repo.path().join("modules/sub/HEAD"));
        assert_eq!(heard.try_recv().unwrap(), base.join("sub"));
        assert!(heard.try_recv().is_err());
    }

    #[test]
    fn ref_listeners_hear_the_refs_and_heads_of_the_work_trees_of_their_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let base = crate::path_utils::canonicalize(tmp.path()).unwrap();
        let repo = git2::Repository::init(&base).unwrap();
        std::fs::create_dir(base.join("src")).unwrap();
        std::fs::write(base.join("src/file.txt"), "a").unwrap();
        commit(&repo);
        repo.worktree("wt", &base.join("wt"), None).unwrap();
        let git_dir = crate::path_utils::canonicalize(repo.path()).unwrap();
        let (shared_picker, workdir) = polled_picker(&base);
        let (state_sender, state) = mpsc::channel::<PathBuf>();
        let _state = shared_picker
            .subscribe_repository_state(move |root| state_sender.send(root.to_path_buf()).unwrap());
        let handle = |path: PathBuf| modified(&shared_picker, &base, &workdir, path);
        let (sender, heard) = mpsc::channel::<PathBuf>();
        let subscription = shared_picker
            .subscribe_ref_changes(&base, move |root| sender.send(root.to_path_buf()).unwrap());
        let (src_sender, src_heard) = mpsc::channel::<PathBuf>();
        let _src = shared_picker.subscribe_ref_changes(&base.join("src"), move |root| {
            src_sender.send(root.to_path_buf()).unwrap()
        });

        // Branches live in the common dir, which both work trees share.
        for changed in [
            "refs/heads/main",
            "refs/heads/feature/nested",
            "refs/tags/v1",
            "refs/remotes/origin/main",
            "packed-refs",
        ] {
            handle(git_dir.join(changed));
            assert_eq!(drain(&heard), [base.clone(), base.join("wt")], "{changed}");
            assert_eq!(
                drain(&src_heard),
                [base.as_path()],
                "{changed} below the work tree"
            );
        }
        assert!(
            drain(&state).is_empty(),
            "a ref change is no git state change"
        );

        // HEAD belongs to one work tree, and stays a git state change.
        handle(git_dir.join("HEAD"));
        assert_eq!(drain(&heard), [base.as_path()]);
        handle(git_dir.join("worktrees/wt/HEAD"));
        assert_eq!(drain(&heard), [base.join("wt")]);
        assert!(
            drain(&src_heard) == [base.as_path()],
            "src is not in the linked worktree"
        );
        assert_eq!(drain(&state), [base.clone(), base.join("wt")]);

        // Upstreams and remotes are shared; a worktree's own config is its own.
        handle(git_dir.join("config"));
        assert_eq!(drain(&heard), [base.clone(), base.join("wt")]);
        handle(git_dir.join("worktrees/wt/config.worktree"));
        assert_eq!(drain(&heard), [base.join("wt")]);
        handle(git_dir.join("config.worktree"));
        assert_eq!(drain(&heard), [base.as_path()]);
        drain(&src_heard);

        for unrelated in [
            "config.lock",
            "refs/heads/main.lock",
            "packed-refs.lock",
            "HEAD.lock",
            "refs/stash",
            "refs/notes/commits",
            "FETCH_HEAD",
            "ORIG_HEAD",
            "logs/HEAD",
            "logs/refs/heads/main",
        ] {
            handle(git_dir.join(unrelated));
            assert!(drain(&heard).is_empty(), "{unrelated} is no ref change");
        }
        handle(base.join("refs/heads/main"));
        assert!(drain(&heard).is_empty(), "a project's own refs folder");

        drop(subscription);
        handle(git_dir.join("refs/heads/main"));
        assert!(drain(&heard).is_empty(), "a dropped listener hears nothing");
        assert_eq!(
            drain(&src_heard),
            [base.as_path()],
            "the other one still does"
        );
    }

    #[test]
    fn an_outside_common_dir_watched_for_refs_is_no_git_state() {
        let tmp = tempfile::tempdir().unwrap();
        let root = crate::path_utils::canonicalize(tmp.path()).unwrap();
        let main = root.join("main");
        let repo = git2::Repository::init(&main).unwrap();
        std::fs::write(main.join("file.txt"), "a").unwrap();
        commit(&repo);
        repo.worktree("linked", &root.join("linked"), None).unwrap();
        let main_git_dir = crate::path_utils::canonicalize(repo.path()).unwrap();
        let base = root.join("linked");
        let (shared_picker, workdir) = polled_picker(&base);
        let (state_sender, state) = mpsc::channel::<PathBuf>();
        let _state = shared_picker
            .subscribe_repository_state(move |root| state_sender.send(root.to_path_buf()).unwrap());
        let (sender, heard) = mpsc::channel::<PathBuf>();
        let _refs = shared_picker
            .subscribe_ref_changes(&base, move |root| sender.send(root.to_path_buf()).unwrap());
        let handle = |path: PathBuf| modified(&shared_picker, &base, &workdir, path);

        // The main checkout's own state, seen only because its common dir holds the refs.
        handle(main_git_dir.join("index"));
        handle(main_git_dir.join("HEAD"));
        assert!(drain(&state).is_empty());
        assert!(
            drain(&heard).is_empty(),
            "the main checkout's HEAD is its own"
        );
        assert!(
            (shared_picker.registered_git_dirs().iter()).all(|known| known.git_dir != main_git_dir),
            "the main repository is no repository of the index"
        );

        handle(main_git_dir.join("refs/heads/other"));
        assert_eq!(drain(&heard), [base.as_path()]);
        // HEAD is both kinds of event, and both spell the work tree as one path does.
        handle(main_git_dir.join("worktrees/linked/HEAD"));
        let spelled = |heard: Vec<PathBuf>| {
            (heard.iter())
                .map(|root| root.as_os_str().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(spelled(drain(&heard)), [base.as_os_str()]);
        assert_eq!(spelled(drain(&state)), [base.as_os_str()]);
    }

    #[test]
    fn the_own_git_dir_is_known_once_the_watcher_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let base = crate::path_utils::canonicalize(tmp.path()).unwrap();
        let repo = git2::Repository::init(&base).unwrap();
        std::fs::write(base.join("file.txt"), "a").unwrap();
        commit(&repo);
        let (shared_picker, _workdir) = polled_picker(&base);
        // A walk reaching the work tree, as after an event for the root, watches nothing anew:
        // watching a path again takes a new snapshot when polling and loses what changed since.
        assert!(!shared_picker.register_git_dir(&base.join(".git"), &base));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn fsevents_changes_the_tree_s_stream_only_at_its_setup() {
        let tmp = tempfile::tempdir().unwrap();
        let root = crate::path_utils::canonicalize(tmp.path()).unwrap();
        let (main, other) = (root.join("main"), root.join("other"));
        let repos = [&main, &other].map(|dir| {
            let repo = git2::Repository::init(dir).unwrap();
            std::fs::write(dir.join("file.txt"), "a").unwrap();
            commit(&repo);
            repo
        });
        let linked = root.join("linked");
        repos[0].worktree("linked", &linked, None).unwrap();
        repos[1]
            .worktree("nested", &linked.join("nested"), None)
            .unwrap();
        NATIVE_ROOTS.lock().push(linked.clone());
        let shared_picker = SharedFilePicker::default();
        FilePicker::new_with_shared_state(
            shared_picker.clone(),
            SharedFrecency::noop(),
            FilePickerOptions {
                base_path: linked.to_string_lossy().into_owned(),
                enable_content_indexing: false,
                watch: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(shared_picker.wait_for_scan(Duration::from_secs(30)));
        assert!(shared_picker.wait_for_watcher(Duration::from_secs(30)));
        let watches = (shared_picker.read().unwrap().as_ref())
            .and_then(|picker| {
                picker
                    .background_watcher
                    .as_ref()
                    .map(BackgroundWatcher::watches)
            })
            .unwrap();
        let paths = |outside: bool| {
            let guard = watches.streams.lock();
            let streams = guard.as_ref().unwrap();
            let stream = match outside {
                true => streams.outside.as_ref().unwrap(),
                false => &streams.tree,
            };
            let mut paths: Vec<_> = stream.watched_paths().unwrap();
            paths.sort();
            paths
        };

        // The walk met the nested worktree's git dir, outside the tree, after the setup.
        assert_eq!(paths(false), [(linked.clone(), RecursiveMode::Recursive)]);
        let _refs = shared_picker.subscribe_ref_changes(&linked, |_| {});
        assert_eq!(paths(false), [(linked.clone(), RecursiveMode::Recursive)]);
        let outside: Vec<_> = paths(true).into_iter().map(|(path, _)| path).collect();
        for git_dir in [main.join(".git/refs"), other.join(".git/refs")] {
            let git_dir = crate::path_utils::canonicalize(git_dir).unwrap();
            assert!(outside.contains(&git_dir), "{git_dir:?} in {outside:?}");
        }
    }

    // The outside stream restarts after the walk for a repository met then; what it may have
    // missed meanwhile is recovered before that returns.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_restarted_outside_stream_recovers_what_it_may_have_missed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = crate::path_utils::canonicalize(tmp.path()).unwrap();
        let (main, other) = (root.join("main"), root.join("other"));
        let repos = [&main, &other].map(|dir| {
            let repo = git2::Repository::init(dir).unwrap();
            std::fs::write(dir.join("file.txt"), "a").unwrap();
            commit(&repo);
            repo
        });
        let linked = root.join("linked");
        repos[0].worktree("linked", &linked, None).unwrap();
        repos[1]
            .worktree("nested", &linked.join("nested"), None)
            .unwrap();
        // A repository whose git dir, outside the index, has neither refs nor logs yet.
        let lone = root.join("lone.git");
        std::fs::create_dir_all(lone.join("objects")).unwrap();
        std::fs::write(lone.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir(linked.join("lone")).unwrap();
        let gitlink = format!("gitdir: {}\n", lone.display());
        std::fs::write(linked.join("lone/.git"), gitlink).unwrap();
        let (shared_picker, watcher) = native_picker(&linked);
        let (state_sender, state) = mpsc::channel::<PathBuf>();
        let _state = shared_picker
            .subscribe_repository_state(move |root| state_sender.send(root.to_path_buf()).unwrap());
        let (sender, heard) = mpsc::channel::<PathBuf>();
        let _refs = shared_picker.subscribe_ref_changes(&linked, move |root| {
            sender.send(root.to_path_buf()).unwrap()
        });
        let git_status = (shared_picker.read().unwrap().as_ref())
            .map(|picker| Arc::clone(&picker.git_status_worker))
            .unwrap();
        drain(&state);
        drain(&heard);
        let status_rescans = git_status.full_rescans_requested();

        std::fs::create_dir_all(lone.join("refs/heads")).unwrap();
        std::fs::create_dir(lone.join("logs")).unwrap();
        repos[1]
            .worktree("fresh", &linked.join("fresh"), None)
            .unwrap();
        (watcher.dir_hook().unwrap())(&linked.join("fresh"));

        let announced = drain(&state);
        for work_tree in [&linked, &linked.join("nested")] {
            assert!(
                announced.contains(work_tree),
                "{work_tree:?} in {announced:?}"
            );
        }
        let heard = drain(&heard);
        assert!(heard.contains(&linked), "{heard:?}");
        assert!(
            heard.contains(&linked.join("lone")),
            "its refs appeared: {heard:?}"
        );
        assert!(git_status.full_rescans_requested() > status_rescans);
        assert!(watcher.watches.refs.watched().contains(&lone.join("refs")));
        let guard = watcher.watches.streams.lock();
        let outside = guard.as_ref().unwrap().outside.as_ref().unwrap();
        let watched = outside.watched_paths().unwrap();
        assert!(
            watched.iter().any(|(path, _)| *path == lone.join("logs")),
            "{watched:?}"
        );
    }

    #[test]
    fn a_watch_update_goes_on_past_a_failed_path() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Config::default().with_poll_interval(Duration::from_secs(60));
        let debouncer = new_debouncer_opt::<_, notify::PollWatcher, _>(
            Duration::from_secs(60),
            None,
            |_: DebounceEventResult| {},
            NoCache::new(),
            config,
        )
        .unwrap();
        let mut debouncer = Debouncer::Poll(debouncer);
        let missing = tmp.path().join("missing");
        let restarted = debouncer.update(vec![
            PathOp::watch_recursive(&missing),
            PathOp::watch_non_recursive(tmp.path()),
            PathOp::unwatch(&missing),
        ]);
        assert!(!restarted, "polling restarts nothing");
        let watched = debouncer.watched_paths().unwrap();
        assert_eq!(
            watched,
            [(tmp.path().to_path_buf(), RecursiveMode::NonRecursive)]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn fsevents_restarts_its_stream_for_each_update_but_not_for_a_path_it_watches() {
        let tmp = tempfile::tempdir().unwrap();
        let debouncer = new_debouncer_opt::<_, notify::RecommendedWatcher, _>(
            Duration::from_secs(60),
            None,
            |_: DebounceEventResult| {},
            NoCache::new(),
            Config::default(),
        )
        .unwrap();
        let mut debouncer = Debouncer::Native(debouncer);
        let dir = tmp.path().to_path_buf();
        let sub = dir.join("sub");
        std::fs::create_dir(&sub).unwrap();
        let both = || {
            vec![
                PathOp::watch_non_recursive(&dir),
                PathOp::watch_recursive(&sub),
            ]
        };
        assert!(debouncer.update(both()), "one restart for both paths");
        assert!(!debouncer.update(both()), "nothing new to watch");
        assert!(debouncer.update(vec![PathOp::unwatch(&sub)]));
    }

    fn subscribed_picker(base: &Path) -> (SharedFilePicker, mpsc::Receiver<Vec<WatchEvent>>) {
        let shared_picker = SharedFilePicker::default();
        let mut picker = FilePicker::new(FilePickerOptions {
            base_path: base.to_string_lossy().into_owned(),
            watch: false,
            ..Default::default()
        })
        .unwrap();
        picker.collect_files().unwrap();
        shared_picker.rebase_watches(base);
        *shared_picker.write().unwrap() = Some(picker);

        let (sender, receiver) = mpsc::channel::<Vec<WatchEvent>>();
        shared_picker
            .watch_registry()
            .subscribe(
                base,
                "**",
                WatchOptions::default(),
                Box::new(move |_, events| sender.send(events.to_vec()).unwrap()),
            )
            .unwrap();
        (shared_picker, receiver)
    }

    // An index of `base` whose polling watcher sees no change made before it started.
    fn polled_picker(base: &Path) -> (SharedFilePicker, Option<PathBuf>) {
        poll_watcher_for(base);
        let shared_picker = SharedFilePicker::default();
        FilePicker::new_with_shared_state(
            shared_picker.clone(),
            SharedFrecency::noop(),
            FilePickerOptions {
                base_path: base.to_string_lossy().into_owned(),
                enable_content_indexing: false,
                watch: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(shared_picker.wait_for_scan(Duration::from_secs(30)));
        assert!(shared_picker.wait_for_watcher(Duration::from_secs(30)));
        let workdir = (shared_picker.read().unwrap().as_ref())
            .and_then(|picker| picker.git_root().map(Path::to_path_buf));
        (shared_picker, workdir)
    }

    // An index of `base` with the native watcher, and its watcher's shared state.
    #[cfg(target_os = "macos")]
    fn native_picker(base: &Path) -> (SharedFilePicker, WatcherHandle) {
        NATIVE_ROOTS.lock().push(base.to_path_buf());
        let shared_picker = SharedFilePicker::default();
        FilePicker::new_with_shared_state(
            shared_picker.clone(),
            SharedFrecency::noop(),
            FilePickerOptions {
                base_path: base.to_string_lossy().into_owned(),
                enable_content_indexing: false,
                watch: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(shared_picker.wait_for_scan(Duration::from_secs(30)));
        assert!(shared_picker.wait_for_watcher(Duration::from_secs(30)));
        let handle = (shared_picker.read().unwrap().as_ref())
            .and_then(|picker| picker.background_watcher.as_ref())
            .map(|watcher| WatcherHandle {
                watches: watcher.watches(),
                hook: watcher.dir_hook(),
            })
            .unwrap();
        (shared_picker, handle)
    }

    #[cfg(target_os = "macos")]
    struct WatcherHandle {
        watches: Arc<Watches>,
        hook: Option<DirHook>,
    }

    #[cfg(target_os = "macos")]
    impl WatcherHandle {
        fn dir_hook(&self) -> Option<DirHook> {
            self.hook.clone()
        }
    }

    fn modified(picker: &SharedFilePicker, base: &Path, workdir: &Option<PathBuf>, path: PathBuf) {
        let event = Event::new(EventKind::Modify(ModifyKind::Any)).add_path(path);
        handle_debounced_events(
            FFFMode::Neovim,
            vec![DebouncedEvent::new(event, Instant::now())],
            base,
            workdir,
            picker,
            &SharedFrecency::noop(),
            &GitStatusWorker::new(),
        );
    }

    // What a listener heard so far, in path order.
    fn drain(heard: &mpsc::Receiver<PathBuf>) -> Vec<PathBuf> {
        let mut heard: Vec<_> = heard.try_iter().collect();
        heard.sort();
        heard
    }

    fn commit(repo: &git2::Repository) {
        let signature = git2::Signature::now("fff", "fff@example.invalid").unwrap();
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        repo.commit(Some("HEAD"), &signature, &signature, "init", &tree, &[])
            .unwrap();
    }

    fn apply(shared_picker: &SharedFilePicker, base: &Path, kinds: Vec<EventKind>, path: &Path) {
        let now = Instant::now();
        let events = kinds
            .into_iter()
            .map(|kind| DebouncedEvent::new(Event::new(kind).add_path(path.to_path_buf()), now))
            .collect();
        handle_debounced_events(
            FFFMode::Neovim,
            events,
            base,
            &None,
            shared_picker,
            &SharedFrecency::noop(),
            &GitStatusWorker::new(),
        );
    }
}
