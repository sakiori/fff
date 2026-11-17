use std::path::{Path, PathBuf};

use notify::EventKind;
use notify::event::RemoveKind;
use tracing::info;

use super::background_watcher::{WatchTask, WatcherOptions, git_dir_of, is_git_dir};
use super::intake::IgnoreFilter;
use super::ref_watches::{RefChanges, RefSignals};
use super::repository::Repo;
use crate::scm::ScmStatusWorker;
use crate::shared::SharedFilePicker;

// What a set's git events ask: subtrees whose rules an exclude file changed, whether the own
// repository's status is read again, and the git state to announce once the index is updated.
#[derive(Default)]
pub(crate) struct GitEvents {
    pub(crate) rule_dirs: Vec<PathBuf>,
    pub(crate) full_status: bool,
    pub(crate) changes: GitChanges,
}

// Classifies the git events of a set; `filter` tells which work trees the index ignores.
#[allow(clippy::too_many_arguments)]
pub(crate) fn classify(
    shared: &SharedFilePicker,
    events: &[notify::Event],
    base_path: &Path,
    git_workdir: Option<&Path>,
    repo: Option<Repo<'_>>,
    ref_signals: Option<&RefSignals>,
    options: &WatcherOptions,
    filter: &mut IgnoreFilter,
) -> GitEvents {
    let mut git = GitEvents::default();
    if options.ignore_options.disable_git {
        return git;
    }
    // A linked worktree's main repository holds its `info`; the rest of its git dir is no state
    // of the index unless a ref listener follows it.
    let own_common_dir = repo
        .map(|repo| repo.workspace.metadata.as_path())
        .map(|git_dir| (super::common_dir_of(git_dir), git_dir))
        .filter(|(common_dir, git_dir)| common_dir.components().ne(git_dir.components()))
        .map(|(common_dir, _)| common_dir);
    let ref_only = |path: &Path| {
        ref_signals.is_some_and(|signals| signals.ref_only(path, base_path))
            || (own_common_dir.as_deref()).is_some_and(|common_dir| {
                path.parent() == Some(common_dir) && !path.ends_with("info")
            })
    };
    // A work tree the rules ignore; one that is gone is judged by whether the index held it, as
    // intake judges a gone path, since the walkers' rules cannot be asked of it alike.
    let held = |dir: &Path| {
        (shared.read().ok()).is_some_and(|guard| guard.as_ref().is_some_and(|p| p.knows_dir(dir)))
    };
    let mut ignored = |work_tree: &Path| match work_tree.starts_with(base_path)
        && work_tree.symlink_metadata().is_err()
    {
        true => !held(work_tree),
        false => filter.is_ignored(work_tree, true),
    };
    // Refs are looked at only while a listener follows a repository.
    let mut ref_changes = RefChanges::default();
    for event in events {
        let kind = event.kind;
        // A file's removal never takes a repository with it.
        let removed = matches!(
            kind,
            EventKind::Remove(RemoveKind::Folder | RemoveKind::Any | RemoveKind::Other)
        );
        for path in &event.paths {
            if let Some(signals) = ref_signals
                && signals.may_hold(path, base_path)
            {
                signals.classify(path, kind, &mut ignored, &mut ref_changes);
            }
            // A repository whose git dir went is recorded anew when it returns. Linux reports a
            // watched git dir's own removal too.
            if removed {
                shared.forget_git_dirs_under(path);
            }
            let outside = !path.starts_with(base_path);
            if is_dotgit_change_affecting_status(path, repo) {
                git.full_status = true;
                if let Some(workdir) = git_workdir {
                    git.changes.roots.insert(workdir.to_path_buf());
                }
            } else if !ref_only(path)
                && let Some(root) = git_state_root(shared, path, &mut git.changes.follow_refs)
                && git_workdir != Some(root.as_path())
                && root.starts_with(base_path)
                && !ignored(&root)
            {
                // A repository nested in the index, which the walk and its watches reach.
                git.changes.roots.insert(root);
            }
            // An exclude file holds rules for its whole work tree (a linked worktree's lies in the
            // common dir outside); its content decides, as for a `.gitignore`.
            if let Some(work_tree) = excluded_work_tree(shared, path, repo) {
                let work_tree = match base_path.starts_with(&work_tree) {
                    true => base_path.to_path_buf(),
                    false => work_tree,
                };
                let recorded =
                    (shared.read().ok()).and_then(|guard| guard.as_ref()?.ignore_source(path));
                if work_tree.starts_with(base_path)
                    && (work_tree == base_path || !ignored(&work_tree))
                    && crate::walk::IgnoreSource::changed(recorded, path)
                {
                    info!(path = %path.display(), "Exclude rules changed; walking the tree");
                    git.rule_dirs.push(work_tree);
                }
                continue;
            }
            if outside && ref_only(path) {
                continue;
            }
            // A repository created in a directory the walk already reached: its git state gets
            // watches of its own, as a walked one's does.
            if !outside
                && path.file_name().is_some_and(|name| name == ".git")
                && path
                    .parent()
                    .is_some_and(|parent| git_workdir != Some(parent) && !ignored(parent))
                && path.symlink_metadata().is_ok()
                && let Some(work_tree) = path.parent()
            {
                (git.changes.watches).push(GitStateChange::Repository(work_tree.to_path_buf()));
            }
            // A git dir's `logs`, made by HEAD's first update after `git init`, and a hand-made
            // `info` appear after the git dir was watched: each needs a watch of its own.
            for (name, change) in [
                ("logs", GitStateChange::Log as fn(PathBuf) -> GitStateChange),
                ("info", GitStateChange::Info),
            ] {
                if path.file_name().is_some_and(|file| file == name)
                    && let Some(git_dir) = path.parent()
                    && is_git_dir(git_dir)
                    && path.is_dir()
                {
                    git.changes.watches.push(change(git_dir.to_path_buf()));
                }
            }
        }
    }
    if let Some(signals) = ref_signals {
        let mut told = |work_tree: &Path| signals.is_own(work_tree) || !ignored(work_tree);
        let changes = &mut git.changes;
        (changes.ref_roots)
            .extend((ref_changes.work_trees.into_iter()).filter(|work_tree| told(work_tree)));
        (changes.heads_beside)
            .extend((ref_changes.heads_beside.into_iter()).filter(|(_, beside)| told(beside)));
        changes.resync_refs = ref_changes.resync;
        changes.renewed_refs = ref_changes.renewed;
    }
    git
}

