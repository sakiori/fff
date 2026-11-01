use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::simd_path::{ArenaPtr, ChunkedPathStore, ChunkedPathStoreBuilder, ChunkedString};
use crate::types::{DirItem, FileItem};

/// Why a directory cannot bound a search over an index.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScopeError {
    #[error("{dir} is outside the index root {root}")]
    OutsideIndex { dir: PathBuf, root: PathBuf },
    #[error("{0} must not contain root, prefix or parent components")]
    NotNormalized(PathBuf),
}

/// Converts `dir` (the root or a path under it) to the index's dir form:
/// `/`-separated, lossily decoded, `/`-terminated, and empty for the root.
pub fn index_relative_dir(index_root: &Path, dir: &Path) -> Result<String, ScopeError> {
    let relative = dir
        .strip_prefix(index_root)
        .map_err(|_| ScopeError::OutsideIndex {
            dir: dir.to_path_buf(),
            root: index_root.to_path_buf(),
        })?;
    let mut out = String::new();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => {
                out.push_str(&part.to_string_lossy());
                out.push('/');
            }
            _ => return Err(ScopeError::NotNormalized(relative.to_path_buf())),
        }
    }
    Ok(out)
}

/// A directory bounding a search: matching, ranking, counts and pagination only
/// see files under it, and query paths are relative to it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct SearchScope {
    dir: String,
}

static ROOT: SearchScope = SearchScope { dir: String::new() };

impl SearchScope {
    /// Scope for `dir`, which must be `index_root` or lie under it.
    pub fn new(index_root: &Path, dir: &Path) -> Result<Self, ScopeError> {
        Ok(Self {
            dir: index_relative_dir(index_root, dir)?,
        })
    }

    /// The whole index.
    pub fn root() -> &'static SearchScope {
        &ROOT
    }

    pub fn is_root(&self) -> bool {
        self.dir.is_empty()
    }

    /// The scope directory in the index's directory form (see [`index_relative_dir`]).
    pub fn dir(&self) -> &str {
        &self.dir
    }

    /// Returns a scope-relative path, or `None` for a path outside the scope.
    pub fn relative_path<'a>(&self, path: &'a str) -> Option<&'a str> {
        path.strip_prefix(self.dir.as_str())
    }
}

impl Default for &SearchScope {
    fn default() -> Self {
        SearchScope::root()
    }
}

/// Scope resolved against one scan. Scan-built items never move until a
/// rescan replaces the index, so this part is cached per scan.
#[derive(Debug)]
pub(crate) struct ScopeBase {
    dir: Box<str>,
    dirs: Range<u32>,
    // One run per sorted file partition (indexable, unindexable).
    files: [Range<usize>; 2],
    file_paths: OnceLock<ScopedPaths>,
    dir_paths: OnceLock<ScopedPaths>,
}

impl ScopeBase {
    pub(crate) fn new(dir: &str, dirs: Range<u32>, files: [Range<usize>; 2]) -> Self {
        Self {
            dir: dir.into(),
            dirs,
            files,
            file_paths: OnceLock::new(),
            dir_paths: OnceLock::new(),
        }
    }

    pub(crate) fn dir(&self) -> &str {
        &self.dir
    }
}

/// Recently resolved scopes of one scan; a rescan drops them with the layout.
#[derive(Debug, Default)]
pub(crate) struct ScopeCache {
    entries: parking_lot::Mutex<Vec<Arc<ScopeBase>>>,
}

// Enough for the distinct client directories one shared index serves at once.
const SCOPE_CACHE_CAPACITY: usize = 8;

impl ScopeCache {
    pub(crate) fn get_or_insert(
        &self,
        dir: &str,
        resolve: impl FnOnce() -> ScopeBase,
    ) -> Arc<ScopeBase> {
        let mut entries = self.entries.lock();
        if let Some(position) = entries.iter().position(|entry| entry.dir() == dir) {
            // Most recently used entries live at the end.
            let entry = entries.remove(position);
            entries.push(Arc::clone(&entry));
            return entry;
        }
        if entries.len() == SCOPE_CACHE_CAPACITY {
            entries.remove(0);
        }
        let entry = Arc::new(resolve());
        entries.push(Arc::clone(&entry));
        entry
    }
}

