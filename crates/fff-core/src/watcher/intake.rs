use std::path::{Path, PathBuf};
use std::sync::Arc;

use ahash::AHashSet;
use git2::Repository;
use notify::EventKind;
use notify::event::{
    AccessKind, AccessMode, CreateKind, MetadataKind, ModifyKind, RemoveKind, RenameMode,
};
use notify_debouncer_full::DebouncedEvent;
#[cfg(test)]
use parking_lot::Mutex;
use tracing::warn;

use super::changed_paths::Change;
use super::git_changes::is_git_file;
use crate::file_picker::FilePickerIgnoreOptions;
use crate::shared::SharedFilePicker;

// Turns events into changed paths as Watchman does: the kind hints at a walk, the reconcile
// decides. Paths below ignored dirs never count, git state goes apart; `repo` serves a first scan.
pub(crate) fn intake(
    shared: &SharedFilePicker,
    events: &[DebouncedEvent],
    repo: Option<&Repository>,
) -> Vec<Change> {
    let Some(context) = Context::of(shared, repo) else {
        return Vec::new();
    };
    let named = context.named(shared, events);
    context.filtered(shared, named)
}

struct Context<'a> {
    base_path: PathBuf,
    rules: Arc<crate::walk::IgnoreRules>,
    repo: Option<&'a Repository>,
    ignore_options: FilePickerIgnoreOptions,
    ref_signals: Option<super::ref_watches::RefSignals>,
}

// A path of the tree the watcher named, and why its events were lost if they were.
struct Named {
    path: PathBuf,
    kind: EventKind,
    lost: Option<String>,
}

impl<'a> Context<'a> {
    fn of(shared: &SharedFilePicker, repo: Option<&'a Repository>) -> Option<Self> {
        let guard = shared.read().ok()?;
        let picker = guard.as_ref()?;
        let watches = (picker.background_watcher.as_ref()).map(|watcher| watcher.watches());
        let mut context = Self {
            base_path: picker.base_path().to_path_buf(),
            rules: picker.ignore_rules(),
            repo,
            ignore_options: picker.ignore_options().clone(),
            ref_signals: None,
        };
        // The ref state is a leaf lock, read once the picker's is released.
        drop(guard);
        context.ref_signals = watches.and_then(|watches| watches.refs.signals());
        Some(context)
    }

    // Every change the events stand for, the tree's paths still unfiltered.
    fn named(
        &self,
        shared: &SharedFilePicker,
        events: &[DebouncedEvent],
    ) -> Vec<Result<Change, Named>> {
        let own_git_dir = self.repo.map(Repository::path);
        let mut changes = Vec::new();
        for event in events
            .iter()
            .filter(|event| !is_read_access(event.event.kind))
        {
            // Lost events have their subtree read again, the root's for the root, above or none
            // (Apple's FSEvents guide, Watchman's recursive pending paths, JetBrains' RECDIRTY).
            if event.need_rescan() {
                changes.extend(self.lost(&event.event, own_git_dir));
                continue;
            }
            let kind = event.event.kind;
            if let EventKind::Modify(ModifyKind::Name(RenameMode::Both)) = kind
                && let [from, to] = event.event.paths.as_slice()
                && !self.is_git_path(from, own_git_dir)
                && !self.is_git_path(to, own_git_dir)
            {
                changes.push(Ok(Change::Renamed(from.clone(), to.clone())));
            }
            for path in &event.event.paths {
                // The root itself replaced, as by a checkout that swaps it, is walked again.
                if *path == self.base_path && may_change_a_subtree(kind) {
                    changes.push(Ok(Change::Root));
                }
                if self.base_path.starts_with(path) {
                    continue;
                }
                if self.is_git_path(path, own_git_dir) {
                    // Objects never tell of a repository's state.
                    let objects = (path.components()).any(|part| part.as_os_str() == "objects");
                    let foreign = (self.ref_signals.as_ref())
                        .is_some_and(|signals| signals.foreign_admin_file(path, &self.base_path));
                    if !objects && !foreign {
                        let mut single = event.event.clone();
                        single.paths = vec![path.clone()];
                        changes.push(Ok(Change::Git(single)));
                    }
                    continue;
                }
                // A repository whose work tree went, or a directory above it, is recorded anew
                // when it returns, also when the path is back already.
                if matches!(
                    kind,
                    EventKind::Remove(RemoveKind::Folder | RemoveKind::Any | RemoveKind::Other)
                ) {
                    shared.forget_git_dirs_under(path);
                }
                changes.push(Err(Named {
                    path: path.clone(),
                    kind,
                    lost: None,
                }));
            }
        }
        changes
    }