impl GitChanges {
    // Test seam: a batch whose ref stores changed, so its ref watches follow them again.
    #[cfg(test)]
    pub(crate) fn resyncing_refs() -> Self {
        Self {
            resync_refs: true,
            ..Self::default()
        }
    }

    // Subscribers hear of git state changes whatever became of the index update.
    pub(crate) fn announce(self, shared: &SharedFilePicker) {
        for root in &self.roots {
            shared.notify_repository_change(root);
        }
        for root in &self.ref_roots {
            shared.notify_ref_change(root);
        }
        for (moved, work_tree) in &self.heads_beside {
            shared.notify_head_beside(moved, work_tree);
        }
        if (!self.watches.is_empty() || self.resync_refs || self.follow_refs)
            && let Some(watches) = shared.installed_watches()
        {
            let announced: Vec<PathBuf> = self.roots.into_iter().collect();
            for change in self.watches {
                watches.request_git_state(change, &announced);
            }
            if self.resync_refs {
                watches.refs.renew(self.renewed_refs);
                watches.send(WatchTask::SyncRefs(Vec::new()));
            } else if self.follow_refs {
                watches.send(WatchTask::FollowRefs(None));
            }
        }
    }
}

// Git state may have changed unseen: the status is read again, repositories are announced and ref
// watches renewed; `within` keeps those whose git dir or common dir is related to its paths.
pub(crate) fn announce_git_state(
    shared_picker: &SharedFilePicker,
    base_path: &Path,
    git_workdir: &Option<PathBuf>,
    options: &WatcherOptions,
    git_status_worker: &dyn ScmStatusWorker,
    within: Option<&[PathBuf]>,
) {
    if options.ignore_options.disable_git {
        return;
    }
    let related = |dir: &Path| {
        within.is_none_or(|paths| {
            (paths.iter()).any(|path| path.starts_with(dir) || dir.starts_with(path))
        })
    };
    let registered: Vec<_> = (shared_picker.registered_git_dirs().into_iter())
        .filter(|known| known.work_tree.starts_with(base_path))
        .filter(|known| related(&known.git_dir) || related(&known.common_dir))
        .collect();
    let own = (git_workdir.as_ref()).filter(|work_tree| {
        within.is_none()
            || registered
                .iter()
                .any(|known| known.work_tree == **work_tree)
    });
    if own.is_some() {
        git_status_worker.request_full_rescan();
    }
    let followed = (shared_picker.installed_watches())
        .filter(|_| within.is_none())
        .map(|watches| watches.refs.followed_work_trees())
        .unwrap_or_default();
    let mut work_trees: Vec<PathBuf> = (registered.iter().map(|known| &known.work_tree))
        .chain(own)
        .chain(&followed)
        .map(|work_tree| work_tree.components().collect())
        .collect();
    if work_trees.is_empty() {
        return;
    }
    info!(
        ?work_trees,
        "Announcing git state that may have changed unseen"
    );
    work_trees.sort_unstable();
    work_trees.dedup();
    for work_tree in &work_trees {
        shared_picker.notify_repository_change(work_tree);
    }
    // Ref listeners hear once, from the watcher's thread, with the ref stores renewed meanwhile.
    let synced = (shared_picker.installed_watches()).is_some_and(|watches| {
        let stores = (registered.iter())
            .flat_map(|known| [known.git_dir.clone(), known.common_dir.clone()])
            .chain(git_workdir.as_deref().and_then(git_dir_of))
            .collect();
        watches.refs.renew(stores);
        watches.send(WatchTask::SyncRefs(work_trees.clone()))
    });
    if !synced {
        for work_tree in &work_trees {
            shared_picker.notify_ref_change(work_tree);
        }
    }
}

