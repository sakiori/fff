use crate::file_picker::{
    is_ambiguous_binary_name, is_known_binary_extension_basename, starts_with_binary_content,
};
use crate::ignore::ignored_dir_globs;
use crate::types::FileItem;
use crate::walk::{WalkHooks, WalkIgnoreRules, WalkOutput, WalkPolicy};
use parking_lot::Mutex;
use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use zlob::walk::{WalkBuilder, WalkFlags, WalkMetadata, WalkState};

const PROGRESS_STEP: usize = 13;

// Keep adjacent workers' locks and vector headers on separate cache lines.
#[repr(align(128))]
#[derive(Default)]
struct WorkerFiles {
    pairs: Vec<(FileItem, String)>,
    dirs: Vec<String>,
}

#[tracing::instrument(skip_all, name = "zlob walker", level = "info")]
pub(super) fn walk_files(
    base_path: &Path,
    subtree: Option<&Path>,
    backend: Option<crate::scm::ScmBackend>,
    policy: WalkPolicy<'_>,
    threads: usize,
    synced_files_count: &Arc<AtomicUsize>,
    hooks: WalkHooks<'_>,
) -> crate::Result<WalkOutput> {
    let on_dir = hooks.on_dir;
    let WalkPolicy {
        follow_symlinks,
        ignore: ignore_options,
    } = policy;
    let use_non_git_defaults = backend.is_none() && !ignore_options.disable_git;
    let mut flags = WalkFlags::GITIGNORE;
    // An index rooted below its work tree takes the rules above it, the repository's exclude
    // file included, as ripgrep's walker does; zlob would list nothing of a root they ignore.
    if let Some(backend) = backend
        && !ignore_options.disable_git
        && !root_ignored(backend, base_path)
    {
        flags |= WalkFlags::GITIGNORE_PARENTS;
    }
    if ignore_options.disable_git {
        flags |= WalkFlags::SKIP_GITIGNORE | WalkFlags::NO_GIT_EXCLUDE | WalkFlags::NO_GIT_GLOBAL;
    }
    if ignore_options.disable_ignore_files {
        flags |= WalkFlags::SKIP_DOT_IGNORE;
    }
    if use_non_git_defaults {
        flags |= WalkFlags::SKIP_HIDDEN;
    }
    if follow_symlinks {
        flags |= WalkFlags::FOLLOW_SYMLINKS;
    }

    let mut builder = WalkBuilder::new(base_path)
        .map_err(|e| crate::Error::WalkFailed(format!("WalkBuilder::new: {e:?}")))?;
    builder
        .options(flags)
        .threads(threads)
        // Bulk-fetch the only metadata FileItem needs; zlob never stats more.
        .metadata(
            WalkMetadata::SIZE | WalkMetadata::MTIME | WalkMetadata::CTIME | WalkMetadata::INODE,
        );

    let extra_ignore = &ignore_options.extra_ignore;
    if use_non_git_defaults || !extra_ignore.is_empty() {
        let defaults = use_non_git_defaults
            .then(ignored_dir_globs)
            .into_iter()
            .flatten();
        let rules: Vec<String> = defaults.chain(extra_ignore.iter().cloned()).collect();
        builder
            .extra_ignore(&rules)
            .map_err(|e| crate::Error::WalkFailed(format!("invalid extra ignore rules: {e:?}")))?;
    }

    let collected: Vec<Mutex<WorkerFiles>> = (0..builder.max_workers())
        .map(|_| Mutex::default())
        .collect();
    synced_files_count.store(0, Ordering::Relaxed);

    let subtree = subtree
        .map(|subtree| super::subtree_relative(base_path, subtree))
        .transpose()?;
    // zlob already excludes .git during traversal.
    let extra_metadata = backend
        .map(|backend| backend.metadata_dir_name())
        .filter(|name| *name != ".git");
    let outcome = match builder.run(|entry| {
        if hooks.cancelled() {
            return WalkState::Quit;
        }
        if extra_metadata.is_some_and(|name| {
            entry
                .path()
                .components()
                .any(|component| component.as_os_str() == name)
        }) {
            return WalkState::Continue;
        }
        let collected = &collected[entry.worker_id()];
        if ignore_options.disable_git && entry.path().file_name().is_some_and(|name| name == ".git")
        {
            return WalkState::SkipDir;
        }
        let relative_path = entry.relative_path_lossy();
        if let Some(subtree) = subtree {
            let path = Path::new(relative_path.as_ref());
            if !path.starts_with(subtree) {
                // Descend only into the ancestors leading to the subtree.
                return if subtree.starts_with(path) {
                    WalkState::Continue
                } else {
                    WalkState::SkipDir
                };
            }
        }
        // Followed links keep the Symlink kind and lstat metadata, so stat
        // the target to index the ones resolving to files (#912).
        let target = if entry.is_file() {
            None
        } else if follow_symlinks && entry.is_symlink() {
            match std::fs::metadata(entry.path()) {
                Ok(meta) if meta.is_file() => Some(meta),
                _ => return WalkState::Continue,
            }
        } else {
            // unlike ripgrep walker zlob doesnt show .git files
            if entry.is_dir() && !relative_path.is_empty() {
                // Reported while the parent is listed, so before this directory is read.
                if let Some(on_dir) = on_dir {
                    on_dir(&base_path.join(relative_path.as_ref()));
                }
                let mut relative_path = match relative_path {
                    Cow::Borrowed(path) => {
                        let mut relative_path = String::with_capacity(path.len() + 1);
                        relative_path.push_str(path);
                        relative_path
                    }
                    Cow::Owned(path) => path,
                };
                relative_path.push('/');
                collected.lock().dirs.push(relative_path);
            }

            return WalkState::Continue;
        };

        // Lossy pair: the offset must index the decoded string, not the raw
        // bytes, or it lands inside a U+FFFD on invalid-UTF-8 names (#799).
        let basename_offset = entry.basename_offset_in_relative_lossy();
        // zlob emits '/'-separated relative paths, which is fff's canonical
        // internal form on every platform — store them verbatim.
        let relative_path = relative_path.into_owned();

        // `basename()` returns `&str` for files only.
        let basename = match target {
            None => entry.basename().unwrap_or(""),
            Some(_) => &relative_path[basename_offset..],
        };
        if basename == ".git" {
            return WalkState::Continue;
        }
        let is_binary = is_known_binary_extension_basename(basename)
            && (!is_ambiguous_binary_name(basename) || starts_with_binary_content(entry.path()));

        let stat = match target {
            Some(meta) => crate::stat::Stat::of(&meta),
            None => stat_of_entry(&entry),
        };

        let mut item = FileItem::new_raw(
            basename_offset as u16,
            stat.size,
            stat.modified,
            None,
            is_binary,
        );
        item.set_stat(stat);
        item.mark_definition_language(basename);

        let mut guard = collected.lock();
        guard.pairs.push((item, relative_path));
        let n = guard.pairs.len();
        drop(guard);

        if n.is_multiple_of(PROGRESS_STEP) {
            synced_files_count.fetch_add(PROGRESS_STEP, Ordering::Relaxed);
        }

        WalkState::Continue
    }) {
        Ok(outcome) => outcome,
        Err(e) => {
            tracing::error!(?e, "zlob walk failed");
            return Err(crate::Error::WalkFailed(format!("{e:?}")));
        }
    };

    let collected: Vec<_> = collected.into_iter().map(Mutex::into_inner).collect();
    let mut pairs = Vec::with_capacity(collected.iter().map(|shard| shard.pairs.len()).sum());
    let mut dirs = Vec::with_capacity(collected.iter().map(|shard| shard.dirs.len()).sum());
    for mut shard in collected {
        pairs.append(&mut shard.pairs);
        dirs.append(&mut shard.dirs);
    }
    // Always report the exact final total regardless of the last step.
    synced_files_count.store(pairs.len(), Ordering::Relaxed);

    let ignore_rules =
        (use_non_git_defaults || outcome.rules().is_some()).then_some(WalkIgnoreRules {
            inner: outcome,
            skip_hidden: use_non_git_defaults,
            root: base_path.to_path_buf(),
        });

    Ok(WalkOutput {
        pairs,
        dirs,
        ignore_rules,
        ignore_sources: Vec::new(),
    })
}

// As `Stat::of` reads it from a stat; zlob reports times in ns since the Unix epoch.
fn stat_of_entry(entry: &zlob::walk::WalkEntry<'_>) -> crate::stat::Stat {
    let modified = entry.modified_ns().unwrap_or(0);
    let (stamp, inode) = match (cfg!(unix), entry.inode(), entry.changed_ns()) {
        (true, Some(inode), Some(changed)) => (
            crate::stat::stamp_of(inode, changed),
            crate::stat::inode_of(inode),
        ),
        _ => (0, 0),
    };
    crate::stat::Stat {
        size: entry.size().unwrap_or(0),
        modified: (modified / 1_000_000_000).max(0) as u64,
        nanos: modified.rem_euclid(1_000_000_000) as u32,
        stamp,
        inode,
    }
}

// Whether the repository around `base_path`, which lies below its work tree, ignores it.
fn root_ignored(backend: crate::scm::ScmBackend, base_path: &Path) -> bool {
    let Some(workspace) = backend.discover(base_path) else {
        return false;
    };
    let base = workspace.git_path(base_path);
    let below = *base != workspace.workdir && base.starts_with(&workspace.workdir);
    below && workspace.open().is_ok_and(|repo| repo.is_ignored(&base))
}
