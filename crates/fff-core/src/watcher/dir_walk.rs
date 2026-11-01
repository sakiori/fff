use crate::file_picker::{FilePicker, IndexUpdateError};
use crate::shared::SharedFilePicker;
use crate::walk::DirHook;
use crate::watch::{BackgroundWatcher, WatchEventKind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use tracing::warn;

// What a walk under the index's root policy lists in a directory, taken outside the picker lock.
pub(super) struct DirWalk {
    dir: PathBuf,
    // Relative and '/'-canonical, sorted; dirs end with '/'.
    files: Vec<WalkedFile>,
    dirs: Vec<String>,
}

struct WalkedFile {
    rel: String,
    identity: (u32, u64, u64),
}

impl DirWalk {
    // `None` when the walk could not run at all.
    pub(super) fn new(
        dir: &Path,
        base_path: &Path,
        is_git_repo: bool,
        follow_symlinks: bool,
        dir_hook: Option<&DirHook>,
    ) -> Option<Self> {
        let walk = match crate::walk::walk_collect_files(
            base_path,
            Some(dir),
            is_git_repo,
            follow_symlinks,
            1,
            &Arc::new(AtomicUsize::new(0)),
            dir_hook,
        ) {
            Ok(walk) => walk,
            Err(e) => {
                warn!(?e, dir = %dir.display(), "Failed to walk directory");
                return None;
            }
        };

        let mut files: Vec<WalkedFile> = walk
            .pairs
            .into_iter()
            .map(|(file, rel)| WalkedFile {
                rel,
                identity: identity(&file),
            })
            .collect();
        let mut dirs = walk.dirs;
        files.sort_unstable_by(|a, b| a.rel.cmp(&b.rel));
        dirs.sort_unstable();
        Some(Self {
            dir: dir.to_path_buf(),
            files,
            dirs,
        })
    }

    fn lists_file(&self, rel: &str) -> bool {
        self.files
            .binary_search_by(|file| file.rel.as_str().cmp(rel))
            .is_ok()
    }

    fn lists_dir(&self, rel: &str) -> bool {
        self.dirs
            .binary_search_by(|dir| dir.as_str().cmp(rel))
            .is_ok()
    }

    // Adds or updates every listed entry the index does not hold as listed. `events` gets
    // each file whose indexed state changed, `indexed` each file it added or updated;
    // fails once the index is out of capacity.
    pub(super) fn apply(
        &self,
        picker: &mut FilePicker,
        base_path: &Path,
        events: &mut Vec<(PathBuf, WatchEventKind)>,
        indexed: &mut Vec<PathBuf>,
    ) -> Result<(), IndexUpdateError> {
        for dir in &self.dirs {
            let path = base_path.join(dir.trim_end_matches('/'));
            if picker.handle_create_directory(&path) == Err(IndexUpdateError::CapacityExhausted) {
                return Err(IndexUpdateError::CapacityExhausted);
            }
        }

        for walked in &self.files {
            let path = base_path.join(&walked.rel);
            let before = picker
                .get_file_by_path(&path)
                .filter(|file| !file.is_deleted())
                .map(identity);
            // The same inode with the same size and modification time is the file the index
            // read; a replaced directory brings new inodes, which are indexed again.
            if before == Some(walked.identity) && walked.identity.0 != 0 {
                continue;
            }
            match picker.apply_create_or_modify(&path) {
                Ok(file) => {
                    let kind = match before {
                        None => Some(WatchEventKind::Created),
                        Some((_, size, modified))
                            if (size, modified) != (file.size, file.modified) =>
                        {
                            Some(WatchEventKind::Modified)
                        }
                        Some(_) => None,
                    };
                    if let Some(kind) = kind {
                        events.push((path.clone(), kind));
                    }
                    indexed.push(path);
                }
                Err(IndexUpdateError::CapacityExhausted) => {
                    return Err(IndexUpdateError::CapacityExhausted);
                }
                Err(IndexUpdateError::Vanished | IndexUpdateError::OutsideIndex) => {}
            }
        }
        Ok(())
    }
}

// Directories a batch found replaced, walked against the index snapshot they will update.
pub(super) struct ReplacedDirs {
    // Sorted by directory and not nested.
    walks: Vec<DirWalk>,
    generation: u64,
}

impl ReplacedDirs {
    // Walks outside the picker lock. A directory whose walk cannot run keeps its entries,
    // as a scan whose walk fails leaves the index as it is.
    pub(super) fn walk(
        mut dirs: Vec<PathBuf>,
        base_path: &Path,
        is_git_repo: bool,
        follow_symlinks: bool,
        shared_picker: &SharedFilePicker,
    ) -> Self {
        dirs.sort_unstable();
        dirs.dedup_by(|nested, outer| nested.starts_with(&*outer));

        let (dir_hook, generation) = shared_picker
            .read()
            .ok()
            .and_then(|guard| {
                guard.as_ref().map(|picker| {
                    // A replaced directory is a new inode: per-directory backends subscribe it again.
                    let hook = picker
                        .background_watcher
                        .as_ref()
                        .and_then(BackgroundWatcher::dir_hook);
                    (hook, picker.sync_generation())
                })
            })
            .unwrap_or((None, 0));

        let walks = dirs
            .iter()
            .filter_map(|dir| {
                if let Some(subscribe) = &dir_hook {
                    subscribe(dir);
                }
                DirWalk::new(
                    dir,
                    base_path,
                    is_git_repo,
                    follow_symlinks,
                    dir_hook.as_ref(),
                )
            })
            .collect();
        Self { walks, generation }
    }

    pub(super) fn len(&self) -> usize {
        self.walks.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.walks.is_empty()
    }

    pub(super) fn covers(&self, path: &Path) -> bool {
        self.owner(path).is_some()
    }

    // Whether `path` is covered and its walk did not list it as a file.
    pub(super) fn drops(&self, path: &Path, base_path: &Path) -> bool {
        let Some(walk) = self.owner(path) else {
            return false;
        };
        let Some(rel) = path.strip_prefix(base_path).ok().and_then(Path::to_str) else {
            return true;
        };
        !walk.lists_file(&crate::path_utils::to_canonical_slashes(rel))
    }

    // Replaces the entries below every walked directory in one pass over the index, unless
    // a scan committed since the walks: its snapshot was taken after the batch's events.
    pub(super) fn apply(
        &self,
        picker: &mut FilePicker,
        base_path: &Path,
        on_removed: impl FnMut(&Path),
        events: &mut Vec<(PathBuf, WatchEventKind)>,
        indexed: &mut Vec<PathBuf>,
    ) -> Result<(), IndexUpdateError> {
        if self.walks.is_empty() || picker.sync_generation() != self.generation {
            return Ok(());
        }
        let dirs: Vec<&Path> = self.walks.iter().map(|walk| walk.dir.as_path()).collect();
        picker.retain_in_dirs(
            &dirs,
            |owner, rel| self.walks[owner].lists_file(rel),
            // The root is the one directory a walk does not list.
            |owner, rel| rel.is_empty() || self.walks[owner].lists_dir(rel),
            on_removed,
        );
        for walk in &self.walks {
            walk.apply(picker, base_path, events, indexed)?;
        }
        Ok(())
    }

    fn owner(&self, path: &Path) -> Option<&DirWalk> {
        let index = self
            .walks
            .partition_point(|walk| walk.dir.as_path() <= path);
        let walk = self.walks.get(index.checked_sub(1)?)?;
        path.starts_with(&walk.dir).then_some(walk)
    }
}

// Inode bits, size and modification time; 0 inode bits where the platform has none.
fn identity(file: &crate::types::FileItem) -> (u32, u64, u64) {
    (file.ino, file.size, file.modified)
}