    // Drops paths and lost events below a directory the rules ignore and the index lacks, as
    // Watchman does; whether a path is ignored or still there, the reconcile decides by its stat.
    fn filtered(
        &self,
        shared: &SharedFilePicker,
        changes: Vec<Result<Change, Named>>,
    ) -> Vec<Change> {
        let held: AHashSet<PathBuf> = (shared.read().ok())
            .map(|guard| {
                let Some(picker) = guard.as_ref() else {
                    return AHashSet::new();
                };
                // Parents, and the directories lost events name.
                (changes.iter())
                    .filter_map(|change| change.as_ref().err())
                    .flat_map(|named| {
                        let lost = named.lost.as_ref().map(|_| named.path.as_path());
                        named.path.parent().into_iter().chain(lost)
                    })
                    .filter(|dir| picker.knows_dir(dir))
                    .map(Path::to_path_buf)
                    .collect()
            })
            .unwrap_or_default();
        let mut filter = IgnoreFilter::new(
            &self.base_path,
            Arc::clone(&self.rules),
            self.repo,
            self.ignore_options.disable_git,
        );
        filter.heuristic = false;
        // A directory the rules ignore and the index does not hold. Batches name many paths of
        // one directory in a row.
        let mut last_dir: Option<(PathBuf, bool)> = None;
        let mut ignored_dir = |dir: &Path| match &last_dir {
            Some((known, ignored)) if known == dir => *ignored,
            _ => {
                let ignored = !held.contains(dir) && filter.is_ignored(dir, true);
                last_dir = Some((dir.to_path_buf(), ignored));
                ignored
            }
        };
        let mut kept = Vec::with_capacity(changes.len());
        for change in changes {
            let named = match change {
                Ok(change) => {
                    kept.push(change);
                    continue;
                }
                Err(named) => named,
            };
            if named.path.parent().is_none_or(&mut ignored_dir) {
                continue;
            }
            if let Some(reason) = named.lost {
                // Loss in an ignored directory is dropped as any other change there.
                if ignored_dir(&named.path) {
                    continue;
                }
                warn!(path = %named.path.display(), reason, "Recrawling after lost events");
                #[cfg(test)]
                RECRAWLED.lock().push(named.path.clone());
                kept.push(Change::Lost(named.path));
            } else if self.is_ignore_file(&named.path) {
                // Its content may change the rules, whatever they say of it.
                kept.push(Change::IgnoreFile(named.path));
            } else {
                kept.push(Change::Path {
                    recursive: may_change_a_subtree(named.kind),
                    path: named.path,
                });
            }
        }
        kept
    }

    // Logs each recrawl once, with its path and reason; a path of the tree is filtered as any
    // other named path first, then walked again whatever it is now.
    fn lost(
        &self,
        event: &notify::Event,
        own_git_dir: Option<&Path>,
    ) -> Vec<Result<Change, Named>> {
        let reason = recrawl_reason(event);
        if event.paths.is_empty() {
            warn!(path = %self.base_path.display(), reason, "Recrawling after lost events");
            #[cfg(test)]
            RECRAWLED.lock().push(self.base_path.clone());
            return vec![Ok(Change::Root)];
        }
        let mut lost = Vec::with_capacity(event.paths.len());
        for path in &event.paths {
            let change = if self.base_path.starts_with(path) {
                Ok(Change::Root)
            } else if self.is_git_path(path, own_git_dir) {
                Ok(Change::LostGit(path.clone()))
            } else {
                lost.push(Err(Named {
                    path: path.clone(),
                    kind: EventKind::Other,
                    lost: Some(reason.to_owned()),
                }));
                continue;
            };
            warn!(path = %path.display(), reason, "Recrawling after lost events");
            #[cfg(test)]
            RECRAWLED.lock().push(path.clone());
            lost.push(change);
        }
        lost
    }

    // Bare repositories keep worktree git dirs outside of any `.git` component.
    fn is_git_path(&self, path: &Path, own_git_dir: Option<&Path>) -> bool {
        !path.starts_with(&self.base_path)
            || is_git_file(path)
            || own_git_dir.is_some_and(|dir| path.starts_with(dir))
    }

