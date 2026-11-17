use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ahash::{AHashMap, AHashSet};
use tracing::{debug, error, info};

use super::background_watcher::WatcherOptions;
use super::changed_paths::{ChangedPaths, Pending};
use super::dir_walk::DirWalk;
use super::git_changes::{GitEvents, announce_git_state};
use super::intake::{IgnoreFilter, PathState};
use super::renames::{FileIdentity, detect_renames};
use super::repository::Repo;
use crate::constants::MAX_OVERFLOW_FILES;
use crate::file_picker::{FilePicker, IndexUpdateError};
use crate::prepared_file::{PreparedFile, RecordSlot};
use crate::rescan_stats::RescanReason;
use crate::scm::{ScmBackend, ScmStatusWorker};
use crate::shared::{SharedFilePicker, SharedFrecency};
use crate::stat::Stat;
use crate::types::FileItem;
use crate::walk::{DirHook, WalkHooks, WalkPolicy};
use crate::watch::{RawWatchEvent, WatchEventKind};

// Minimum seconds between frecency tracks of the same file in AI mode.
// Prevents score inflation from rapid burst edits by AI agents.
const AI_MODE_COOLDOWN_SECS: u64 = 5 * 60;

// Checks a set's paths, walks its recursive entries, applies the difference in one write and tells
// subscribers unless `report` is false; returns the full rescan needed if it did not all fit.
pub(crate) fn reconcile(
    shared: &SharedFilePicker,
    frecency: &SharedFrecency,
    changed: ChangedPaths,
    report: bool,
    repo: Option<Repo<'_>>,
) -> Option<RescanReason> {
    if changed.is_empty() {
        return None;
    }
    let context = Context::of(shared)?;
    context.reconcile(shared, frecency, changed.drain(), report, repo)
}

// What reconciling needs of the index, read once.
struct Context {
    options: WatcherOptions,
    base_path: PathBuf,
    git_workdir: Option<PathBuf>,
    git_backend: Option<ScmBackend>,
    git_status_worker: Arc<dyn ScmStatusWorker>,
    rules: Arc<crate::walk::IgnoreRules>,
    ref_signals: Option<super::ref_watches::RefSignals>,
    dir_hook: Option<DirHook>,
    cancelled: Arc<AtomicBool>,
}

// One path the set names, as it is now.
enum Check {
    // A file, named by the watcher, so recorded again whatever its stat says.
    File(PathBuf),
    // Gone, or nothing the index holds: its entry and everything below it leave.
    Gone(PathBuf),
    // A directory: walked again when recursive or new to the index.
    Dir(PathBuf, bool),
}

// The index's difference: removed and created files with the identity a rename keeps, files
// whose content changed, and every file recorded again.
#[derive(Default)]
struct Delta {
    removed: Vec<(PathBuf, FileIdentity)>,
    created: Vec<(PathBuf, FileIdentity)>,
    modified: Vec<PathBuf>,
    indexed: Vec<PathBuf>,
    rejected: bool,
}

impl Context {
    fn of(shared: &SharedFilePicker) -> Option<Self> {
        let guard = shared.read().ok()?;
        let picker = guard.as_ref()?;
        let watcher = picker.background_watcher.as_ref();
        let watches = watcher.map(|watcher| watcher.watches());
        let mut context = Self {
            options: WatcherOptions::of(picker),
            base_path: picker.base_path().to_path_buf(),
            git_workdir: picker.git_root().map(Path::to_path_buf),
            git_backend: picker.scm_workspace().map(|workspace| workspace.backend),
            git_status_worker: Arc::clone(&picker.scm_status_worker),
            rules: picker.ignore_rules(),
            ref_signals: None,
            dir_hook: watcher.and_then(|watcher| watcher.dir_hook()),
            cancelled: Arc::clone(&picker.signals.cancelled),
        };
        // The ref state is a leaf lock, read once the picker's is released.
        drop(guard);
        context.ref_signals = watches.and_then(|watches| watches.refs.signals());
        Some(context)
    }

