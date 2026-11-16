use std::path::{Path, PathBuf};

use git2::Repository;

// The loop's one handle on its index's repository, opened again when the work tree changes or
// its git dir went: intake and reconcile ask it instead of opening the repository per batch.
#[derive(Default)]
pub(super) struct LoopRepository {
    workdir: Option<PathBuf>,
    repo: Option<Repository>,
}

impl LoopRepository {
    pub(super) fn at(&mut self, workdir: Option<&Path>) -> Option<&Repository> {
        let stale = (self.repo.as_ref()).is_some_and(|repo| !repo.path().exists());
        if stale || self.workdir.as_deref() != workdir {
            self.workdir = workdir.map(Path::to_path_buf);
            self.repo = workdir.and_then(|workdir| Repository::open(workdir).ok());
        }
        self.repo.as_ref()
    }
}