/// A scope resolved for one query. Membership is a dir index check: scan-built
/// dirs are sorted by path, so the dirs under a prefix form one run.
#[derive(Debug)]
pub(crate) struct ResolvedScope {
    base: Arc<ScopeBase>,
    // Watcher-appended dirs under the scope, sorted.
    overflow_dirs: Vec<u32>,
    overflow_files_start: usize,
}

impl ResolvedScope {
    pub(crate) fn new(
        base: Arc<ScopeBase>,
        overflow_dirs: Vec<u32>,
        overflow_files_start: usize,
    ) -> Self {
        Self {
            base,
            overflow_dirs,
            overflow_files_start,
        }
    }

    /// Length of the index-relative prefix every in-scope path starts with.
    #[inline]
    pub(crate) fn prefix_len(&self) -> usize {
        self.base.dir.len()
    }

    #[inline]
    pub(crate) fn contains_dir(&self, dir_index: u32) -> bool {
        self.base.dirs.contains(&dir_index) || self.overflow_dirs.binary_search(&dir_index).is_ok()
    }

    #[inline]
    pub(crate) fn contains_file(&self, file: &FileItem) -> bool {
        self.contains_dir(file.parent_dir_index)
    }

    /// Scope-relative form of a contained path. Contained items sit in dirs that
    /// start with the prefix, so their paths carry it.
    #[inline]
    pub(crate) fn relative<'p>(&self, path: &'p str) -> &'p str {
        debug_assert!(
            path.starts_with(self.base.dir()),
            "{path} is outside the scope"
        );
        &path[self.prefix_len()..]
    }

    /// Index ranges of the scan-built files under the scope.
    pub(crate) fn base_files(&self) -> [Range<usize>; 2] {
        self.base.files.clone()
    }

    /// Ranges holding every in-scope file; the watcher-appended one needs a membership check.
    pub(crate) fn file_spans(&self, file_count: usize) -> [Range<usize>; 3] {
        let [indexable, unindexable] = self.base_files();
        [
            indexable,
            unindexable,
            self.overflow_files_start.min(file_count)..file_count,
        ]
    }

    pub(crate) fn live_file_count(&self, files: &[FileItem]) -> usize {
        self.file_spans(files.len())
            .into_iter()
            .flat_map(|span| &files[span])
            .filter(|file| !file.is_deleted() && self.contains_file(file))
            .count()
    }

    /// Dir table indices under the scope, scan-built first.
    pub(crate) fn dir_indices(&self) -> impl Iterator<Item = u32> + '_ {
        self.base
            .dirs
            .clone()
            .chain(self.overflow_dirs.iter().copied())
    }

    pub(crate) fn live_dir_count(&self, dirs: &[DirItem]) -> usize {
        self.dir_indices()
            .filter(|&index| !dirs[index as usize].is_deleted())
            .count()
    }

    /// Relative paths aligned with [`Self::base_files`] concatenated, laid out once per scan.
    pub(crate) fn base_file_paths(&self, files: &[FileItem], arena: ArenaPtr) -> &ScopedPaths {
        self.base.file_paths.get_or_init(|| {
            let [indexable, unindexable] = self.base_files();
            ScopedPaths::for_files(self, indexable.chain(unindexable).map(|i| &files[i]), arena)
        })
    }

    /// Relative paths aligned with the dir run at [`Self::base_dirs_start`], laid out once per scan.
    pub(crate) fn base_dir_paths(&self, dirs: &[DirItem], arena: ArenaPtr) -> &ScopedPaths {
        self.base.dir_paths.get_or_init(|| {
            let run = self.base.dirs.clone();
            ScopedPaths::for_dirs(self, run.map(|i| (i, &dirs[i as usize])), arena)
        })
    }

    pub(crate) fn base_dirs_start(&self) -> u32 {
        self.base.dirs.start
    }
}