    fn reconcile(
        &self,
        shared: &SharedFilePicker,
        frecency: &SharedFrecency,
        pending: Pending,
        report: bool,
        repo: Option<Repo<'_>>,
    ) -> Option<RescanReason> {
        let base_path = self.base_path.as_path();
        let mut filter = IgnoreFilter::new(
            base_path,
            Arc::clone(&self.rules),
            repo,
            self.options.ignore_options.disable_git,
        );
        let git = super::git_changes::classify(
            shared,
            &pending.git,
            base_path,
            self.git_workdir.as_deref(),
            repo,
            self.ref_signals.as_ref(),
            &self.options,
            &mut filter,
        );
        let rule_dirs = self.rule_dirs(shared, &pending, &git, &mut filter);
        let checks = self.checks(shared, &pending, &mut filter);
        let notified: AHashSet<&Path> = (pending.entries.iter())
            .map(|(path, _)| path.as_path())
            .chain(pending.named.iter().map(PathBuf::as_path))
            .collect();

        // Directories walked again, none below another; any of them whose ignore rules may have
        // changed has them replaced even where no ignore file is known.
        let mut roots: Vec<PathBuf> = (checks.iter())
            .filter_map(|check| match check {
                Check::Dir(path, true) => Some(path.clone()),
                _ => None,
            })
            .chain(rule_dirs.iter().cloned())
            .collect();
        roots.sort_unstable();
        roots.dedup_by(|nested, outer| nested.starts_with(&*outer));
        let walks = self.walk(roots);
        let walked = |path: &Path| walk_of(&walks, path).is_some();
        let checks: Vec<Check> = (checks.into_iter())
            .filter(|check| match check {
                Check::File(path) | Check::Gone(path) => !walked(path),
                Check::Dir(..) => false,
            })
            .collect();

        if walks.is_empty() && checks.is_empty() && !git.full_status {
            debug!("No file index changes to apply");
            self.after(
                shared,
                frecency,
                Delta::default(),
                AHashMap::new(),
                git,
                &pending,
            );
            return None;
        }
        info!(
            walks = walks.len(),
            paths = checks.len(),
            "Reconciling changed paths"
        );
        // Decided under a read lock, read with none, committed by the write section; new files past
        // what the index takes are left to a full rescan, held ones still counting.
        let (records, overflowed) = {
            let Ok(guard) = shared.read() else {
                return None;
            };
            let picker = guard.as_ref()?;
            let mut records = plan_walks(picker, base_path, &walks, &notified);
            records.extend(plan_checks(picker, &checks));
            let new = records.iter().filter(|record| !record.slot.held).count();
            let overflowed = new > picker.new_file_capacity();
            (records, overflowed)
        };
        let records: Vec<Record> = (records.into_iter())
            .filter(|record| record.slot.held || !overflowed)
            .collect();
        let mut prepared = Vec::with_capacity(records.len());
        for record in records {
            // Removing or replacing the index never waits for the files it would have read.
            if self.cancelled.load(Ordering::Acquire) {
                return None;
            }
            let file = PreparedFile::read(&record.path, base_path, record.slot);
            #[cfg(test)]
            crate::scan::tests::reached(crate::scan::tests::Stage::Prepared, base_path);
            prepared.push((record, file));
        }
        let (delta, renames, overflow) = {
            let Ok(mut guard) = shared.write() else {
                error!("Failed to acquire file picker write lock");
                return None;
            };
            let picker = guard.as_mut()?;
            if self.cancelled.load(Ordering::Acquire) {
                return None;
            }
            let mut delta = Delta::default();
            let mut walks = walks;
            apply_walks(picker, base_path, &mut walks, &rule_dirs, &mut delta);
            if !delta.rejected {
                apply_checks(picker, checks, &mut delta);
            }
            if !delta.rejected {
                commit(picker, prepared, &mut delta);
            }
            let written: AHashSet<&Path> = (delta.created.iter().map(|(path, _)| path.as_path()))
                .chain(delta.modified.iter().map(PathBuf::as_path))
                .collect();
            let renames =
                detect_renames(&delta.removed, &delta.created, &written, &pending.renames);
            let overflow = picker.get_overflow_files().len();
            (delta, renames, overflow)
        };
        info!(
            files_updated = delta.indexed.len(),
            overflow, "File index changes applied",
        );
        if report {
            self.dispatch(shared, &delta, &renames);
        }
        let rescan = if delta.rejected {
            Some(RescanReason::IndexUpdateRejected)
        } else {
            (overflowed || overflow > MAX_OVERFLOW_FILES).then_some(RescanReason::OverflowCapacity)
        };
        if let Some(reason) = rescan {
            info!(%reason, "Watcher faced limit of index overflow. Triggering rescan");
        }
        // A rescan reads every status again.
        if !delta.rejected && repo.is_some() {
            if git.full_status {
                self.git_status_worker.request_full_rescan();
            } else if !delta.indexed.is_empty() {
                self.git_status_worker.enqueue_paths(delta.indexed.clone());
            }
        }
        self.after(shared, frecency, delta, renames, git, &pending);
        rescan
    }

