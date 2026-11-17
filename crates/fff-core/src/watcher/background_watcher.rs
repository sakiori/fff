use super::git_changes::{GitStateChange, announce_git_state};
use super::ref_watches::RefWatches;
use super::rescan_tap::{RescanLog, Tapped};
use crate::constants::MAX_OVERFLOW_FILES;
use crate::error::Error;
use crate::file_picker::{FFFMode, FilePicker, FilePickerIgnoreOptions};
use crate::scm::{ScmStatusWorker, ScmWorkspace};
use crate::shared::{SharedFilePicker, SharedFrecency};
use crate::walk::DirHook;
use notify::event::ModifyKind;
use notify::{Config, EventKind, EventKindMask, PathOp, RecursiveMode, Watcher, WatcherKind};
use notify_debouncer_full::{DebounceEventResult, DebouncedEvent, NoCache, new_debouncer_opt};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

// The OS-native backend; test and debug builds can poll instead (see `test_poll_interval`).
enum Debouncer {
    Native(notify_debouncer_full::Debouncer<Tapped<notify::RecommendedWatcher>, NoCache>),
    Poll(notify_debouncer_full::Debouncer<Tapped<notify::PollWatcher>, NoCache>),
}

impl Debouncer {
    fn watch(&mut self, path: &Path, mode: RecursiveMode) -> notify::Result<()> {
        #[cfg(test)]
        let _starting = StartGuard::begin(self);
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
        #[cfg(test)]
        let _starting = restarted.then(|| StartGuard::begin(self));
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

/// The index's file-system watcher: its owner thread alone starts, changes and stops its streams,
/// walks subscribing only their directories inline. Stopping it never waits.
pub struct BackgroundWatcher {
    watches: Arc<Watches>,
}

// What the watcher's threads share: the way to the owner thread and what it watches for.
pub(crate) struct Watches {
    // Closed when the watcher stops, so the owner thread ends and stops the streams.
    tasks: Mutex<Option<mpsc::Sender<WatchTask>>>,
    // Set when the watcher stops: its streams deliver nothing more, its queued tasks are dropped,
    // and no directory is subscribed any more.
    stopped: Arc<AtomicBool>,
    // The tree's stream where it watches each directory, which walks subscribe inline, as
    // Watchman's crawler adds its inotify watches; held for one subscription at a time.
    directories: Mutex<Option<Debouncer>>,
    recursive: bool,
    // FSEvents restarts a stream for every change, so nothing waits for one there.
    restarts: bool,
    pub(super) refs: RefWatches,
    // The git dirs this watcher watches, which its walks register: a new watcher starts with
    // none but the own one, so it watches every repository its walk meets.
    pub(crate) git_dirs: Arc<crate::git_dirs::GitDirs>,
    git_status: Arc<dyn ScmStatusWorker>,
    base_path: PathBuf,
    // Set when the index's first walk begins; what a stream misses before, the walk reads.
    walking: AtomicBool,
    // A rebuild's new watcher tells listeners of no repository until the rebuild announces
    // every one, so each is heard once.
    quiet: AtomicBool,
    // False for an index without Git integration, which follows no repository's state.
    follows_git: bool,
    // Test seam: tasks sent and not yet handled.
    #[cfg(test)]
    queued: AtomicUsize,
}

impl Watches {
    pub(super) fn send(&self, task: WatchTask) -> bool {
        #[cfg(test)]
        self.queued.fetch_add(1, Ordering::SeqCst);
        let sent = (self.tasks.lock().as_ref()).is_some_and(|tasks| tasks.send(task).is_ok());
        #[cfg(test)]
        if !sent {
            self.queued.fetch_sub(1, Ordering::SeqCst);
        }
        sent
    }

    // `announced`: the work trees the batch that saw the change announced.
    pub(super) fn request_git_state(&self, change: GitStateChange, announced: &[PathBuf]) -> bool {
        self.send(match change {
            GitStateChange::Repository(work_tree) => WatchTask::NewRepository(work_tree),
            GitStateChange::Log(git_dir) => WatchTask::NewGitLog(git_dir, announced.to_vec()),
            GitStateChange::Info(git_dir) => WatchTask::NewGitInfo(git_dir),
        })
    }

    // Whether the owner thread has a task to handle, as a stream restart it runs.
    #[cfg(test)]
    fn busy(&self) -> bool {
        self.queued.load(Ordering::SeqCst) > 0
    }

    // The tasks a leaving owner thread drops count as handled, and none is queued after.
    #[cfg(test)]
    fn drop_queued(&self, tasks: &mpsc::Receiver<WatchTask>) {
        self.tasks.lock().take();
        let dropped = tasks.try_iter().count();
        self.queued.fetch_sub(dropped, Ordering::SeqCst);
    }

    // Sends a task with a reply and waits for it; `None` once the watcher stopped.
    fn ask<R>(&self, task: impl FnOnce(mpsc::Sender<R>) -> WatchTask) -> Option<R> {
        let (reply, answer) = mpsc::channel();
        self.send(task(reply)).then(|| answer.recv().ok()).flatten()
    }

    // The paths a stream watches, as its owner thread reads them within a minute.
    #[cfg(all(test, target_os = "macos"))]
    fn watched_paths(&self, outside: bool) -> Vec<(PathBuf, RecursiveMode)> {
        let (reply, answer) = mpsc::channel();
        let sent = self.send(WatchTask::Watched { outside, reply });
        let answer = sent.then(|| answer.recv_timeout(Duration::from_secs(60)));
        answer.and_then(Result::ok).unwrap_or_default()
    }
}

// The tree's recursive stream, and on FSEvents one for paths outside it: a stream restarts from
// "now" on each change of its paths, so later changes go outside, where a restart loses no file.
struct Streams {
    // `None` where the tree is watched per directory: `Watches::directories` holds that stream.
    tree: Option<Debouncer>,
    outside: Option<Debouncer>,
}

// The owner thread, the one actor that holds the streams: every change of what they watch
// reaches it as a task, in order.
struct Owner {
    watches: Arc<Watches>,
    streams: Streams,
    picker: crate::shared::WeakFilePicker,
    own_work_tree: Option<PathBuf>,
    lost: Lost,
    // The work trees the task in hand found for listeners, each told once when it ends.
    told: Told,
}

#[derive(Default)]
struct Told {
    repositories: Vec<PathBuf>,
    refs: Vec<PathBuf>,
}

impl Owner {
    // Until the watcher stops: the tasks still queued then are dropped, and the streams stop.
    fn run(mut self, tasks: mpsc::Receiver<WatchTask>) {
        while let Ok(task) = tasks.recv() {
            // Once the watcher stopped or its picker was dropped, the thread leaves.
            let Some(picker) = self.live().then(|| self.picker.upgrade()).flatten() else {
                #[cfg(test)]
                self.watches.queued.fetch_sub(1, Ordering::SeqCst);
                break;
            };
            #[cfg(test)]
            let number = handled(&self.watches, &task);
            #[cfg(test)]
            TASK.with(|task| task.set(number));
            // A task that panics leaves what the streams watch unknown, as the index's loop
            // treats a turn: the watcher stops, and the index is rebuilt with a new one.
            let handled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                #[cfg(test)]
                panicking_task(&self.watches.base_path);
                self.handle(&picker, task);
                self.tell(&picker);
            }));
            #[cfg(test)]
            TASK.with(|task| task.set(0));
            #[cfg(test)]
            self.watches.queued.fetch_sub(1, Ordering::SeqCst);
            if handled.is_err() {
                error!("A task of the watcher's thread panicked; rebuilding with a new watcher");
                self.watches.stopped.store(true, Ordering::Release);
                (self.lost)();
                break;
            }
        }
        #[cfg(test)]
        self.watches.drop_queued(&tasks);
        let directories = self.watches.directories.lock().take();
        let streams = [
            self.streams.tree.take(),
            self.streams.outside.take(),
            directories,
        ];
        streams
            .into_iter()
            .flatten()
            .for_each(Debouncer::stop_nonblocking);
    }

    // Whether the watcher still runs: a task in flight when it stops tells nobody of it.
    fn live(&self) -> bool {
        !self.watches.stopped.load(Ordering::Acquire)
    }

    // Whether listeners hear of the git state this task found.
    fn tells(&self) -> bool {
        self.live() && !self.watches.quiet.load(Ordering::Acquire)
    }

    // Watches a directory of the tree on its own where the tree is watched per directory.
    fn watch_directory(&mut self, dir: &Path) {
        if let Some(stream) = self.watches.directories.lock().as_mut() {
            let _ = stream.watch(dir, RecursiveMode::NonRecursive);
        }
    }

    fn handle(&mut self, picker: &SharedFilePicker, task: WatchTask) {
        let covered =
            |dir: &Path| self.watches.recursive && dir.starts_with(&self.watches.base_path);
        match task {
            WatchTask::NewRepository(work_tree) => {
                self.watch_directory(&work_tree);
                if self.watches.follows_git
                    && let Some(git_dir) = git_dir_of(&work_tree)
                    && self.watches.git_dirs.register(&git_dir, &work_tree)
                {
                    self.watch_repository(picker, &work_tree, &git_dir);
                }
            }
            WatchTask::WatchRepository {
                work_tree,
                git_dir,
                done,
            } => {
                self.watch_repository(picker, &work_tree, &git_dir);
                if let Some(done) = done {
                    let _ = done.send(());
                }
            }
            WatchTask::NewGitLog(git_dir, announced) => {
                let covered = covered(&git_dir);
                self.watch_new_log(picker, &git_dir, covered, &announced);
            }
            WatchTask::NewGitInfo(git_dir) => {
                let covered = covered(&git_dir);
                self.watch_new_info(picker, &git_dir, covered);
            }
            WatchTask::SyncRefs(told) => {
                self.watches.refs.reread_storages();
                self.sync_refs(picker, told);
            }
            WatchTask::FollowRefs(done) => {
                self.sync_refs(picker, Vec::new());
                if let Some(done) = done {
                    let _ = done.send(());
                }
            }
            // After every repository the rebuild's walk met, which went before it.
            WatchTask::Announce => {
                self.watches.quiet.store(false, Ordering::Release);
                if let Some(options) =
                    (picker.read().ok()).and_then(|guard| Some(WatcherOptions::of(guard.as_ref()?)))
                {
                    let workdir = (picker.read().ok())
                        .and_then(|guard| Some(guard.as_ref()?.git_root()?.to_path_buf()));
                    let (base_path, git_status) =
                        (&self.watches.base_path, &self.watches.git_status);
                    let git_status = git_status.as_ref();
                    announce_git_state(picker, base_path, &workdir, &options, git_status, None);
                }
            }
            #[cfg(all(test, target_os = "macos"))]
            WatchTask::Watched { outside, reply } => {
                let directories = self.watches.directories.lock();
                let stream = match outside {
                    true => self.streams.outside.as_ref(),
                    false => self.streams.tree.as_ref().or(directories.as_ref()),
                };
                let mut paths =
                    (stream.and_then(|stream| stream.watched_paths().ok())).unwrap_or_default();
                paths.sort();
                let _ = reply.send(paths);
            }
        }
    }

    // Applies `ops` in one update and returns whether the git state of the repositories
    // outside the tree must be announced again, as a stream restarted after the walk began.
    fn update(&mut self, ops: Vec<PathOp>) -> bool {
        let restarted = match (self.streams.outside.as_mut(), self.streams.tree.as_mut()) {
            (Some(outside), _) => outside.update(ops),
            (None, Some(tree)) => tree.update(ops),
            (None, None) => (self.watches.directories.lock().as_mut())
                .is_some_and(|directories| directories.update(ops)),
        };
        #[cfg(test)]
        if restarted {
            started_stream(&self.watches.base_path);
        }
        restarted && self.watches.walking.load(Ordering::Acquire)
    }

    // Watches a walked repository's git state unless covered, its followed refs in the same update,
    // and announces it, as `git init` to `git add` outrun a walk; a first scan has none to hear it.
    fn watch_repository(&mut self, picker: &SharedFilePicker, work_tree: &Path, git_dir: &Path) {
        let watches = Arc::clone(&self.watches);
        let mut ops = Vec::new();
        if !watches.recursive || !git_dir.starts_with(&watches.base_path) {
            ops.extend(git_dir_watches(git_dir));
        }
        // A repository created again lost the watches of its refs; a linked worktree shares
        // its main repository's.
        let renew = (super::common_dir_of(git_dir) == git_dir).then_some(git_dir);
        let refs = watches.refs.plan(picker, renew);
        ops.extend(refs.ops);
        if self.update(ops) {
            self.recover_outside(picker);
        }
        self.told.refs.extend(refs.appeared);
        self.told.repositories.push(work_tree.to_path_buf());
    }

    // What changed outside the tree while its stream restarted is lost: the own git status is
    // read again, ref stores and logs that appeared get their watches, and the repositories
    // whose git dir or common dir lies outside the tree are announced as changed.
    fn recover_outside(&mut self, picker: &SharedFilePicker) {
        let watches = Arc::clone(&self.watches);
        let outside = |path: &Path| !path.starts_with(&watches.base_path);
        let registered = watches.git_dirs.all();
        let mut appeared = Vec::new();
        // Each update adds only paths not watched yet, so the candidates bound the rounds.
        let mut rounds = 0;
        let mut bound = None;
        loop {
            let plan = watches.refs.plan(picker, None);
            let git_dirs = (registered.iter())
                .filter(|known| known.work_tree.starts_with(&watches.base_path))
                .map(|known| known.git_dir.clone())
                .chain(watches.refs.own_git_dir());
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
            if !self.update(ops) {
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
        if watches.refs.own_git_dir_outside() {
            watches.git_status.request_full_rescan();
        }
        let mut work_trees: Vec<PathBuf> = (registered.into_iter())
            .filter(|known| {
                known.work_tree.starts_with(&watches.base_path)
                    && (outside(&known.git_dir) || outside(&known.common_dir))
            })
            .map(|known| known.work_tree.components().collect())
            .collect();
        work_trees.extend(watches.refs.own_work_tree_outside());
        self.told.repositories.extend(work_trees.iter().cloned());
        self.told
            .refs
            .extend(appeared.into_iter().chain(work_trees));
    }

    // Brings the ref watches in line with the ref listeners, which hear of each work tree whose
    // refs appeared or that `told` names.
    fn sync_refs(&mut self, picker: &SharedFilePicker, told: Vec<PathBuf>) {
        let plan = self.watches.refs.plan(picker, None);
        if self.update(plan.ops) {
            self.recover_outside(picker);
        }
        self.told.refs.extend(told.into_iter().chain(plan.appeared));
    }

    // Tells listeners once of each work tree the task found, unless the watcher stopped or is
    // quiet: a stream restart within the task finds some of them again.
    fn tell(&mut self, picker: &SharedFilePicker) {
        let Told { repositories, refs } = std::mem::take(&mut self.told);
        if !self.tells() {
            return;
        }
        for work_tree in once_each(repositories) {
            picker.notify_repository_change(&work_tree);
        }
        for work_tree in once_each(refs) {
            picker.notify_ref_change(&work_tree);
        }
    }

    // Watches the log a git dir gained unless the tree's watch covers it, and announces its
    // repository, whose HEAD moved unwatched, unless covered and its batch announced it already.
    fn watch_new_log(
        &mut self,
        picker: &SharedFilePicker,
        git_dir: &Path,
        covered: bool,
        announced: &[PathBuf],
    ) {
        let canonical = |path: &Path| crate::path_utils::canonicalize(path).ok();
        let own = (self.own_work_tree.as_ref())
            .filter(|_| {
                (self.watches.refs.own_git_dir())
                    .is_some_and(|own| canonical(&own) == canonical(git_dir))
            })
            .cloned();
        let work_tree = own.or_else(|| {
            (self.watches.git_dirs.holding(&git_dir.join("logs")))
                .filter(|(registered, _)| registered == git_dir)
                .map(|(_, work_tree)| work_tree)
        });
        let Some(work_tree) = work_tree else {
            return;
        };
        let logs = PathOp::watch_non_recursive(git_dir.join("logs"));
        if !covered && self.update(vec![logs]) {
            self.recover_outside(picker);
        }
        if !(covered && announced.contains(&work_tree)) {
            self.told.repositories.push(work_tree);
        }
    }

    // Watches a git dir's `info` that appeared after the git dir was watched; its exclude file,
    // maybe written before the watch, is checked as if changed, which applies its rules.
    fn watch_new_info(&mut self, picker: &SharedFilePicker, git_dir: &Path, covered: bool) {
        let info = git_dir.join("info");
        if !covered && self.update(vec![PathOp::watch_non_recursive(info.clone())]) {
            self.recover_outside(picker);
        }
        let exclude = info.join("exclude");
        if self.live() && exclude.is_file() {
            let event = notify::Event::new(EventKind::Modify(ModifyKind::Any)).add_path(exclude);
            let event = DebouncedEvent::new(event, std::time::Instant::now());
            let _ = picker.submit_events(None, vec![event]);
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct WatcherOptions {
    pub(crate) mode: FFFMode,
    pub(crate) follow_symlinks: bool,
    pub(crate) ignore_options: FilePickerIgnoreOptions,
    pub(crate) enable_fs_root_scanning: bool,
    pub(crate) enable_home_dir_scanning: bool,
}

impl WatcherOptions {
    pub(crate) fn of(picker: &FilePicker) -> Self {
        Self {
            mode: picker.mode(),
            follow_symlinks: picker.follows_symlinks(),
            ignore_options: picker.ignore_options().clone(),
            enable_fs_root_scanning: picker.fs_root_scanning_enabled(),
            enable_home_dir_scanning: picker.home_dir_scanning_enabled(),
        }
    }
}

pub(super) enum WatchTask {
    /// A `.git` appeared in this directory, which the walk had already reached.
    NewRepository(PathBuf),
    /// A walk met the repository of this work tree and registered this git dir.
    WatchRepository {
        work_tree: PathBuf,
        git_dir: PathBuf,
        done: Option<mpsc::Sender<()>>,
    },
    /// This git dir gained its `logs`, in a batch that announced these work trees.
    NewGitLog(PathBuf, Vec<PathBuf>),
    /// This git dir gained its `info`, which holds its exclude file.
    NewGitInfo(PathBuf),
    /// A ref store or its format may have changed: the ref watches follow, and ref listeners
    /// hear of these work trees with those whose refs appeared.
    SyncRefs(Vec<PathBuf>),
    /// The ref listeners or the git dirs they may follow changed: the ref watches follow, then
    /// `done` hears.
    FollowRefs(Option<mpsc::Sender<()>>),
    /// The rebuild that installed this watcher is published: listeners hear of every repository.
    Announce,
    #[cfg(all(test, target_os = "macos"))]
    Watched {
        outside: bool,
        reply: mpsc::Sender<Vec<(PathBuf, RecursiveMode)>>,
    },
}

const DEBOUNCE_TIMEOUT: Duration = Duration::from_millis(50);
/// More affected paths than this in one batch aggregate into the directories they changed.
pub(crate) const MAX_BATCH_PATHS: usize = MAX_OVERFLOW_FILES * 4;
// Per-directory subscriptions stop after this many failures in a row.
const MAX_CONSECUTIVE_WATCH_FAILURES: usize = 16;

// The loop hears from the owner thread once the watcher is installed, or could not be, and
// once it was lost to a task that panicked.
pub(crate) type Installed = Box<dyn FnOnce(Result<BackgroundWatcher, Error>) + Send>;
pub(crate) type Lost = Box<dyn Fn() + Send>;

impl BackgroundWatcher {
    // Starts the owner thread, which creates the streams and hands the watcher to `installed`;
    // false when the thread cannot start.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start(
        base_path: PathBuf,
        git: Option<ScmWorkspace>,
        shared_picker: &SharedFilePicker,
        options: WatcherOptions,
        scm_status_worker: Arc<dyn ScmStatusWorker>,
        trace_span: tracing::Span,
        quiet: bool,
        installed: Installed,
        lost: Lost,
    ) -> bool {
        let picker = shared_picker.weaken();
        let spawned = std::thread::Builder::new()
            .name("fff-watcher-own".into())
            .spawn(move || {
                #[cfg(test)]
                OWNER_ROOT.with(|root| *root.borrow_mut() = Some(base_path.clone()));
                // The run loops of its streams take its priority, the scans' pool's.
                crate::parallelism::pin_user_initiated();
                let _g = trace_span.enter();
                let Some(shared_picker) = picker.upgrade() else {
                    return;
                };
                let git_workdir = git.as_ref().map(|git| git.workdir.clone());
                let (tasks, task_receiver) = mpsc::channel::<WatchTask>();
                // A setup that panics fails as any other, so the initial scan goes on without it.
                let created = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    Self::create(
                        &base_path,
                        git.as_ref(),
                        &shared_picker,
                        &options,
                        scm_status_worker,
                        tasks,
                        quiet,
                    )
                }))
                .unwrap_or_else(|_| {
                    error!("Setting up the watcher panicked");
                    Err(Error::ThreadPanic)
                });
                match created {
                    Ok((watcher, streams)) => {
                        let owner = Owner {
                            watches: watcher.watches(),
                            streams,
                            picker: shared_picker.weaken(),
                            own_work_tree: git_workdir,
                            lost,
                            told: Told::default(),
                        };
                        drop(shared_picker);
                        installed(Ok(watcher));
                        owner.run(task_receiver);
                    }
                    Err(e) => {
                        drop(shared_picker);
                        installed(Err(e));
                    }
                }
                info!("Background watcher is stopped");
            });
        spawned
            .inspect_err(|e| error!(?e, "Failed to start the watcher's thread"))
            .is_ok()
    }

    fn create(
        base_path: &Path,
        git: Option<&ScmWorkspace>,
        shared_picker: &SharedFilePicker,
        options: &WatcherOptions,
        scm_status_worker: Arc<dyn ScmStatusWorker>,
        tasks: mpsc::Sender<WatchTask>,
        quiet: bool,
    ) -> Result<(Self, Streams), Error> {
        info!(
            "Initializing background watcher for path: {}, mode: {:?}",
            base_path.display(),
            options.mode,
        );

        // Root and home watching are error prone; consumers must opt in explicitly.
        crate::path_utils::validate_base_path(
            base_path,
            options.enable_fs_root_scanning,
            options.enable_home_dir_scanning,
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
        let use_recursive = recursive_watch(base_path);

        let poll_interval = test_poll_interval(base_path);
        let fsevents = poll_interval.is_none()
            && <notify::RecommendedWatcher as Watcher>::kind() == WatcherKind::Fsevent;
        // Removing a watch restarts the FSEvents stream, so ref watches stay once added there.
        let follows_git = !options.ignore_options.disable_git;
        let refs = RefWatches::new(base_path, use_recursive, git, fsevents, follows_git);
        let git_workdir = git.map(|git| git.workdir.as_path());
        // The own git dir is watched from the start, so a walk reaching the work tree finds it
        // known, and watches it anew only once it was created again.
        let git_dirs = Arc::new(crate::git_dirs::GitDirs::default());
        if let Some(workdir) = git_workdir
            && let Some(git_dir) = git_dir_of(workdir)
        {
            git_dirs.register(&git_dir, workdir);
        }

        let stopped = Arc::new(AtomicBool::new(false));
        let mut tree = match poll_interval {
            None => Debouncer::Native(Self::create_debouncer(shared_picker, &stopped, None)?),
            Some(interval) => {
                info!(?interval, "File watcher uses the polling backend");
                Debouncer::Poll(Self::create_debouncer(
                    shared_picker,
                    &stopped,
                    Some(interval),
                )?)
            }
        };
        let mut outside = match fsevents {
            true => Some(Debouncer::Native(Self::create_debouncer(
                shared_picker,
                &stopped,
                None,
            )?)),
            false => None,
        };

        if use_recursive {
            tree.watch(base_path, RecursiveMode::Recursive)?;
            info!(
                "File watcher initialized with single recursive watch on {}",
                base_path.display(),
            );
        } else {
            // Scans subscribe every directory they walk through `dir_hook`.
            tree.watch(base_path, RecursiveMode::NonRecursive)?;
            info!(
                "File watcher initialized with per-directory watches on {}",
                base_path.display(),
            );
        }
        // The .git directory is excluded from the file list but we still need to observe
        // changes that affect git status (staging, committing, branch switches, merges, etc).
        // The own git dir may lie outside the tree, as a linked worktree's does.
        let own_git = git
            .map(|git| {
                let mut watches: Vec<_> = git_dir_watches(&git.metadata).collect();
                // A linked worktree's `info`, created later in its main repository's git dir,
                // appears there: that git dir is watched too, ref listeners or not.
                let common_dir = super::common_dir_of(&git.metadata);
                if common_dir.components().ne(git.metadata.components()) {
                    watches.push(PathOp::watch_non_recursive(common_dir));
                }
                watches
            })
            .unwrap_or_default();
        match outside.as_mut() {
            Some(outside) => {
                let (inside, mut beyond): (Vec<_>, Vec<_>) =
                    (own_git.into_iter()).partition(|op| op.as_path().starts_with(base_path));
                tree.update(inside);
                beyond.extend(refs.own_targets(shared_picker));
                outside.update(beyond);
            }
            None => {
                tree.update(own_git);
            }
        }
        #[cfg(test)]
        started_stream(base_path);

        info!("Background file watcher initialized successfully");

        let (tree, directories) = match use_recursive {
            true => (Some(tree), None),
            false => (None, Some(tree)),
        };
        let streams = Streams { tree, outside };
        let watches = Arc::new(Watches {
            tasks: Mutex::new(Some(tasks)),
            stopped,
            directories: Mutex::new(directories),
            recursive: use_recursive,
            restarts: fsevents,
            refs,
            git_dirs,
            git_status: scm_status_worker,
            base_path: base_path.to_path_buf(),
            walking: AtomicBool::new(false),
            quiet: AtomicBool::new(quiet),
            follows_git,
            #[cfg(test)]
            queued: AtomicUsize::new(0),
        });
        Ok((Self { watches }, streams))
    }

    fn create_debouncer<W: notify::Watcher>(
        shared_picker: &SharedFilePicker,
        stopped: &Arc<AtomicBool>,
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

        let file_picker = shared_picker.weaken();
        let stopped = Arc::clone(stopped);
        // The debouncer keeps one pending recrawl request: the tap keeps every one.
        let rescans = RescanLog::default();
        let installed = rescans.install_for_next_watcher();
        let debouncer = new_debouncer_opt(
            DEBOUNCE_TIMEOUT,
            Some(DEBOUNCE_TIMEOUT / 2), // tick rate for the event span
            move |result: DebounceEventResult| match result {
                Ok(mut events) => {
                    rescans.merge_into(&mut events);
                    if stopped.load(Ordering::Acquire) {
                        return;
                    }
                    let Some(file_picker) = file_picker.upgrade() else {
                        return;
                    };
                    // Held for a running scan, or applied on the index's writer. Waiting for it
                    // is the backpressure: the debouncer merges what arrives meanwhile.
                    let submitted = file_picker.submit_events(None, events);
                    drop(file_picker);
                    if let Some(done) = submitted {
                        let _ = done.recv();
                    }
                }
                Err(errors) => {
                    error!("File watcher errors: {:?}", errors);
                }
            },
            // There is an issue with recommended cache implementation on macos
            // it keeps track of all the files added to the watcher which is not a problem
            // for us because any rename to the file will anyway require the removing from the
            // ordedred index and adding it back with the new name
            NoCache::new(),
            config,
        );
        drop(installed);

        Ok(debouncer?)
    }

    /// Signal the watcher to shut down without blocking on its worker
    /// threads. Safe to call from any context, including while holding
    /// the [`SharedFilePicker`] write lock.
    pub fn stop(&mut self) {
        self.watches.stopped.store(true, Ordering::Release);
        // Closing the owner thread's queue wakes it, and it stops the streams.
        self.watches.tasks.lock().take();
        info!("Background file watcher stop signaled");
    }

    /// Runs for each directory a walk reaches: subscribes it unless one recursive watch
    /// covers the tree, and watches the git state of a repository it holds.
    pub(crate) fn dir_hook(&self) -> Option<DirHook> {
        Some(DirSubscriptions::hook(&self.watches))
    }

    // Test seam: a watcher with no thread, no stream and no git watches of its walks, which
    // holds a registry of git dirs.
    #[cfg(test)]
    pub(crate) fn unstarted(base_path: &Path) -> Self {
        let watches = Watches {
            tasks: Mutex::new(None),
            stopped: Arc::default(),
            directories: Mutex::new(None),
            recursive: false,
            restarts: false,
            refs: RefWatches::new(base_path, false, None, false, true),
            git_dirs: Arc::default(),
            git_status: crate::scm::ScmBackend::configured().create_worker(Arc::default()),
            base_path: base_path.to_path_buf(),
            walking: AtomicBool::new(false),
            quiet: AtomicBool::new(false),
            follows_git: false,
            #[cfg(test)]
            queued: AtomicUsize::new(0),
        };
        Self {
            watches: Arc::new(watches),
        }
    }

    // Has listeners hear of every repository the watcher follows, after what it was asked before.
    pub(crate) fn announce(&self) {
        self.send(WatchTask::Announce);
    }

    // From now on a stream that restarts misses what the walk cannot read for it.
    pub(crate) fn walk_begins(&self) {
        self.watches.walking.store(true, Ordering::Release);
    }

    // Changes the watched paths off the picker lock, as the ref listeners need.
    pub(crate) fn watches(&self) -> Arc<Watches> {
        Arc::clone(&self.watches)
    }

    pub(super) fn send(&self, task: WatchTask) -> bool {
        self.watches.send(task)
    }

    // Has the owner thread bring the ref watches in line with the ref listeners; `done` hears.
    pub(crate) fn follow_refs(&self, done: mpsc::Sender<()>) -> bool {
        self.send(WatchTask::FollowRefs(Some(done)))
    }
}

impl Drop for BackgroundWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

// Applies changes to `paths` reported by a program exactly as watcher events for them, on the
// index's loop in order with the watcher's. Returns false when a scan holds them.
pub(crate) fn apply_reported_changes(
    shared_picker: &SharedFilePicker,
    shared_frecency: SharedFrecency,
    paths: Vec<PathBuf>,
) -> bool {
    let now = std::time::Instant::now();
    let events = (paths.into_iter())
        .map(|path| {
            let event = notify::Event::new(EventKind::Modify(ModifyKind::Any)).add_path(path);
            DebouncedEvent::new(event, now)
        })
        .collect();
    match shared_picker.submit_events(Some(shared_frecency), events) {
        None => true,
        // Dropped unapplied when the index was replaced meanwhile.
        Some(done) => done.recv().unwrap_or(false),
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

// Test seam: streams of a root start this much later, as FSEvents' when its daemon is slow.
#[cfg(test)]
static SLOW_STARTS: Mutex<Vec<(PathBuf, Duration)>> = Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn slow_stream_starts_for(root: &Path, delay: Duration) {
    SLOW_STARTS.lock().push((root.to_path_buf(), delay));
}

// Test seam: when each start of a root's native streams by its owner thread began and ended. The
// time inside is the backend's: FSEvents' daemon registers concurrent streams one at a time.
#[cfg(test)]
static STARTS: Mutex<Vec<Starting>> = Mutex::new(Vec::new());

#[cfg(test)]
struct Starting {
    root: PathBuf,
    began: std::time::Instant,
    ended: Option<std::time::Instant>,
}

#[cfg(test)]
thread_local! {
    // The root whose streams this thread, an owner thread, starts.
    static OWNER_ROOT: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

// Ends the start it names when dropped, also on a way out by an error.
#[cfg(test)]
struct StartGuard(Option<usize>);

#[cfg(test)]
impl StartGuard {
    // Times a start or restart of a native stream on an owner thread, and nothing elsewhere.
    fn begin(debouncer: &Debouncer) -> Self {
        let root = OWNER_ROOT.with(|root| root.borrow().clone());
        let Some(root) = root.filter(|_| matches!(debouncer, Debouncer::Native(_))) else {
            return Self(None);
        };
        let mut starts = STARTS.lock();
        starts.push(Starting {
            root,
            began: std::time::Instant::now(),
            ended: None,
        });
        Self(Some(starts.len() - 1))
    }
}

#[cfg(test)]
impl Drop for StartGuard {
    fn drop(&mut self) {
        if let Some(at) = self.0 {
            STARTS.lock()[at].ended = Some(std::time::Instant::now());
        }
    }
}

// How many starts of a root's native streams began.
#[cfg(test)]
pub(crate) fn stream_starts(root: &Path) -> usize {
    (STARTS.lock().iter())
        .filter(|start| start.root == root)
        .count()
}

// How many starts of a root's native streams began at or after `from`.
#[cfg(all(test, target_os = "macos"))]
pub(crate) fn stream_starts_since(root: &Path, from: std::time::Instant) -> usize {
    (STARTS.lock().iter())
        .filter(|start| start.root == root && start.began >= from)
        .count()
}

// Whether `done` holds within `limit` of the product's time past `from`. The backend's time, up to
// 300 s, is that of a start under way at `from` and of the first `starts` the product needs after.
#[cfg(test)]
pub(crate) fn within_product_time(
    root: &Path,
    from: std::time::Instant,
    limit: Duration,
    starts: usize,
    mut done: impl FnMut() -> bool,
) -> bool {
    let backend_limit = Duration::from_secs(300);
    loop {
        if done() {
            return true;
        }
        let now = std::time::Instant::now();
        let backend: Duration = {
            let all = STARTS.lock();
            let running = |start: &&Starting| start.ended.is_none_or(|ended| ended > from);
            let mine = (all.iter())
                .filter(|start| start.root == root)
                .filter(running);
            let (under_way, begun): (Vec<_>, Vec<_>) = mine.partition(|start| start.began < from);
            (under_way.into_iter().chain(begun.into_iter().take(starts)))
                .map(|start| {
                    let (began, ended) = (start.began.max(from), start.ended.unwrap_or(now));
                    ended.saturating_duration_since(began)
                })
                .sum()
        };
        assert!(
            backend < backend_limit,
            "the backend spent {backend:?} starting the streams of {}",
            root.display()
        );
        if now.saturating_duration_since(from) >= limit + backend {
            return done();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// Test seam: each path the native streams delivered, when the backend called back with it.
#[cfg(all(test, target_os = "macos"))]
static DELIVERED: Mutex<Vec<Delivered>> = Mutex::new(Vec::new());

#[cfg(all(test, target_os = "macos"))]
struct Delivered {
    path: PathBuf,
    at: std::time::Instant,
    rescan: bool,
}

#[cfg(all(test, target_os = "macos"))]
pub(super) fn record_delivery(event: &notify::Event) {
    let (at, rescan) = (std::time::Instant::now(), event.need_rescan());
    let paths = (event.paths.iter()).map(|path| Delivered {
        path: path.clone(),
        at,
        rescan,
    });
    DELIVERED.lock().extend(paths);
}

// Test seam: paths at or below these never reach the index from a stream, as when FSEvents loses
// their events; whether anything is left of an event.
#[cfg(test)]
static UNDELIVERED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

#[cfg(test)]
pub(super) fn deliverable(event: &mut notify::Event) -> bool {
    let undelivered = UNDELIVERED.lock();
    let had = !event.paths.is_empty();
    (event.paths).retain(|path| !undelivered.iter().any(|hidden| path.starts_with(hidden)));
    !had || !event.paths.is_empty()
}

// Test seam: a stream of the test's own on a root, in a child process of the test binary, started
// before the change and never restarted: it tells a loss by FSEvents from one by this process.
#[cfg(all(test, target_os = "macos"))]
pub(crate) struct Control {
    child: std::process::Child,
    seen: PathBuf,
    _dir: tempfile::TempDir,
}

#[cfg(all(test, target_os = "macos"))]
const CONTROL_ROOT: &str = "FFF_TEST_CONTROL_ROOT";
#[cfg(all(test, target_os = "macos"))]
const CONTROL_SEEN: &str = "FFF_TEST_CONTROL_SEEN";
#[cfg(all(test, target_os = "macos"))]
const CONTROL_READY: &str = "\0ready";

#[cfg(all(test, target_os = "macos"))]
impl Control {
    // Starts the child and waits until its stream runs, as long as a stream of ours may take.
    fn on(root: &Path) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let seen = dir.path().join("seen");
        std::fs::write(&seen, "").unwrap();
        let module = module_path!()
            .split_once("::")
            .map_or(module_path!(), |(_, path)| path);
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("{module}::tests::fsevents_control"),
                "--nocapture",
            ])
            .env(CONTROL_ROOT, root)
            .env(CONTROL_SEEN, &seen)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let control = Self {
            child,
            seen,
            _dir: dir,
        };
        let (limit, begun) = (Duration::from_secs(300), std::time::Instant::now());
        while !control.lines().iter().any(|line| line == CONTROL_READY) {
            assert!(
                begun.elapsed() < limit,
                "the control's stream did not start within {limit:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        control
    }

    fn lines(&self) -> Vec<String> {
        let read = std::fs::read_to_string(&self.seen).unwrap_or_default();
        read.lines().map(str::to_owned).collect()
    }

    fn missed(&self, matches: impl Fn(&Path) -> bool, what: String) -> Missed {
        let lines = self.lines();
        let seen: Vec<_> = (lines.iter())
            .filter(|line| *line != CONTROL_READY)
            .map(Path::new)
            .collect();
        let by_all = !seen.iter().any(|path| matches(path));
        let whose = match by_all {
            false => "received it: the product's streams did not",
            true => "received none of it either: FSEvents lost or withheld it",
        };
        let said = format!(
            "{what}; a stream of the test's own on the root {whose} ({} paths in all)",
            seen.len()
        );
        Missed { by_all, said }
    }
}

#[cfg(all(test, target_os = "macos"))]
impl Drop for Control {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// The child's side of `Control`: lists each path its stream receives until the test ends it, or
// its parent is gone.
#[cfg(all(test, target_os = "macos"))]
fn run_control(root: &Path, seen: &Path) {
    use std::io::Write;
    let parent = std::os::unix::process::parent_id();
    let file = Mutex::new(std::fs::OpenOptions::new().append(true).open(seen).unwrap());
    let into = Arc::new(file);
    let out = Arc::clone(&into);
    let handler = move |event: notify::Result<notify::Event>| {
        if let Ok(event) = event {
            let mut file = out.lock();
            for path in event.paths {
                let _ = writeln!(file, "{}", path.display());
            }
        }
    };
    let mut watcher = notify::RecommendedWatcher::new(handler, Config::default()).unwrap();
    watcher.watch(root, RecursiveMode::Recursive).unwrap();
    writeln!(into.lock(), "{CONTROL_READY}").unwrap();
    while std::os::unix::process::parent_id() == parent {
        std::thread::sleep(Duration::from_millis(100));
    }
}

// A live delivery the product's streams did not get, and whether the control did not either.
#[cfg(all(test, target_os = "macos"))]
pub(crate) struct Missed {
    by_all: bool,
    said: String,
}

// Test seam: when a native stream first delivered a path `matches` takes, waited for long, as
// FSEvents can withhold deliveries for minutes; a failure names what it delivered below `root`.
#[cfg(all(test, target_os = "macos"))]
pub(crate) fn delivered(
    root: &Path,
    what: &str,
    matches: impl Fn(&Path) -> bool,
    control: &Control,
) -> Result<std::time::Instant, Missed> {
    let limit = Duration::from_secs(300);
    let begun = std::time::Instant::now();
    loop {
        let delivered = DELIVERED.lock();
        let first = (delivered.iter()).find(|delivered| matches(&delivered.path));
        if let Some(first) = first {
            return Ok(first.at);
        }
        if begun.elapsed() >= limit {
            let meanwhile: Vec<_> = (delivered.iter())
                .filter(|delivered| delivered.at >= begun && delivered.path.starts_with(root))
                .map(|delivered| (&delivered.path, delivered.rescan))
                .collect();
            let elsewhere = (delivered.iter()).filter(|delivered| delivered.at >= begun);
            let what = format!("FSEvents delivered nothing of {what} within {limit:?}");
            let mut missed = control.missed(&matches, what);
            let below = format!(
                "; below the root {meanwhile:?}, {} paths in all",
                elsewhere.count()
            );
            missed.said.push_str(&below);
            return Err(missed);
        }
        drop(delivered);
        std::thread::sleep(Duration::from_millis(20));
    }
}

// Test seam: waits until a native stream of `root` delivers, writing a new file into it each second,
// as FSEvents can withhold or lose deliveries for minutes; the control then watches what follows.
#[cfg(all(test, target_os = "macos"))]
pub(crate) fn until_delivering(root: &Path) -> Control {
    let control = Control::on(root);
    let limit = Duration::from_secs(300);
    let canary = |path: &Path| {
        path.parent() == Some(root)
            && (path.file_name()).is_some_and(|name| name.to_string_lossy().starts_with(".canary-"))
    };
    let mut written = 0;
    retried_while_lost(|_| {
        let begun = std::time::Instant::now();
        while begun.elapsed() < limit {
            std::fs::write(root.join(format!(".canary-{written}")), "").unwrap();
            written += 1;
            let at = std::time::Instant::now();
            while at.elapsed() < Duration::from_secs(1) {
                if DELIVERED
                    .lock()
                    .iter()
                    .any(|delivered| canary(&delivered.path))
                {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let what = format!(
            "FSEvents delivered nothing to the streams of {}",
            root.display()
        );
        Err(control.missed(canary, format!("{what} within {limit:?}")))
    });
    control
}

// Runs `attempt` again, up to three times in all, while FSEvents lost its delivery for every
// client, as it does under load; a delivery only the product's streams missed fails at once.
#[cfg(all(test, target_os = "macos"))]
pub(crate) fn retried_while_lost<T>(mut attempt: impl FnMut(usize) -> Result<T, Missed>) -> T {
    let mut lost = Vec::new();
    for count in 1..=3 {
        match attempt(count) {
            Ok(done) => return done,
            Err(missed) if missed.by_all => {
                eprintln!("attempt {count} of 3 lost: {}", missed.said);
                lost.push(format!("attempt {count}: {}", missed.said));
            }
            Err(missed) => panic!("{}", missed.said),
        }
    }
    panic!("{} attempts, each lost: {}", lost.len(), lost.join(" | "))
}

// Test seam: each task of the owner threads, in order.
#[cfg(test)]
static HANDLED: Mutex<Vec<Handled>> = Mutex::new(Vec::new());

#[cfg(test)]
struct Handled {
    task: usize,
    watcher: usize,
    root: PathBuf,
    kind: &'static str,
}

#[cfg(all(test, target_os = "macos"))]
fn tasks_handled(root: &Path) -> usize {
    (HANDLED.lock().iter())
        .filter(|handled| handled.root == root)
        .count()
}

// Test seam: a task of an owner thread, the watcher it served and the work trees it told
// repository and ref listeners of.
#[cfg(test)]
pub(crate) struct TaskTold {
    pub(crate) task: usize,
    pub(crate) watcher: usize,
    pub(crate) kind: &'static str,
    pub(crate) repositories: Vec<PathBuf>,
    pub(crate) refs: Vec<PathBuf>,
}

// The tasks of `root`'s owner threads numbered above `since`, in order.
#[cfg(test)]
pub(crate) fn tasks_told(root: &Path, since: usize) -> Vec<TaskTold> {
    let handled: Vec<_> = (HANDLED.lock().iter())
        .filter(|handled| handled.root == root && handled.task > since)
        .map(|handled| (handled.task, handled.watcher, handled.kind))
        .collect();
    let told = |told: &Mutex<Vec<(usize, PathBuf)>>, task: usize| -> Vec<PathBuf> {
        (told.lock().iter())
            .filter(|(known, _)| *known == task)
            .map(|(_, work_tree)| work_tree.clone())
            .collect()
    };
    (handled.into_iter())
        .map(|(task, watcher, kind)| TaskTold {
            task,
            watcher,
            kind,
            repositories: told(&REPOSITORIES_TOLD, task),
            refs: told(&REFS_TOLD, task),
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn tasks_started() -> usize {
    TASKS.load(Ordering::SeqCst)
}

#[cfg(test)]
fn handled(watches: &Arc<Watches>, task: &WatchTask) -> usize {
    let number = TASKS.fetch_add(1, Ordering::SeqCst) + 1;
    let kind = match task {
        WatchTask::NewRepository(_) => "NewRepository",
        WatchTask::WatchRepository { .. } => "WatchRepository",
        WatchTask::NewGitLog(..) => "NewGitLog",
        WatchTask::NewGitInfo(_) => "NewGitInfo",
        // An announcement's, which names the work trees it announced.
        WatchTask::SyncRefs(told) if !told.is_empty() => "SyncRefs naming",
        WatchTask::SyncRefs(_) => "SyncRefs",
        WatchTask::FollowRefs(_) => "FollowRefs",
        WatchTask::Announce => "Announce",
        #[cfg(target_os = "macos")]
        WatchTask::Watched { .. } => "Watched",
    };
    HANDLED.lock().push(Handled {
        task: number,
        watcher: Arc::as_ptr(watches) as usize,
        root: watches.base_path.clone(),
        kind,
    });
    number
}

// Test seam: the repository and ref notifications each task of an owner thread made, by task.
#[cfg(test)]
static TASKS: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
static REPOSITORIES_TOLD: Mutex<Vec<(usize, PathBuf)>> = Mutex::new(Vec::new());

#[cfg(test)]
static REFS_TOLD: Mutex<Vec<(usize, PathBuf)>> = Mutex::new(Vec::new());

#[cfg(test)]
thread_local! {
    static TASK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn repository_told(work_tree: &Path) {
    told_in_task(&REPOSITORIES_TOLD, work_tree);
}

#[cfg(test)]
pub(crate) fn ref_told(work_tree: &Path) {
    told_in_task(&REFS_TOLD, work_tree);
}

#[cfg(test)]
fn told_in_task(told: &Mutex<Vec<(usize, PathBuf)>>, work_tree: &Path) {
    let task = TASK.with(std::cell::Cell::get);
    if task != 0 {
        told.lock().push((task, work_tree.to_path_buf()));
    }
}

// Test seam: the next task of a root's owner thread panics.
#[cfg(test)]
static PANICKING_TASKS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn panicking_task_for(root: &Path) {
    PANICKING_TASKS.lock().push(root.to_path_buf());
}

#[cfg(test)]
fn panicking_task(base_path: &Path) {
    let mut panicking = PANICKING_TASKS.lock();
    if let Some(at) = panicking.iter().position(|root| root == base_path) {
        panicking.remove(at);
        drop(panicking);
        panic!("a task of the watcher's thread failed halfway");
    }
}

// Test seam: setting up a root's streams panics.
#[cfg(test)]
static PANICKING_STARTS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn panicking_stream_starts_for(root: &Path) {
    PANICKING_STARTS.lock().push(root.to_path_buf());
}

#[cfg(test)]
fn started_stream(base_path: &Path) {
    if PANICKING_STARTS.lock().iter().any(|root| root == base_path) {
        panic!("starting the streams failed halfway");
    }
    let delay =
        (SLOW_STARTS.lock().iter()).find_map(|(root, delay)| (root == base_path).then_some(*delay));
    if let Some(delay) = delay {
        std::thread::sleep(delay);
    }
}

// Roots whose watchers tests start with a watch per directory, as on Linux, on any platform.
#[cfg(test)]
static PER_DIRECTORY_ROOTS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

#[cfg(all(test, target_os = "macos"))]
pub(crate) fn native_watcher_for(root: &Path) {
    NATIVE_ROOTS.lock().push(root.to_path_buf());
}

#[cfg(test)]
pub(crate) fn watch_per_directory_for(root: &Path) {
    PER_DIRECTORY_ROOTS.lock().push(root.to_path_buf());
}

fn recursive_watch(_base_path: &Path) -> bool {
    #[cfg(test)]
    if PER_DIRECTORY_ROOTS
        .lock()
        .iter()
        .any(|root| root == _base_path)
    {
        return false;
    }
    cfg!(any(target_os = "macos", target_os = "windows"))
}

/// Test seam: watchers started later for `root` in this process poll instead of
/// using OS events. Debug and test builds only.
#[cfg(any(test, debug_assertions))]
#[doc(hidden)]
pub fn poll_watcher_for(root: &Path) {
    let root = crate::path_utils::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    POLLED_ROOTS.lock().push(root);
}

// Subscribes directories one at a time as a walk reaches them, inline, and has the owner thread
// watch the git state of the repositories among them, which walks skip.
struct DirSubscriptions {
    watches: Arc<Watches>,
    watched: AtomicUsize,
    consecutive_failures: AtomicUsize,
}

impl DirSubscriptions {
    fn hook(watches: &Arc<Watches>) -> DirHook {
        let subscriptions = Self {
            watches: Arc::clone(watches),
            watched: AtomicUsize::new(0),
            consecutive_failures: AtomicUsize::new(0),
        };
        Arc::new(move |dir: &Path| subscriptions.reach(dir))
    }

    fn reach(&self, dir: &Path) {
        if !self.watches.recursive {
            self.subscribe(dir);
        }
        if self.watches.follows_git {
            self.watch_git_state(dir);
        }
    }

    // A directory is subscribed before it is read, as Watchman's crawler does. Once the watcher
    // stopped, nothing is.
    fn subscribe(&self, dir: &Path) {
        if self.consecutive_failures.load(Ordering::Relaxed) >= MAX_CONSECUTIVE_WATCH_FAILURES {
            return;
        }
        let mut directories = self.watches.directories.lock();
        if self.watches.stopped.load(Ordering::Acquire) {
            return;
        }
        let Some(stream) = directories.as_mut() else {
            return;
        };
        match stream.watch(dir, RecursiveMode::NonRecursive) {
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

    // Records work tree `dir`'s git dir and watches it if new or replaced, never again, as a
    // polling snapshot loses what changed since; the walk waits unless a stream restarts for it.
    fn watch_git_state(&self, dir: &Path) {
        let Some(git_dir) = git_dir_of(dir) else {
            return;
        };
        if !self.watches.git_dirs.register(&git_dir, dir) {
            return;
        }
        let work_tree = dir.to_path_buf();
        if self.watches.restarts {
            let done = None;
            self.watches.send(WatchTask::WatchRepository {
                work_tree,
                git_dir,
                done,
            });
        } else {
            self.watches.ask(|done| WatchTask::WatchRepository {
                work_tree,
                git_dir,
                done: Some(done),
            });
        }
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

// The work trees in path order, each once.
fn once_each(work_trees: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut work_trees: Vec<PathBuf> = (work_trees.into_iter())
        .map(|work_tree| work_tree.components().collect())
        .collect();
    work_trees.sort_unstable();
    work_trees.dedup();
    work_trees
}

// Whether `dir` is a git dir by its shape.
pub(crate) fn is_git_dir(dir: &Path) -> bool {
    dir.join("HEAD").is_file() && (dir.join("objects").is_dir() || dir.join("commondir").is_file())
}

// The git state of a repository lives in its git dir and its log.
// A repository's exclude file, in its common dir's `info`, holds rules for its whole work tree.
fn git_dir_watches(git_dir: &Path) -> impl Iterator<Item = PathOp> {
    let info = super::common_dir_of(git_dir).join("info");
    [git_dir.to_path_buf(), git_dir.join("logs"), info]
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
    let content = String::from_utf8(crate::stat::read_regular(&dot_git)?).ok()?;
    let target = content.strip_prefix("gitdir:")?.trim();
    crate::path_utils::canonicalize(dir.join(target)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_picker::{FilePicker, FilePickerOptions};
    use crate::watch::{WatchEvent, WatchEventKind, WatchOptions};
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
        let created = Event::new(EventKind::Create(CreateKind::Folder)).add_path(dir.clone());
        shared_picker.feed(None, vec![DebouncedEvent::new(created, Instant::now())]);
        // Past the capacity none of its files is read or reported: the rescan that takes them in
        // tells subscribers once its index is in place.
        let mut reported = Vec::new();
        loop {
            let events = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
            if let Some(rescan) = events.iter().find(|e| e.kind == WatchEventKind::Rescan) {
                assert_eq!(rescan.path, base);
                break;
            }
            reported.extend(events);
        }
        assert!(reported.is_empty(), "{reported:?}");
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

    #[cfg(any(feature = "libgit2", feature = "gitoxide"))]
    #[test]
    fn dotgit_status_filter_matches_worktree_state_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        assert!(status.success());
        let workspace = crate::scm::ScmWorkspace::discover(tmp.path()).unwrap();
        let git_dir = workspace.metadata.clone();
        let opened = workspace.open().unwrap();
        let repo = Some(super::super::repository::Repo {
            workspace: &workspace,
            repo: &opened,
        });
        use crate::watch::git_changes::{
            affects_worktree_state, is_dotgit_change_affecting_status,
        };

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
                is_dotgit_change_affecting_status(&git_dir.join(p), repo),
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
                !is_dotgit_change_affecting_status(&git_dir.join(p), repo),
                "{p} must NOT trigger a git status rescan"
            );
        }

        // Worktree paths outside .git never match.
        assert!(!is_dotgit_change_affecting_status(
            &tmp.path().join("src/main.rs"),
            repo
        ));
        assert!(!is_dotgit_change_affecting_status(
            &git_dir.join("index"),
            None
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
        // The registry is the installed watcher's.
        (shared_picker.write().unwrap().as_mut().unwrap()).background_watcher =
            Some(BackgroundWatcher::unstarted(&base));
        let linked = base.join("main/.git/worktrees/worktree");
        assert!(shared_picker.register_git_dir(&inner.join(".git"), &inner));
        assert!(shared_picker.register_git_dir(&linked, &worktree));
        let removed = |path: &Path| {
            let event = Event::new(EventKind::Remove(notify::event::RemoveKind::Folder))
                .add_path(path.to_path_buf());
            crate::watch::apply_events(
                &shared_picker,
                &WatcherOptions::default(),
                None,
                None,
                vec![DebouncedEvent::new(event, Instant::now())],
            );
            // Listeners hear on the dispatch thread.
            shared_picker.watch_registry().wait_for_dispatch();
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
            crate::watch::apply_events(
                &shared_picker,
                &WatcherOptions::default(),
                None,
                None,
                vec![DebouncedEvent::new(event, Instant::now())],
            );
            // Listeners hear on the dispatch thread.
            shared_picker.watch_registry().wait_for_dispatch();
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
            crate::watch::apply_events(
                &shared_picker,
                &WatcherOptions::default(),
                workdir.as_deref(),
                None,
                vec![DebouncedEvent::new(event, Instant::now())],
            );
            // Listeners hear on the dispatch thread.
            shared_picker.watch_registry().wait_for_dispatch();
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

    // An index without Git integration follows no repository: git state changes and new
    // repositories inside it are not heard, and no walk records a git dir.
    #[test]
    fn an_index_without_git_follows_no_repository_state() {
        let tmp = tempfile::tempdir().unwrap();
        let base = crate::path_utils::canonicalize(tmp.path()).unwrap();
        let inner = base.join("inner");
        git2::Repository::init(&inner).unwrap();
        std::fs::write(inner.join("file.txt"), "a").unwrap();
        let (shared_picker, _events) = subscribed_picker(&base);
        let (sender, heard) = mpsc::channel::<PathBuf>();
        let _subscription = shared_picker
            .subscribe_repository_state(move |root| sender.send(root.to_path_buf()).unwrap());
        let handle = |options: &WatcherOptions, path: PathBuf| {
            let event =
                Event::new(EventKind::Modify(notify::event::ModifyKind::Any)).add_path(path);
            crate::watch::apply_events(
                &shared_picker,
                options,
                None,
                None,
                vec![DebouncedEvent::new(event, Instant::now())],
            );
            // Listeners hear on the dispatch thread.
            shared_picker.watch_registry().wait_for_dispatch();
        };
        let without_git = WatcherOptions {
            ignore_options: FilePickerIgnoreOptions {
                disable_git: true,
                ..Default::default()
            },
            ..Default::default()
        };
        handle(&without_git, inner.join(".git/index"));
        assert!(
            heard.try_recv().is_err(),
            "an index without git heard git state"
        );
        handle(&WatcherOptions::default(), inner.join(".git/index"));
        assert_eq!(heard.try_recv().unwrap(), inner);
        // Only the zlob walker builds an index without Git integration.
        #[cfg(feature = "zlob")]
        walk_records_no_git_dir(&base, &inner, without_git.ignore_options);
    }

    #[cfg(feature = "zlob")]
    fn walk_records_no_git_dir(base: &Path, inner: &Path, ignore: FilePickerIgnoreOptions) {
        poll_watcher_for(base);
        let shared_picker = SharedFilePicker::default();
        FilePicker::new_with_shared_state_and_ignore_options(
            shared_picker.clone(),
            SharedFrecency::noop(),
            FilePickerOptions {
                base_path: base.to_string_lossy().into_owned(),
                enable_content_indexing: false,
                watch: true,
                ..Default::default()
            },
            ignore,
        )
        .unwrap();
        assert!(shared_picker.wait_for_scan(Duration::from_secs(30)));
        assert!(shared_picker.wait_for_watcher(Duration::from_secs(30)));
        assert!(
            shared_picker
                .registered_git_dir(&inner.join(".git/index"))
                .is_none()
        );
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
            drain(&src_heard) == [base.as_path(), base.as_path()],
            "src is not in the linked worktree, so its own work tree stands for it"
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

    // A covered repository's first log in one batch with `logs/HEAD`: the batch announces it,
    // and the log's task does not again.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn a_first_log_with_its_head_in_one_batch_is_announced_once() {
        assert_eq!(first_log_announcements(&["logs", "logs/HEAD"]), 1);
    }

    // A first log of remote-tracking refs alone is announced by the log's task, as before.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    #[test]
    fn a_first_remote_tracking_log_alone_is_announced_by_its_task() {
        assert_eq!(
            first_log_announcements(&["logs", "logs/refs/remotes/origin/main"]),
            1
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

    // A linked worktree's git dir registered by a change of its HEAD, before the worktree's `.git`
    // reaches the watcher, is followed at once: its ref listener hears its next ref change.
    #[test]
    fn a_git_dir_registered_by_a_change_of_its_state_is_followed_for_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let base = crate::path_utils::canonicalize(tmp.path()).unwrap();
        let main = base.join("main");
        std::fs::create_dir(&main).unwrap();
        let repo = git2::Repository::init(&main).unwrap();
        std::fs::write(main.join("file.txt"), "a").unwrap();
        commit(&repo);
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("wt", &head, false).unwrap();
        let sub = base.join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("s.txt"), "s").unwrap();
        let (shared_picker, workdir) = polled_picker(&base);
        let (sender, heard) = mpsc::channel::<PathBuf>();
        let _refs = shared_picker
            .subscribe_ref_changes(&sub, move |root| sender.send(root.to_path_buf()).unwrap());
        let handle = |path: PathBuf| modified(&shared_picker, &base, &workdir, path);
        let watches = shared_picker.installed_watches().unwrap();
        let idle = || {
            let deadline = Instant::now() + Duration::from_secs(30);
            while watches.busy() {
                assert!(Instant::now() < deadline, "the watcher's thread stays busy");
                std::thread::sleep(Duration::from_millis(20));
            }
        };

        // What `git worktree add` writes before the worktree's `.git`.
        let main_git_dir = crate::path_utils::canonicalize(repo.path()).unwrap();
        let admin = main_git_dir.join("worktrees/wt");
        let worktree = sub.join("wt");
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::create_dir(&worktree).unwrap();
        std::fs::write(admin.join("commondir"), "../..\n").unwrap();
        let gitdir = format!("{}\n", worktree.join(".git").display());
        std::fs::write(admin.join("gitdir"), gitdir).unwrap();
        std::fs::write(admin.join("HEAD"), "ref: refs/heads/wt\n").unwrap();
        handle(admin.join("HEAD"));
        let registered = shared_picker.registered_git_dirs();
        assert!(
            registered.iter().any(|known| known.git_dir == admin),
            "the worktree's git dir is not registered"
        );
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", admin.display()),
        )
        .unwrap();
        handle(worktree.join(".git"));
        idle();
        drain(&heard);

        let signature = git2::Signature::now("fff", "fff@example.invalid").unwrap();
        let tree = head.tree().unwrap();
        let next = (repo.commit(None, &signature, &signature, "next", &tree, &[&head])).unwrap();
        std::fs::write(main_git_dir.join("refs/heads/wt"), format!("{next}\n")).unwrap();
        handle(main_git_dir.join("refs/heads/wt"));
        let told = drain(&heard);
        assert!(told.contains(&worktree), "{told:?}");
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
        let (shared_picker, watcher) = native_picker(&linked, 3);
        let watches = watcher.watches;
        let paths = |outside: bool| watches.watched_paths(outside);

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
    // missed meanwhile is recovered on the watcher's thread.
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
        let (shared_picker, watcher) = native_picker(&linked, 3);
        let (state_sender, state) = mpsc::channel::<PathBuf>();
        let _state = shared_picker
            .subscribe_repository_state(move |root| state_sender.send(root.to_path_buf()).unwrap());
        let (sender, heard) = mpsc::channel::<PathBuf>();
        let _refs = shared_picker.subscribe_ref_changes(&linked, move |root| {
            sender.send(root.to_path_buf()).unwrap()
        });
        let git_status = (shared_picker.read().unwrap().as_ref())
            .map(|picker| Arc::clone(&picker.scm_status_worker))
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

        // The watcher's thread may restart the stream for the new worktree first: the same is
        // recovered. The window counts from the end of its restarts, run one task at a time.
        let outside_watched = || watcher.watches.watched_paths(true);
        let work_trees = [linked.clone(), linked.join("nested")];
        let (mut announced, mut told) = (Vec::new(), Vec::new());
        let window = Duration::from_secs(10);
        let mut deadline = Instant::now() + window;
        // However long the restarts take, the wait ends, and what was seen by then is asserted.
        let limit = Instant::now() + Duration::from_secs(120);
        loop {
            if watcher.watches.busy() {
                deadline = Instant::now() + window;
            }
            announced.extend(drain(&state));
            told.extend(drain(&heard));
            let mut recovered = work_trees
                .iter()
                .all(|work_tree| announced.contains(work_tree))
                && told.contains(&linked)
                && told.contains(&linked.join("lone"))
                && git_status.full_rescans_requested() > status_rescans
                && watcher.watches.refs.watched().contains(&lone.join("refs"));
            if recovered {
                // The query waits behind the tasks queued before it, restarts included.
                let asked = Instant::now();
                recovered = (outside_watched().iter()).any(|(path, _)| *path == lone.join("logs"));
                deadline += asked.elapsed();
            }
            if recovered || Instant::now() >= deadline.min(limit) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        for work_tree in &work_trees {
            assert!(
                announced.contains(work_tree),
                "{work_tree:?} in {announced:?}"
            );
        }
        assert!(told.contains(&linked), "{told:?}");
        assert!(
            told.contains(&linked.join("lone")),
            "its refs appeared: {told:?}"
        );
        assert!(git_status.full_rescans_requested() > status_rescans);
        assert!(watcher.watches.refs.watched().contains(&lone.join("refs")));
        let watched = outside_watched();
        assert!(
            watched.iter().any(|(path, _)| *path == lone.join("logs")),
            "{watched:?}"
        );
    }

    // A ref store that appears outside the tree is watched by a task that restarts the outside
    // stream: ref listeners hear once of each work tree its recovery and its own plan found.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_task_that_restarts_the_outside_stream_tells_each_work_tree_once() {
        let tmp = tempfile::tempdir().unwrap();
        let root = crate::path_utils::canonicalize(tmp.path()).unwrap();
        let main = root.join("main");
        let repo = git2::Repository::init(&main).unwrap();
        std::fs::write(main.join("file.txt"), "a").unwrap();
        commit(&repo);
        let linked = root.join("linked");
        repo.worktree("linked", &linked, None).unwrap();
        // A repository whose git dir, outside the index, has no refs yet.
        let lone = root.join("lone.git");
        std::fs::create_dir_all(lone.join("objects")).unwrap();
        std::fs::write(lone.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir(linked.join("lone")).unwrap();
        let gitlink = format!("gitdir: {}\n", lone.display());
        std::fs::write(linked.join("lone/.git"), gitlink).unwrap();
        let (shared_picker, watcher) = native_picker(&linked, 3);
        let _refs = shared_picker.subscribe_ref_changes(&linked, |_| {});
        let idle = || {
            let deadline = Instant::now() + Duration::from_secs(60);
            while watcher.watches.busy() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        idle();
        let since = TASKS.load(Ordering::SeqCst);

        // The store appears whole, as one event, reported as the stream would deliver it.
        let staged = root.join("staged-refs");
        std::fs::create_dir_all(staged.join("heads")).unwrap();
        std::fs::rename(&staged, lone.join("refs")).unwrap();
        let reported = Instant::now();
        let refs = vec![lone.join("refs")];
        let answer = shared_picker.apply_reported_changes(&SharedFrecency::noop(), refs);
        assert_eq!(answer, crate::ReportedChanges::Applied);
        let watched = within_product_time(&linked, reported, Duration::from_secs(10), 1, || {
            watcher.watches.refs.watched().contains(&lone.join("refs"))
        });
        assert!(watched, "the new ref store is not watched within 10 s");
        idle();
        let told: Vec<_> = (REFS_TOLD.lock().iter())
            .filter(|(task, work_tree)| *task > since && work_tree.starts_with(&linked))
            .cloned()
            .collect();
        for work_tree in [linked.clone(), linked.join("lone")] {
            let mut tasks: Vec<_> = (told.iter())
                .filter(|(_, known)| *known == work_tree)
                .map(|(task, _)| *task)
                .collect();
            let times = tasks.len();
            tasks.dedup();
            assert!(times > 0, "{work_tree:?} was not told: {told:?}");
            assert_eq!(
                tasks.len(),
                times,
                "{work_tree:?} twice in a task: {told:?}"
            );
        }
    }

    // A directory appears unseen, holding a repository and files, and only one of its files is
    // reported: the directory is walked whole, so its files are indexed and its repository heard.
    #[test]
    fn a_file_below_a_directory_that_appeared_unseen_brings_in_the_whole_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let base = crate::path_utils::canonicalize(tmp.path()).unwrap();
        std::fs::write(base.join("a.txt"), "a").unwrap();
        let (shared_picker, _) = polled_picker(&base);
        let watches = (shared_picker.read().unwrap().as_ref())
            .and_then(|picker| picker.background_watcher.as_ref().map(|w| w.watches()))
            .unwrap();
        let (sender, heard) = mpsc::channel::<PathBuf>();
        let _state = shared_picker
            .subscribe_repository_state(move |root| sender.send(root.to_path_buf()).unwrap());
        let unseen = base.join("unseen");
        UNDELIVERED.lock().push(unseen.clone());
        git2::Repository::init(unseen.join("repo")).unwrap();
        for file in ["repo/a.rs", "repo/b.rs", "c.rs"] {
            std::fs::write(unseen.join(file), "fn f() {}").unwrap();
        }
        let reported = vec![unseen.join("repo/a.rs")];
        let answer = shared_picker.apply_reported_changes(&SharedFrecency::noop(), reported);
        assert_eq!(answer, crate::ReportedChanges::Applied);
        let idle = within(Instant::now(), Duration::from_secs(30), || !watches.busy());
        assert!(idle, "the watcher's thread is still busy");
        shared_picker.watch_registry().wait_for_dispatch();
        let files: Vec<String> = {
            let guard = shared_picker.read().unwrap();
            let picker = guard.as_ref().unwrap();
            let live = (picker.get_files().iter()).filter(|file| !file.is_deleted());
            live.map(|file| file.relative_path(picker)).collect()
        };
        for file in ["unseen/repo/a.rs", "unseen/repo/b.rs", "unseen/c.rs"] {
            assert!(
                files.iter().any(|known| known == file),
                "{file} not in {files:?}"
            );
        }
        assert_eq!(drain(&heard), [unseen.join("repo")]);
    }

    // Ignored files below directories the index never saw change nothing: none is walked, as
    // none would be again and again while those files are written.
    #[test]
    fn an_ignored_file_below_a_directory_never_seen_walks_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let base = crate::path_utils::canonicalize(tmp.path()).unwrap();
        git2::Repository::init(&base).unwrap();
        std::fs::write(base.join(".gitignore"), "*.log\nbuild/\n").unwrap();
        let (shared_picker, _) = polled_picker(&base);
        let unseen = base.join("unseen");
        UNDELIVERED.lock().push(unseen.clone());
        std::fs::create_dir_all(unseen.join("logs")).unwrap();
        std::fs::create_dir_all(unseen.join("build/sub")).unwrap();
        std::fs::write(unseen.join("logs/a.log"), "a").unwrap();
        std::fs::write(unseen.join("build/sub/b.rs"), "fn f() {}").unwrap();
        let reported = vec![unseen.join("logs/a.log"), unseen.join("build/sub/b.rs")];
        let answer = shared_picker.apply_reported_changes(&SharedFrecency::noop(), reported);
        assert_eq!(answer, crate::ReportedChanges::Applied);
        assert_eq!(super::super::reconcile::walks_at_or_below(&unseen), 0);
        let guard = shared_picker.read().unwrap();
        let picker = guard.as_ref().unwrap();
        let below =
            |file: &crate::types::FileItem| file.relative_path(picker).starts_with("unseen/");
        assert!(!picker.get_files().iter().any(below));
    }

    // A directory walked for a file below it is known from then on: that file reported again,
    // or a new one in it, walks nothing more.
    #[test]
    fn a_directory_walked_for_a_file_never_seen_is_walked_once() {
        let tmp = tempfile::tempdir().unwrap();
        let base = crate::path_utils::canonicalize(tmp.path()).unwrap();
        std::fs::write(base.join("a.txt"), "a").unwrap();
        let (shared_picker, _) = polled_picker(&base);
        let unseen = base.join("unseen");
        UNDELIVERED.lock().push(unseen.clone());
        std::fs::create_dir_all(unseen.join("sub")).unwrap();
        std::fs::write(unseen.join("sub/a.rs"), "fn f() {}").unwrap();
        let frecency = SharedFrecency::noop();
        let walks = || super::super::reconcile::walks_at_or_below(&unseen);
        let answer = shared_picker.apply_reported_changes(&frecency, vec![unseen.join("sub/a.rs")]);
        assert_eq!(answer, crate::ReportedChanges::Applied);
        assert_eq!(walks(), 1);
        shared_picker.apply_reported_changes(&frecency, vec![unseen.join("sub/a.rs")]);
        assert_eq!(walks(), 1);
        // The walk's root holds no file of its own, so only the walk made it known.
        std::fs::write(unseen.join("b.rs"), "fn g() {}").unwrap();
        shared_picker.apply_reported_changes(&frecency, vec![unseen.join("b.rs")]);
        assert_eq!(walks(), 1);
        let guard = shared_picker.read().unwrap();
        let picker = guard.as_ref().unwrap();
        let files: Vec<String> = (picker.get_files().iter())
            .filter(|file| !file.is_deleted())
            .map(|file| file.relative_path(picker))
            .collect();
        for file in ["unseen/sub/a.rs", "unseen/b.rs"] {
            assert!(
                files.iter().any(|known| known == file),
                "{file} not in {files:?}"
            );
        }
    }

    // Not a test of its own: the stream of `Control`, which a test runs in a child process.
    #[cfg(target_os = "macos")]
    #[test]
    fn fsevents_control() {
        let root = std::env::var_os(CONTROL_ROOT);
        if let (Some(root), Some(seen)) = (root, std::env::var_os(CONTROL_SEEN)) {
            run_control(Path::new(&root), Path::new(&seen));
        }
    }

    // A linked worktree of a repository outside the tree is added inside it: repository and ref
    // listeners hear of it, each work tree once in a task of the watcher's thread.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_task_that_follows_a_new_linked_worktree_tells_each_work_tree_once() {
        let tmp = tempfile::tempdir().unwrap();
        let root = crate::path_utils::canonicalize(tmp.path()).unwrap();
        let (tree, outside) = (root.join("tree"), root.join("outside"));
        for (dir, file) in [(&tree, "a.txt"), (&outside, "m.txt")] {
            let repo = git2::Repository::init(dir).unwrap();
            std::fs::write(dir.join(file), "a").unwrap();
            commit(&repo);
        }
        let (shared_picker, watcher) = native_picker(&tree, 2);
        let _repositories = shared_picker.subscribe_repository_state(|_| {});
        let _refs = shared_picker.subscribe_ref_changes(&tree, |_| {});
        let idle = || {
            let deadline = Instant::now() + Duration::from_secs(60);
            while watcher.watches.busy() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        idle();
        let since = TASKS.load(Ordering::SeqCst);
        let told = |told: &Mutex<Vec<(usize, PathBuf)>>| -> Vec<(usize, PathBuf)> {
            (told.lock().iter())
                .filter(|(task, work_tree)| *task > since && work_tree.starts_with(&tree))
                .cloned()
                .collect()
        };

        let control = until_delivering(&tree);
        let outside = git2::Repository::open(&outside).unwrap();
        // A worktree FSEvents lost for every client leaves the result open: a fresh one is added.
        retried_while_lost(|attempt| {
            let name = match attempt {
                1 => "wt".to_string(),
                _ => format!("wt-{attempt}"),
            };
            let worktree = tree.join(&name);
            outside.worktree(&name, &worktree, None).unwrap();
            // Any path in the worktree, written after its `.git`, has the watcher follow it through
            // a restart: FSEvents may report the files without the `.git` itself.
            let inside = |path: &Path| path.starts_with(&worktree) && path != worktree;
            let delivery = delivered(&tree, &format!("the worktree {name}"), inside, &control);
            if let Ok(at) = delivery {
                // One update of the outside stream watches its git dir, logs, info and refs.
                let heard = within_product_time(&tree, at, Duration::from_secs(10), 1, || {
                    (told(&REPOSITORIES_TOLD).iter()).any(|(_, known)| *known == worktree)
                });
                let starts = stream_starts_since(&tree, at);
                assert!(
                    starts <= 1,
                    "the product restarted the streams {starts} times to follow {name}, not once"
                );
                assert!(
                    heard,
                    "{name} not told within 10 s of the delivery: {:?}",
                    told(&REPOSITORIES_TOLD)
                );
            }
            std::thread::sleep(Duration::from_millis(500));
            idle();
            for told in [told(&REPOSITORIES_TOLD), told(&REFS_TOLD)] {
                let mut once = told.clone();
                once.sort();
                once.dedup();
                assert_eq!(once.len(), told.len(), "twice in a task: {told:?}");
            }
            delivery
        });
    }

    // A repository with its git dir outside appears in a ready index: its watches change on the
    // watcher's thread, so a slow stream restart holds up no change the loop applies meanwhile.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_slow_stream_restart_holds_up_no_reported_change() {
        use crate::scan::tests::on_a_thread;
        let tmp = tempfile::tempdir().unwrap();
        let root = crate::path_utils::canonicalize(tmp.path()).unwrap();
        let (main, tree) = (root.join("main"), root.join("tree"));
        std::fs::create_dir_all(&main).unwrap();
        let repo = git2::Repository::init(&main).unwrap();
        std::fs::write(main.join("file.txt"), "a").unwrap();
        commit(&repo);
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join("a.rs"), "fn a() {}").unwrap();
        let (shared_picker, _watcher) = native_picker(&tree, 1);
        slow_stream_starts_for(&tree, Duration::from_secs(4));
        repo.worktree("wt", &tree.join("wt"), None).unwrap();
        let (reporter, reported) = (shared_picker.clone(), tree.join("wt"));
        let worktree = on_a_thread(move || {
            reporter.apply_reported_changes(&SharedFrecency::noop(), vec![reported])
        });
        std::thread::sleep(Duration::from_millis(300));
        std::fs::write(tree.join("b.rs"), "fn b() {}").unwrap();
        let (started, reporter) = (Instant::now(), shared_picker.clone());
        let reported = vec![tree.join("b.rs")];
        let answered =
            on_a_thread(move || reporter.apply_reported_changes(&SharedFrecency::noop(), reported));
        let (answered, waited) = (
            answered.recv_timeout(Duration::from_secs(30)),
            started.elapsed(),
        );
        assert!(
            answered.is_ok() && waited < Duration::from_millis(1500),
            "a reported change waited {waited:?}"
        );
        let worktree = worktree.recv_timeout(Duration::from_secs(30));
        assert!(worktree.is_ok(), "the worktree's report: {worktree:?}");
    }

    // A stopped watcher's owner thread drops what is still queued for it: no repository is
    // watched or announced, and no stream changes, for an index that is gone.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_stopped_watcher_drops_its_queued_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let root = crate::path_utils::canonicalize(tmp.path()).unwrap();
        let (main, tree) = (root.join("main"), root.join("tree"));
        std::fs::create_dir_all(&main).unwrap();
        let repo = git2::Repository::init(&main).unwrap();
        std::fs::write(main.join("file.txt"), "a").unwrap();
        commit(&repo);
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join("a.rs"), "fn a() {}").unwrap();
        let (shared_picker, watcher) = native_picker(&tree, 1);
        slow_stream_starts_for(&tree, Duration::from_secs(1));
        let worktrees: Vec<PathBuf> = (0..4).map(|i| tree.join(format!("wt{i}"))).collect();
        for (i, path) in worktrees.iter().enumerate() {
            repo.worktree(&format!("wt{i}"), path, None).unwrap();
        }
        let _ = shared_picker.apply_reported_changes(&SharedFrecency::noop(), worktrees);
        std::thread::sleep(Duration::from_millis(300));
        shared_picker.remove_index();
        let at_removal = tasks_handled(&tree);
        std::thread::sleep(Duration::from_secs(6));
        // The task running at the removal ends; none queued after it starts.
        assert!(
            tasks_handled(&tree) <= at_removal + 1,
            "{} tasks started after the removal",
            tasks_handled(&tree) - at_removal
        );
        // Once the task in flight ends, the test seam counts the dropped tasks as handled.
        let deadline = Instant::now() + Duration::from_secs(60);
        while watcher.watches.busy() {
            assert!(
                Instant::now() < deadline,
                "dropped tasks still count as queued"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_watch_update_goes_on_past_a_failed_path() {
        let tmp = tempfile::tempdir().unwrap();
        let config = Config::default().with_poll_interval(Duration::from_secs(60));
        let debouncer = new_debouncer_opt::<_, Tapped<notify::PollWatcher>, _>(
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
        let debouncer = new_debouncer_opt::<_, Tapped<notify::RecommendedWatcher>, _>(
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

    // An index of `base` with the native watcher, and its watcher's shared state. Its first stream
    // is started within 10 s and the index is ready within 30 s, besides the daemon's time.
    #[cfg(target_os = "macos")]
    // `starts`: those its setup needs, of the tree's stream and the git state inside and out.
    fn native_picker(base: &Path, starts: usize) -> (SharedFilePicker, WatcherHandle) {
        NATIVE_ROOTS.lock().push(base.to_path_buf());
        let shared_picker = SharedFilePicker::default();
        let created = Instant::now();
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
        let requested = within(created, Duration::from_secs(10), || stream_starts(base) > 0);
        assert!(requested, "no stream was started within 10 s");
        let ready = within_product_time(base, created, Duration::from_secs(30), starts, || {
            shared_picker.wait_for_scan(Duration::ZERO)
                && shared_picker.wait_for_watcher(Duration::ZERO)
        });
        assert!(ready, "the scan and the watcher are not ready");
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
        let _ = base;
        crate::watch::apply_events(
            picker,
            &WatcherOptions::default(),
            workdir.as_deref(),
            None,
            vec![DebouncedEvent::new(event, Instant::now())],
        );
        // Listeners hear on the dispatch thread.
        picker.watch_registry().wait_for_dispatch();
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
        let _ = base;
        crate::watch::apply_events(
            shared_picker,
            &WatcherOptions::default(),
            None,
            None,
            events,
        );
    }

    // How often repository listeners hear of an index's own repository from one batch that
    // reports its git dir's `logs` as new, with `created` below the git dir.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn first_log_announcements(created: &[&str]) -> usize {
        let tmp = tempfile::tempdir().unwrap();
        let base = crate::path_utils::canonicalize(tmp.path()).unwrap();
        let repo = git2::Repository::init(&base).unwrap();
        std::fs::write(base.join("a.txt"), "a").unwrap();
        commit(&repo);
        let remote_log = base.join(".git/logs/refs/remotes/origin/main");
        std::fs::create_dir_all(remote_log.parent().unwrap()).unwrap();
        std::fs::write(&remote_log, "").unwrap();
        let (shared_picker, workdir) = polled_picker(&base);
        let watches = (shared_picker.read().unwrap().as_ref())
            .and_then(|picker| picker.background_watcher.as_ref().map(|w| w.watches()))
            .unwrap();
        let (sender, heard) = mpsc::channel::<PathBuf>();
        let _state = shared_picker
            .subscribe_repository_state(move |root| sender.send(root.to_path_buf()).unwrap());
        let now = Instant::now();
        let events = (created.iter())
            .map(|path| {
                let event = Event::new(EventKind::Create(CreateKind::Any));
                DebouncedEvent::new(event.add_path(base.join(".git").join(path)), now)
            })
            .collect();
        let options = WatcherOptions::default();
        crate::watch::apply_events(&shared_picker, &options, workdir.as_deref(), None, events);
        let idle = within(Instant::now(), Duration::from_secs(30), || !watches.busy());
        assert!(idle, "the watcher's thread is still busy");
        shared_picker.watch_registry().wait_for_dispatch();
        let heard = drain(&heard);
        assert!(heard.iter().all(|root| *root == base), "{heard:?}");
        heard.len()
    }

    // Whether `done` holds within `limit` of `from`.
    fn within(from: Instant, limit: Duration, mut done: impl FnMut() -> bool) -> bool {
        while !done() {
            if from.elapsed() >= limit {
                return done();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }
}