/// Scope-relative copies of index paths used as fuzzy haystacks: chunks cannot
/// be sliced at an arbitrary byte, so relative paths need their own store.
#[derive(Debug)]
pub(crate) struct ScopedPaths {
    store: ChunkedPathStore,
    // Aligned with the source items; empty for items outside the scope.
    paths: Vec<ChunkedString>,
}

impl ScopedPaths {
    pub(crate) fn for_files<'f>(
        scope: &ResolvedScope,
        files: impl Iterator<Item = &'f FileItem>,
        arena: ArenaPtr,
    ) -> Self {
        let mut builder = ChunkedPathStoreBuilder::new(files.size_hint().0);
        let mut buf = [0u8; crate::constants::PATH_BUF_SIZE];
        let paths = files
            .map(|file| {
                if !scope.contains_file(file) {
                    return ChunkedString::empty();
                }
                let path = scope.relative(file.path.read_to_buf(arena, &mut buf));
                let filename_offset = file.path.filename_offset - scope.prefix_len() as u16;
                builder.add_file_immediate(path, filename_offset)
            })
            .collect();
        Self {
            store: builder.finish(),
            paths,
        }
    }

    pub(crate) fn for_dirs<'d>(
        scope: &ResolvedScope,
        dirs: impl Iterator<Item = (u32, &'d DirItem)>,
        arena: ArenaPtr,
    ) -> Self {
        let mut builder = ChunkedPathStoreBuilder::new(dirs.size_hint().0);
        let mut buf = [0u8; crate::constants::PATH_BUF_SIZE];
        let paths = dirs
            .map(|(index, dir)| {
                if !scope.contains_dir(index) {
                    return ChunkedString::empty();
                }
                builder.add_dir_immediate(scope.relative(dir.read_relative_path(arena, &mut buf)))
            })
            .collect();
        Self {
            store: builder.finish(),
            paths,
        }
    }

    pub(crate) fn arena(&self) -> ArenaPtr {
        self.store.as_arena_ptr()
    }

    pub(crate) fn paths(&self) -> &[ChunkedString] {
        &self.paths
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_relative_dir_matches_the_index_form() {
        let root = Path::new("/index");
        assert_eq!(index_relative_dir(root, root).unwrap(), "");
        assert_eq!(
            index_relative_dir(root, Path::new("/index/a/./b")).unwrap(),
            "a/b/"
        );
        assert!(matches!(
            index_relative_dir(root, Path::new("/elsewhere")),
            Err(ScopeError::OutsideIndex { .. })
        ));
        assert!(matches!(
            index_relative_dir(root, Path::new("/index/a/../b")),
            Err(ScopeError::NotNormalized(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_components_decode_like_indexed_names() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let root = Path::new("/index");
        let dir = root.join(OsStr::from_bytes(b"caf\xe9"));
        let expected = format!(
            "{}/",
            Path::new(OsStr::from_bytes(b"caf\xe9")).to_string_lossy()
        );
        assert_eq!(index_relative_dir(root, &dir).unwrap(), expected);
        assert_eq!(SearchScope::new(root, &dir).unwrap().dir(), expected);
    }

    #[test]
    fn root_scope_contains_every_path() {
        let scope = SearchScope::root();
        assert!(scope.is_root());
        assert_eq!(scope.relative_path("a/b.rs"), Some("a/b.rs"));
        let sub = SearchScope::new(Path::new("/i"), Path::new("/i/sub")).unwrap();
        assert!(!sub.is_root());
        assert_eq!(sub.relative_path("sub/b.rs"), Some("b.rs"));
        assert_eq!(sub.relative_path("submarine/b.rs"), None);
    }
}