    // Subtrees whose rules may have changed: those whose events were lost, an ignore file whose
    // content changed, or an exclude file's work tree.
    fn rule_dirs(
        &self,
        shared: &SharedFilePicker,
        pending: &Pending,
        git: &GitEvents,
        filter: &mut IgnoreFilter,
    ) -> Vec<PathBuf> {
        let base_path = self.base_path.as_path();
        let mut rule_dirs: Vec<PathBuf> = (pending.rule_dirs.iter())
            .filter(|dir| dir.is_dir())
            .cloned()
            .chain(git.rule_dirs.iter().cloned())
            .collect();
        for file in &pending.ignore_files {
            let Some(parent) = file.parent() else {
                continue;
            };
            if !parent.starts_with(base_path) || filter.is_ignored(parent, true) {
                continue;
            }
            // Only a change of content changes the rules: a touch reads nothing again.
            let recorded =
                (shared.read().ok()).and_then(|guard| guard.as_ref()?.ignore_source(file));
            if crate::walk::IgnoreSource::changed(recorded, file) {
                info!(path = %file.display(), "Ignore rules changed; walking their subtree");
                rule_dirs.push(parent.to_path_buf());
            }
        }
        rule_dirs.sort_unstable();
        rule_dirs.dedup();
        rule_dirs
    }

    // What each named path is now, read outside the lock; the index decides only whether a
    // directory is new to it.
    fn checks(
        &self,
        shared: &SharedFilePicker,
        pending: &Pending,
        filter: &mut IgnoreFilter,
    ) -> Vec<Check> {
        let follow_symlinks = self.options.follow_symlinks;
        let mut checks = Vec::with_capacity(pending.entries.len());
        for (path, recursive) in &pending.entries {
            let (path, recursive) = (path.as_path(), *recursive);
            match PathState::of(path, follow_symlinks) {
                PathState::File => {
                    if filter.is_ignored(path, false) {
                        continue;
                    }
                    match self.unseen_ancestor(shared, path, filter) {
                        Some(unseen) => checks.push(Check::Dir(unseen, true)),
                        None => checks.push(Check::File(path.to_path_buf())),
                    }
                }
                PathState::Dir => {
                    if path != self.base_path && filter.is_ignored(path, true) {
                        // A directory the rules ignore holds nothing the index keeps.
                        checks.push(Check::Gone(path.to_path_buf()));
                        continue;
                    }
                    if let Some(unseen) = self.unseen_ancestor(shared, path, filter) {
                        checks.push(Check::Dir(unseen, true));
                        continue;
                    }
                    let known = (shared.read().ok())
                        .is_some_and(|guard| guard.as_ref().is_some_and(|p| p.knows_dir(path)));
                    checks.push(Check::Dir(path.to_path_buf(), recursive || !known));
                }
                PathState::Gone => {
                    // A repository whose work tree went is recorded anew when it returns.
                    shared.forget_git_dirs_under(path);
                    checks.push(Check::Gone(path.to_path_buf()));
                }
            }
        }
        checks
    }

    // The topmost directory above a path the rules keep, below the root, that the index does not
    // know: it appeared unseen, so it is walked whole, as Syncthing scans one.
    fn unseen_ancestor(
        &self,
        shared: &SharedFilePicker,
        path: &Path,
        filter: &mut IgnoreFilter,
    ) -> Option<PathBuf> {
        let unknown: Vec<&Path> = {
            let guard = shared.read().ok()?;
            let picker = guard.as_ref()?;
            (path.ancestors().skip(1))
                .take_while(|dir| *dir != self.base_path && dir.starts_with(&self.base_path))
                .take_while(|dir| !picker.knows_dir(dir))
                .collect()
        };
        // Below an ignored directory nothing is walked, whatever the rules say of the path itself.
        if unknown.iter().any(|dir| filter.is_ignored(dir, true)) {
            return None;
        }
        unknown.last().map(|dir| dir.to_path_buf())
    }

