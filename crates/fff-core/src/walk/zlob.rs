use crate::file_picker::{
    is_ambiguous_binary_name, is_known_binary_extension_basename, starts_with_binary_content,
};
use crate::ignore::ignored_dir_globs;
use crate::types::FileItem;
use crate::walk::{DirHook, WalkIgnoreRules, WalkOutput, WalkPolicy};
use parking_lot::Mutex;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;
use zlob::walk::{WalkBuilder, WalkFlags, WalkMetadata, WalkState};

const PROGRESS_STEP: usize = 13;

#[tracing::instrument(skip_all, name = "zlob walker", level = "info")]
pub(crate) fn walk_collect_files(
    base_path: &Path,
    subtree: Option<&Path>,
    is_git_repo: bool,
    policy: WalkPolicy<'_>,
    threads: usize,
    synced_files_count: &Arc<AtomicUsize>,
    on_dir: Option<&DirHook>,
) -> crate::Result<WalkOutput> {
    let WalkPolicy {
        follow_symlinks,
        ignore: ignore_options,
    } = policy;
    let use_non_git_defaults = !is_git_repo && !ignore_options.disable_git;
    let mut flags = WalkFlags::GITIGNORE;
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
        .metadata(WalkMetadata::SIZE | WalkMetadata::MTIME | WalkMetadata::INODE);

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

    // Single lock for both collections: every entry is either a file or a
    // dir, so this keeps one mutex acquisition per entry.
    let collected = Mutex::new((Vec::new(), Vec::new()));

    let subtree = subtree
        .map(|subtree| super::subtree_relative(base_path, subtree))
        .transpose()?;
    let outcome = match builder.run(|entry| {
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
                let mut relative_path = relative_path.into_owned();
                relative_path.push('/');
                collected.lock().1.push(relative_path);
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
        let is_binary = is_known_binary_extension_basename(basename)
            && (!is_ambiguous_binary_name(basename) || starts_with_binary_content(entry.path()));

        let (size, modified, ino) = match target {
            Some(meta) => (
                meta.len(),
                meta.modified()
                    .ok()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs()),
                crate::types::inode_bits(&meta),
            ),
            // zlob reports mtime in ns since the Unix epoch; FileItem wants secs.
            None => (
                entry.size().unwrap_or(0),
                entry
                    .modified_ns()
                    .map(|ns| (ns / 1_000_000_000).max(0) as u64)
                    .unwrap_or(0),
                entry.inode().map_or(0, |ino| ino as u32),
            ),
        };

        let mut item = FileItem::new_raw(basename_offset as u16, size, modified, None, is_binary);
        item.ino = ino;
        item.mark_definition_language(basename);

        let mut guard = collected.lock();
        guard.0.push((item, relative_path));
        let n = guard.0.len();
        drop(guard);

        if n % PROGRESS_STEP == 0 {
            synced_files_count.store(n, Ordering::Relaxed);
        }

        WalkState::Continue
    }) {
        Ok(outcome) => outcome,
        Err(e) => {
            // Preserve whatever we collected before the failure so the caller
            // can still surface a partial index instead of nothing.
            tracing::error!(?e, "zlob walk failed");
            return Err(crate::Error::WalkFailed(format!("{e:?}")));
        }
    };

    let (pairs, dirs) = collected.into_inner();
    // Always report the exact final total regardless of the last step.
    synced_files_count.store(pairs.len(), Ordering::Relaxed);

    // Retain the ignore rules only when the walk actually gathered some
    // (git roots with .gitignore/.ignore). Otherwise callers fall back.
    let ignore_rules = outcome
        .rules()
        .is_some()
        .then(|| WalkIgnoreRules { inner: outcome });

    Ok(WalkOutput {
        pairs,
        dirs,
        ignore_rules,
    })
}
