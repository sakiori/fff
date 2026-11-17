use crate::scm::{ScmRepository, ScmWorkspace};

// The loop's one handle on its index's repository, opened again when the work tree changes or
// its git dir went: intake and reconcile ask it instead of opening the repository per batch.
#[derive(Default)]
pub(super) struct LoopRepository {
    workspace: Option<ScmWorkspace>,
    repo: Option<ScmRepository>,
}

impl LoopRepository {
    pub(super) fn at(&mut self, workspace: Option<&ScmWorkspace>) -> Option<Repo<'_>> {
        let stale = (self.workspace.as_ref())
            .is_some_and(|open| self.repo.is_some() && !open.metadata.exists());
        let workdir = |workspace: Option<&ScmWorkspace>| workspace.map(|w| w.workdir.clone());
        if stale || workdir(self.workspace.as_ref()) != workdir(workspace) {
            self.workspace = workspace.cloned();
            self.repo = workspace.and_then(|workspace| workspace.open().ok());
        }
        Some(Repo {
            workspace: self.workspace.as_ref()?,
            repo: self.repo.as_ref()?,
        })
    }
}

// An open repository with the workspace it was opened for, which maps the index's paths to git's.
#[derive(Clone, Copy)]
pub(crate) struct Repo<'a> {
    pub(crate) workspace: &'a ScmWorkspace,
    pub(crate) repo: &'a ScmRepository,
}

impl Repo<'_> {
    pub(crate) fn is_ignored(&self, path: &std::path::Path) -> bool {
        self.repo.is_ignored(&self.workspace.git_path(path))
    }

    // Whether `path` lies in the repository's git dir.
    pub(crate) fn holds_git_path(&self, path: &std::path::Path) -> bool {
        (self.workspace.git_path(path)).starts_with(&self.workspace.metadata)
    }
}