    // Walks each root outside the lock, subscribing a directory before reading it, as Watchman's
    // crawler; a root whose walk cannot run keeps its entries, as a failed scan leaves the index.
    fn walk(&self, roots: Vec<PathBuf>) -> Vec<DirWalk> {
        let policy = WalkPolicy {
            follow_symlinks: self.options.follow_symlinks,
            ignore: &self.options.ignore_options,
        };
        let hooks = WalkHooks::new(self.dir_hook.as_ref(), Some(&self.cancelled));
        (roots.into_iter())
            .filter_map(|root| {
                if let Some(subscribe) = &self.dir_hook {
                    subscribe(&root);
                }
                #[cfg(test)]
                WALKED.lock().push(root.clone());
                DirWalk::new(&root, &self.base_path, self.git_backend, policy, hooks)
            })
            .collect()
    }

    fn dispatch(
        &self,
        shared: &SharedFilePicker,
        delta: &Delta,
        renames: &AHashMap<PathBuf, PathBuf>,
    ) {
        let registry = shared.watch_registry();
        if !registry.is_active() {
            return;
        }
        let sources: AHashSet<&Path> = renames.values().map(PathBuf::as_path).collect();
        let mut events: Vec<RawWatchEvent> = Vec::new();
        let event = |path: &Path, kind, from: Option<&PathBuf>| RawWatchEvent {
            path: path.to_path_buf(),
            kind,
            is_ignored: false,
            from: from.cloned(),
        };
        for (path, _) in &delta.removed {
            if !sources.contains(path.as_path()) {
                events.push(event(path, WatchEventKind::Removed, None));
            }
        }
        for (path, _) in &delta.created {
            match renames.get(path) {
                Some(from) => events.push(event(path, WatchEventKind::Renamed, Some(from))),
                None => events.push(event(path, WatchEventKind::Created, None)),
            }
        }
        for path in &delta.modified {
            match renames.get(path) {
                Some(from) => events.push(event(path, WatchEventKind::Renamed, Some(from))),
                None => events.push(event(path, WatchEventKind::Modified, None)),
            }
        }
        if !events.is_empty() {
            registry.dispatch(&self.base_path, events);
        }
    }

    // Frecency follows renames and AI edits, and git state is announced, whatever became of
    // the index update.
    fn after(
        &self,
        shared: &SharedFilePicker,
        frecency: &SharedFrecency,
        delta: Delta,
        renames: AHashMap<PathBuf, PathBuf>,
        git: GitEvents,
        pending: &Pending,
    ) {
        carry_frecency(shared, frecency, &renames);
        if self.options.mode.is_ai() {
            let written = (delta.created.iter().map(|(path, _)| path)).chain(&delta.modified);
            track_ai_edits(shared, frecency, &self.base_path, written);
        }
        git.changes.announce(shared);
        let workdir = &self.git_workdir;
        let worker = self.git_status_worker.as_ref();
        if !pending.lost_git.is_empty() {
            let within = Some(pending.lost_git.as_slice());
            announce_git_state(
                shared,
                &self.base_path,
                workdir,
                &self.options,
                worker,
                within,
            );
        }
        if pending.git_lost {
            announce_git_state(
                shared,
                &self.base_path,
                workdir,
                &self.options,
                worker,
                None,
            );
        }
    }
}

// A file recorded again: its entry before, whether a change of it is told, and what reading it
// needs of the index.
struct Record {
    path: PathBuf,
    before: Option<Stat>,
    visible: bool,
    slot: RecordSlot,
}

// The listed files of each walk whose entry is missing, differs by its stat, or that the watcher
// named. A smudged entry is read again silently unless its stat tells of a change.
fn plan_walks(
    picker: &FilePicker,
    base_path: &Path,
    walks: &[DirWalk],
    notified: &AHashSet<&Path>,
) -> Vec<Record> {
    let mut records = Vec::new();
    for walk in walks {
        for (rel, stat) in walk.files() {
            let path = base_path.join(rel);
            let before = live_stat(picker, &path);
            let named = notified.contains(path.as_path());
            let changed = match before {
                None => true,
                Some(before) => named || stat.differs_from(&before),
            };
            if !changed {
                continue;
            }
            let visible = before.is_none_or(|before| named || stat.visibly_differs_from(&before));
            let slot = picker.record_slot(&path);
            records.push(Record {
                path,
                before,
                visible,
                slot,
            });
        }
    }
    records
}

// Every file a check names outside the walks, as the watcher named it.
fn plan_checks(picker: &FilePicker, checks: &[Check]) -> Vec<Record> {
    (checks.iter())
        .filter_map(|check| match check {
            Check::File(path) => Some(Record {
                path: path.clone(),
                before: live_stat(picker, path),
                visible: true,
                slot: picker.record_slot(path),
            }),
            _ => None,
        })
        .collect()
}

