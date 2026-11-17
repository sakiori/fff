use crate::stat::Stat;
use crate::walk::{IgnoreSource, WalkHooks, WalkIgnoreRules, WalkPolicy};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use tracing::warn;

// What a walk under the index's root policy lists in a directory, taken outside the picker lock.
pub(super) struct DirWalk {
    pub(super) dir: PathBuf,
    // Relative and '/'-canonical, sorted; dirs end with '/'.
    files: Vec<(String, Stat)>,
    dirs: Vec<String>,
    rules: Option<WalkIgnoreRules>,
    sources: Vec<(PathBuf, IgnoreSource)>,
}

impl DirWalk {
    // `None` when the walk could not run at all, or stopped as its index was cancelled.
    pub(super) fn new(
        dir: &Path,
        base_path: &Path,
        backend: Option<crate::scm::ScmBackend>,
        policy: WalkPolicy<'_>,
        hooks: WalkHooks<'_>,
    ) -> Option<Self> {
        let walk = match crate::walk::walk_collect_files(
            base_path,
            Some(dir),
            backend,
            policy,
            walk_threads(),
            &Arc::new(AtomicUsize::new(0)),
            hooks,
        ) {
            Ok(walk) => walk,
            Err(_) if hooks.cancelled() => return None,
            Err(e) => {
                warn!(?e, dir = %dir.display(), "Failed to walk directory");
                return None;
            }
        };
        let mut files: Vec<(String, Stat)> = (walk.pairs.into_iter())
            .map(|(file, rel)| (rel, file.stat()))
            .collect();
        let mut dirs = walk.dirs;
        files.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        dirs.sort_unstable();
        Some(Self {
            dir: dir.to_path_buf(),
            files,
            dirs,
            rules: walk.ignore_rules,
            sources: walk.ignore_sources,
        })
    }

    // The rules this walk read, and what each ignore file at or below its directory held.
    pub(super) fn take_rules(&mut self) -> (Option<WalkIgnoreRules>, Vec<(PathBuf, IgnoreSource)>) {
        (self.rules.take(), std::mem::take(&mut self.sources))
    }

    pub(super) fn files(&self) -> &[(String, Stat)] {
        &self.files
    }

    pub(super) fn dirs(&self) -> &[String] {
        &self.dirs
    }

    pub(super) fn lists_file(&self, rel: &str) -> bool {
        (self.files)
            .binary_search_by(|(file, _)| file.as_str().cmp(rel))
            .is_ok()
    }

    pub(super) fn lists_dir(&self, rel: &str) -> bool {
        self.dirs
            .binary_search_by(|dir| dir.as_str().cmp(rel))
            .is_ok()
    }
}

// As many as a scan's walk: a root reconcile replaces one. The walkers run their own threads, so
// the loop waits for its walk alone and parks no pool thread.
fn walk_threads() -> usize {
    crate::parallelism::indexing_pool().current_num_threads()
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_reconcile_walks_with_the_parallelism_of_a_scan() {
        let scan = crate::parallelism::indexing_pool().current_num_threads();
        assert_eq!(super::walk_threads(), scan);
    }
}
