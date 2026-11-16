use std::path::{Path, PathBuf};

use ahash::AHashMap;
use parking_lot::Mutex;

// The git dirs of the repositories inside one index, with their work trees. Each index has its
// own, so a walk of an index that was replaced registers nothing into its successor.
#[derive(Default)]
pub(crate) struct GitDirs(Mutex<AHashMap<PathBuf, GitDir>>);

// A git dir the index registered, with its work tree and the common dir holding its refs.
pub(crate) struct RegisteredGitDir {
    pub(crate) git_dir: PathBuf,
    pub(crate) work_tree: PathBuf,
    pub(crate) common_dir: PathBuf,
}

// A recorded git dir: its work tree, the common dir holding its refs, and which directory
// it was when recorded.
struct GitDir {
    work_tree: PathBuf,
    common_dir: PathBuf,
    identity: Option<DirIdentity>,
}

// What tells a directory from one created at the same path later: a file system may give
// the new one the inode the old one freed, but not its birth time, where it records one.
#[derive(Clone, Copy, PartialEq, Eq)]
struct DirIdentity {
    file: (u64, u64),
    born: Option<std::time::SystemTime>,
}

impl GitDirs {
    // Returns false when `git_dir` was known already as the directory it is now; true for a
    // new one and for one removed and created again since, also within one batch.
    pub(crate) fn register(&self, git_dir: &Path, work_tree: &Path) -> bool {
        let identity = dir_identity(git_dir);
        let common_dir = crate::watch::common_dir_of(git_dir);
        let known = self.0.lock().insert(
            git_dir.to_path_buf(),
            GitDir {
                work_tree: work_tree.to_path_buf(),
                common_dir,
                identity,
            },
        );
        known.is_none_or(|known| known.identity != identity || identity.is_none())
    }

    // Forgets the git dirs of repositories at or below a removed `path`, so that one created
    // there again is recorded and watched anew: the old watches went with it. The removal
    // decides, also when the path is back by the time the batch is read, as a file system
    // may give the new directory the old one's inode.
    pub(crate) fn forget_under(&self, path: &Path) {
        (self.0.lock()).retain(|git_dir, known| {
            !git_dir.starts_with(path) && !known.work_tree.starts_with(path)
        });
    }

    // The registered git dir holding `path`, with its work tree.
    pub(crate) fn holding(&self, path: &Path) -> Option<(PathBuf, PathBuf)> {
        let git_dirs = self.0.lock();
        path.ancestors().skip(1).find_map(|dir| {
            git_dirs
                .get(dir)
                .map(|known| (dir.to_path_buf(), known.work_tree.clone()))
        })
    }

    // Every registered git dir with its work tree and common dir.
    pub(crate) fn all(&self) -> Vec<RegisteredGitDir> {
        (self.0.lock().iter())
            .map(|(git_dir, known)| RegisteredGitDir {
                git_dir: git_dir.clone(),
                work_tree: known.work_tree.clone(),
                common_dir: known.common_dir.clone(),
            })
            .collect()
    }

    // The work tree of the registered git dir `git_dir` itself.
    pub(crate) fn work_tree(&self, git_dir: &Path) -> Option<PathBuf> {
        (self.0.lock().get(git_dir)).map(|known| known.work_tree.clone())
    }
}

fn dir_identity(path: &Path) -> Option<DirIdentity> {
    let metadata = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::MetadataExt;
        (metadata.dev(), metadata.ino())
    };
    #[cfg(not(unix))]
    let file = (0, 0);
    Some(DirIdentity {
        file,
        born: metadata.created().ok(),
    })
}