// Replaces what the index holds below each walked directory with what its walk listed, in one
// pass over the index: unlisted entries leave, and the walk's directories and rules come in.
fn apply_walks(
    picker: &mut FilePicker,
    base_path: &Path,
    walks: &mut [DirWalk],
    rule_dirs: &[PathBuf],
    delta: &mut Delta,
) {
    if walks.is_empty() {
        return;
    }
    // A file the walked directory replaced leaves with its entry, which is not below it.
    for walk in walks.iter() {
        remove_file(picker, &walk.dir, delta);
    }
    let dirs: Vec<&Path> = walks.iter().map(|walk| walk.dir.as_path()).collect();
    let mut removed = Vec::new();
    picker.retain_in_dirs(
        &dirs,
        |owner, rel| walks[owner].lists_file(rel),
        // The root is the one directory a walk does not list.
        |owner, rel| rel.is_empty() || walks[owner].lists_dir(rel),
        |path, file| removed.push((path.to_path_buf(), FileIdentity::of(&file.stat()))),
    );
    delta.removed.append(&mut removed);
    // Rules first: a capacity failure below leaves the files to a rescan, not the rules.
    for walk in walks.iter_mut() {
        let forced = rule_dirs.iter().any(|dir| dir.starts_with(&walk.dir));
        let (rules, sources) = walk.take_rules();
        picker.replace_ignore_rules(&walk.dir, rules, sources, forced);
    }
    for walk in walks.iter() {
        for dir in walk.dirs() {
            let path = base_path.join(dir.trim_end_matches('/'));
            if picker.handle_create_directory(&path) == Err(IndexUpdateError::CapacityExhausted) {
                delta.rejected = true;
                return;
            }
        }
    }
}

// Paths outside every walk that are gone take their entry and every entry below it, as does a
// directory a file now replaces, all directories in one pass over the index.
fn apply_checks(picker: &mut FilePicker, checks: Vec<Check>, delta: &mut Delta) {
    let mut dirs = Vec::new();
    for check in checks {
        match check {
            Check::Gone(path) => {
                if !remove_file(picker, &path, delta) && picker.knows_dir(&path) {
                    dirs.push(path);
                }
            }
            Check::File(path) if picker.knows_dir(&path) => dirs.push(path),
            Check::File(_) | Check::Dir(..) => {}
        }
    }
    if dirs.is_empty() {
        return;
    }
    let mut removed = Vec::new();
    let dirs = dirs.iter().map(PathBuf::as_path);
    picker.remove_all_files_in_dirs_with_callback(dirs, |path, file| {
        removed.push((path.to_path_buf(), FileIdentity::of(&file.stat())));
    });
    delta.removed.append(&mut removed);
}

// Commits the files read beforehand, each only while its entry is still the one it was read
// against: one changed since is left to the next reconcile.
fn commit(picker: &mut FilePicker, prepared: Vec<(Record, PreparedFile)>, delta: &mut Delta) {
    for (record, prepared) in prepared {
        if live_stat(picker, &record.path) != record.before {
            continue;
        }
        let Record {
            path,
            before,
            visible,
            ..
        } = record;
        match picker.commit_prepared(prepared) {
            Ok(file) => {
                let identity = FileIdentity::of(&file.stat());
                match before {
                    None => delta.created.push((path.clone(), identity)),
                    Some(_) if visible => delta.modified.push(path.clone()),
                    Some(_) => {}
                }
                delta.indexed.push(path);
            }
            Err(IndexUpdateError::CapacityExhausted) => {
                delta.rejected = true;
                return;
            }
            // Gone since its stat: its entry, if any, left with it.
            Err(IndexUpdateError::Vanished) => {
                if let Some(before) = before {
                    delta.removed.push((path, FileIdentity::of(&before)));
                }
            }
            Err(IndexUpdateError::OutsideIndex | IndexUpdateError::Stale) => {}
        }
    }
}

// Whether the index held a file at `path`, which it no longer does.
fn remove_file(picker: &mut FilePicker, path: &Path, delta: &mut Delta) -> bool {
    let Some(before) = live_stat(picker, path) else {
        return false;
    };
    picker.remove_file_by_path(path);
    delta
        .removed
        .push((path.to_path_buf(), FileIdentity::of(&before)));
    true
}

fn live_stat(picker: &FilePicker, path: &Path) -> Option<Stat> {
    (picker.get_file_by_path(path))
        .filter(|file| !file.is_deleted())
        .map(FileItem::stat)
}