    fn is_ignore_file(&self, path: &Path) -> bool {
        match path.file_name().and_then(|name| name.to_str()) {
            Some(".ignore") => !self.ignore_options.disable_ignore_files,
            Some(".gitignore") => !self.ignore_options.disable_git,
            _ => false,
        }
    }
}

// A directory created, removed, renamed, or given other permissions or owner may hold anything
// a walk can read now; a write or a children's time change alters nothing below a path.
fn may_change_a_subtree(kind: EventKind) -> bool {
    match kind {
        EventKind::Create(CreateKind::File) | EventKind::Remove(RemoveKind::File) => false,
        EventKind::Create(_) | EventKind::Remove(_) => true,
        EventKind::Modify(ModifyKind::Name(_)) => true,
        EventKind::Modify(ModifyKind::Metadata(kind)) => matches!(
            kind,
            MetadataKind::Any
                | MetadataKind::Permissions
                | MetadataKind::Ownership
                | MetadataKind::Other
        ),
        _ => false,
    }
}

// The ignore rules of the last walk and the subtrees walked again since, else libgit2's.
pub(crate) struct IgnoreFilter<'a> {
    base_path: &'a Path,
    rules: Arc<crate::walk::IgnoreRules>,
    // Borrowed from the caller, which uses it for git state as well.
    repo: Option<&'a Repository>,
    disable_git: bool,
    // Whether the non-git heuristic decides where neither rules nor a repository do.
    pub(crate) heuristic: bool,
}

impl<'a> IgnoreFilter<'a> {
    pub(crate) fn new(
        base_path: &'a Path,
        rules: Arc<crate::walk::IgnoreRules>,
        repo: Option<&'a Repository>,
        disable_git: bool,
    ) -> Self {
        Self {
            base_path,
            rules,
            repo,
            disable_git,
            heuristic: true,
        }
    }

    pub(crate) fn is_ignored(&mut self, path: &Path, is_dir: bool) -> bool {
        let Ok(relative) = path.strip_prefix(self.base_path) else {
            return false;
        };
        if let Some(ignored) = Arc::make_mut(&mut self.rules).is_ignored(relative, is_dir) {
            return ignored;
        }
        match self.repo {
            Some(repo) => repo.is_path_ignored(path) == Ok(true),
            None if self.disable_git || !self.heuristic => false,
            // No repo and no rules: the non-code-dir heuristic, applied to the
            // base-relative path so ancestors of the base (e.g. a temp dir
            // under AppData/Local on Windows) never match.
            None => crate::ignore::is_non_code_directory(relative),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathState {
    // Gone, or nothing the index holds: not a regular file, or a symlink it does not follow.
    Gone,
    File,
    Dir,
}

impl PathState {
    // One lstat, and a stat of the target only for a symlink followed. Only a regular file is a
    // file, as for a scan: a FIFO, socket or device is never indexed, so never opened.
    pub(crate) fn of(path: &Path, follow_symlinks: bool) -> Self {
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return Self::Gone;
        };
        let meta = match meta.file_type().is_symlink() {
            true if !follow_symlinks => return Self::Gone,
            true => match std::fs::metadata(path) {
                Ok(target) => target,
                Err(_) => return Self::Gone,
            },
            false => meta,
        };
        match (meta.is_dir(), meta.is_file()) {
            (true, _) => Self::Dir,
            (_, true) => Self::File,
            _ => Self::Gone,
        }
    }
}

// Why a watcher asks for a recrawl: notify's reason for dropped FSEvents, else coalesced
// events for a path, or an overflowed queue (inotify, Windows) without one.
fn recrawl_reason(event: &notify::Event) -> &str {
    event.info().unwrap_or(if event.paths.is_empty() {
        "queue overflow"
    } else {
        "coalesced (must scan subdirs)"
    })
}

// Reads, our own previews and greps among them, change nothing.
fn is_read_access(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::Access(
            AccessKind::Read
                | AccessKind::Open(_)
                | AccessKind::Close(AccessMode::Read | AccessMode::Execute)
        )
    )
}

// Test seam: every path a recrawl was logged for.
#[cfg(test)]
static RECRAWLED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn recrawls_logged(path: &Path) -> usize {
    RECRAWLED
        .lock()
        .iter()
        .filter(|logged| *logged == path)
        .count()
}