// What a batch tells of git state: work trees whose state or refs changed, git dirs and logs to
// watch, and whether the ref watches follow a ref store or git dir the batch registered.
#[derive(Default)]
pub(crate) struct GitChanges {
    roots: ahash::AHashSet<PathBuf>,
    ref_roots: ahash::AHashSet<PathBuf>,
    watches: Vec<GitStateChange>,
    resync_refs: bool,
    follow_refs: bool,
    renewed_refs: Vec<PathBuf>,
    // See `RefChanges`.
    heads_beside: ahash::AHashSet<(PathBuf, PathBuf)>,
}

pub(crate) enum GitStateChange {
    // A `.git` appeared in this directory, which the walk had already reached.
    Repository(PathBuf),
    // This git dir gained its `logs`.
    Log(PathBuf),
    // This git dir gained its `info`.
    Info(PathBuf),
}

// The work tree whose exclude file `path` is: the index's own repository's, or a registered one's.
fn excluded_work_tree(
    picker: &SharedFilePicker,
    path: &Path,
    repo: Option<Repo<'_>>,
) -> Option<PathBuf> {
    if !path.ends_with("info/exclude") {
        return None;
    }
    let git_dir = path.parent()?.parent()?;
    if let Some(repo) = repo {
        let own = repo.workspace.metadata.as_path();
        let common_dir = super::common_dir_of(own);
        if own.components().eq(git_dir.components())
            || common_dir.components().eq(git_dir.components())
        {
            return Some(repo.workspace.workdir.components().collect());
        }
    }
    picker.registered_work_tree(git_dir)
}

#[inline]
pub(crate) fn is_git_file(path: &Path) -> bool {
    // it could be in submodule
    crate::scm::is_metadata_path(path, None)
}

pub(super) fn is_dotgit_change_affecting_status(changed: &Path, repo: Option<Repo<'_>>) -> bool {
    repo.is_some_and(|repo| {
        (repo.workspace.git_path(changed))
            .strip_prefix(&repo.workspace.metadata)
            .is_ok_and(affects_worktree_state)
    })
}

// The work tree whose state `changed` affects: of a registered git dir, or of the nearest one by
// shape inside a `.git` directory, as linked worktrees' sit; `registered` is set if it is new.
fn git_state_root(
    picker: &SharedFilePicker,
    changed: &Path,
    registered: &mut bool,
) -> Option<PathBuf> {
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
        foreign_work_tree(git_dir)?
    };
    *registered |= picker.register_git_dir(git_dir, &work_tree);
    Some(work_tree)
}

pub(super) fn affects_worktree_state(path_in_git_dir: &Path) -> bool {
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

// The work tree of a git dir found by shape inside a `.git`; only libgit2 opens one here, so
// builds without it announce such a repository once the walk registers it.
#[cfg(feature = "libgit2")]
fn foreign_work_tree(git_dir: &Path) -> Option<PathBuf> {
    let repo = git2::Repository::open(git_dir).ok()?;
    crate::path_utils::canonicalize(repo.workdir()?).ok()
}

#[cfg(not(feature = "libgit2"))]
fn foreign_work_tree(_git_dir: &Path) -> Option<PathBuf> {
    None
}