fn walk_of<'a>(walks: &'a [DirWalk], path: &Path) -> Option<&'a DirWalk> {
    let index = walks.partition_point(|walk| walk.dir.as_path() <= path);
    let walk = walks.get(index.checked_sub(1)?)?;
    path.starts_with(&walk.dir).then_some(walk)
}

// Carries the access history onto the new path. Copied, not moved: checking out a revision
// where the old path exists again must still rank it.
fn carry_frecency(
    shared: &SharedFilePicker,
    frecency: &SharedFrecency,
    renames: &AHashMap<PathBuf, PathBuf>,
) {
    if renames.is_empty() {
        return;
    }
    let mut carried = Vec::new();
    if let Ok(guard) = frecency.read()
        && let Some(tracker) = guard.as_ref()
    {
        for (to, from) in renames {
            match tracker.copy_history(from, to) {
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
    if carried.is_empty() {
        return;
    }
    info!(count = carried.len(), "Carried frecency across renames");
    update_frecency(shared, frecency, &carried);
}

// AI mode: tracks frecency for every file written, once per cooldown, as AI agents often edit
// the same file many times in minutes.
fn track_ai_edits<'a>(
    shared: &SharedFilePicker,
    frecency: &SharedFrecency,
    base_path: &Path,
    written: impl Iterator<Item = &'a PathBuf>,
) {
    let shared_base = (shared.read().ok()).and_then(|guard| {
        guard
            .as_ref()?
            .worktree_shared_base()
            .map(Path::to_path_buf)
    });
    let mut tracked = Vec::new();
    if let Ok(guard) = frecency.read()
        && let Some(tracker) = guard.as_ref()
    {
        for path in written {
            let due = match tracker.seconds_since_last_access(path) {
                Ok(Some(secs)) => secs >= AI_MODE_COOLDOWN_SECS,
                Ok(None) | Err(_) => true,
            };
            if !due {
                continue;
            }
            let shared = (shared_base.as_deref())
                .and_then(|shared_base| crate::scm::shared_twin(path, base_path, shared_base));
            match tracker.track_access_shared(path, shared.as_deref()) {
                Ok(_) => tracked.push(path.clone()),
                Err(e) => error!("Failed to track frecency for {:?}: {:?}", path, e),
            }
        }
    }
    if !tracked.is_empty() {
        info!("AI mode: tracked frecency for {} files", tracked.len());
        update_frecency(shared, frecency, &tracked);
    }
}

fn update_frecency(shared: &SharedFilePicker, frecency: &SharedFrecency, paths: &[PathBuf]) {
    if let Ok(mut guard) = shared.write()
        && let Some(picker) = guard.as_mut()
        && let Ok(frecency) = frecency.read()
        && let Some(tracker) = frecency.as_ref()
    {
        for path in paths {
            let _ = picker.update_single_file_frecency(path, tracker);
        }
    }
}

// Applies a batch as the loop does, but with the given options, work tree and directory hook.
#[cfg(test)]
pub(crate) fn apply_events(
    shared: &SharedFilePicker,
    options: &WatcherOptions,
    git_workdir: Option<&Path>,
    dir_hook: Option<DirHook>,
    events: Vec<notify_debouncer_full::DebouncedEvent>,
) -> Option<RescanReason> {
    let mut context = Context::of(shared)?;
    context.dir_hook = dir_hook.or(context.dir_hook);
    let workspace = git_workdir.and_then(|workdir| ScmBackend::configured().discover(workdir));
    let mut repository = super::repository::LoopRepository::default();
    let repo = repository.at(workspace.as_ref());
    let mut changed = ChangedPaths::new(&context.base_path);
    changed.add(super::intake::intake(shared, &events, repo));
    context.options = options.clone();
    context.git_workdir = git_workdir.map(Path::to_path_buf);
    context.git_backend = workspace.as_ref().map(|workspace| workspace.backend);
    let frecency = SharedFrecency::noop();
    context.reconcile(shared, &frecency, changed.drain(), true, repo)
}

// Test seam: the root of every walk a reconcile ran.
#[cfg(test)]
static WALKED: parking_lot::Mutex<Vec<PathBuf>> = parking_lot::Mutex::new(Vec::new());

#[cfg(test)]
pub(super) fn walks_at_or_below(dir: &Path) -> usize {
    WALKED
        .lock()
        .iter()
        .filter(|root| root.starts_with(dir))
        .count()
}
