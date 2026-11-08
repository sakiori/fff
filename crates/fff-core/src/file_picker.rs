//! Core file picker: filesystem indexing, background watching, and fuzzy search.
//!
//! [`FilePicker`] is the central component of fff-search. It:
//!
//! 1. **Indexes** a directory tree in a background thread, collecting every
//!    non-ignored file into a path-sorted `Vec<FileItem>`.
//! 2. **Watches** the filesystem via the `notify` crate, applying
//!    create/modify/delete events to the index in real time.
//! 3. **Owns files**: Provides a values for search and provides a good entry point for
//!    fuzzy search and live grep
//!
//! # Lifecycle
//!
//! ```text
//!   new_with_shared_state()
//!     │
//!     ├─> background scan thread ──> populates SharedPicker
//!     └─> file-system watcher    ──> live updates SharedPicker
//!
//!   search()         <── borrows &self, delegates to fuzzy_search
//!   grep()           <── static, borrows &[FileItem] (live content search)
//!   trigger_rescan() <── synchronous re-index
//!   cancel()         <── shuts down background work
//! ```
//!
//! # Thread Safety
//!
//! `FilePicker` itself is **not** `Sync`!
//! all concurrent access goes through [`crate::SharedFilePicker`]

use crate::FFFStringStorage;
use crate::constants::{MAX_OVERFLOW_FILES, PATH_BUF_SIZE};
use crate::error::Error;
use crate::frecency::{FrecencyReader, FrecencyTracker};
use crate::git::{GitDiscovery, GitStatusCache};
use crate::git_recency::{self, GitRecencyConfig};
use crate::grep::{GrepResult, GrepSearchOptions, grep_search, multi_grep_search};
use crate::index::{BigramFilter, BigramOverlay};
use crate::listeners::CoverageListeners;
use crate::query_tracker::QueryTracker;
use crate::scan::{ScanConfig, ScanJob, ScanSignals};
use crate::scope::{ResolvedScope, ScopeBase, ScopeCache, SearchScope};
use crate::score::{
    fuzzy_match_and_score_dirs, fuzzy_match_and_score_files, fuzzy_match_byte_offsets_for_page,
};
use crate::shared::{SharedFilePicker, SharedFrecency};
use crate::simd_path::{ArenaPtr, Arenas};
use crate::stable_vec::StableVec;
use crate::types::{
    ContentCacheBudget, DirItem, DirSearchResult, FileItem, MixedItemRef, MixedSearchResult,
    PaginationArgs, Score, ScoringContext, SearchResult,
};
use crate::walk::WalkOutput;
use crate::watch::BackgroundWatcher;
use ahash::AHashMap;
use fff_query_parser::FFFQuery;
use git2::{Repository, Status};
use rayon::prelude::*;
use std::fmt::Debug;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread::JoinHandle;
use std::time::SystemTime;
use tracing::{Level, debug, error, info, warn};

use crate::parallelism::BACKGROUND_THREAD_POOL;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FFFMode {
    #[default]
    Neovim,
    Ai,
}

impl FFFMode {
    pub fn is_ai(self) -> bool {
        self == FFFMode::Ai
    }
}

/// Configuration for a single fuzzy search invocation.
///
/// Passed to [`FilePicker::search`] to control threading, pagination,
/// and scoring behavior.
#[derive(Debug, Clone, Copy, Default)]
pub struct FuzzySearchOptions<'a> {
    pub max_threads: usize,
    pub current_file: Option<&'a str>,
    pub project_path: Option<&'a Path>,
    pub combo_boost_score_multiplier: i32,
    pub min_combo_count: u32,
    pub pagination: PaginationArgs,
    /// Directory the search is limited to; query paths and highlight offsets
    /// are relative to it. Defaults to the whole index.
    pub scope: &'a SearchScope,
    /// Rank as if the index had no frecency database; a query without fuzzy
    /// text then lists entries in path order.
    pub exclude_frecency: bool,
}

#[derive(Debug)]
pub(crate) struct FileSync {
    pub(crate) git: Option<GitDiscovery>,
    // Base path re-rooted into the main checkout when indexing a linked git worktree,
    // frecency and query history are keyed by it so all worktrees share one history
    shared_base: Option<PathBuf>,
    /// Base files laid out in two partitions, each internally sorted by
    /// (parent_dir, filename):
    ///   `files[..indexable_count]` - indexable
    ///   `files[indexable_count..base_count]` - original-unindexable
    ///   `files[base_count..]` - overflow
    files: StableVec<FileItem>,
    indexable_count: usize,
    base_count: usize,
    /// Number of active present files that exists in the file system
    pub(crate) live_count: usize,
    /// Sorted directory table. `StableVec` so post-scan snapshots can keep
    /// the allocation alive across a picker drop without copying, and so
    /// concurrent readers observe a consistent view via the same shared
    /// allocation. Dir frecency is updated through the per-entry atomic
    /// (`DirItem::max_access_frecency`) without `&mut` aliasing.
    /// Layout mirrors `files`: `dirs[..base_dirs_count]` is the sorted
    /// scan-built region, `dirs[base_dirs_count..]` holds watcher-appended dirs.
    dirs: StableVec<DirItem>,
    base_dirs_count: usize,
    /// Number of dirs with at least one live file (mirrors `live_count`).
    live_dirs_count: usize,
    /// Shared builder for overflow file paths. Each overflow file's ChunkedString
    /// uses `arena_override` pointing into this builder's arena.
    overflow_builder: Option<crate::simd_path::ChunkedPathStoreBuilder>,
    bigram_index: Option<Arc<BigramFilter>>,
    bigram_overlay: Option<Arc<parking_lot::RwLock<BigramOverlay>>>,
    /// Chunk-level deduped path store. Arc so post-scan snapshots can hold
    /// the arena alive while iterating file paths.
    chunked_paths: Option<Arc<crate::simd_path::ChunkedPathStore>>,
    /// Ignore rules the walker assembled (zlob backend only). Shared with the
    /// background watcher so filesystem events can be filtered without libgit2.
    pub(crate) ignore_rules: Option<Arc<crate::walk::WalkIgnoreRules>>,
    /// Search scopes resolved against this scan's layout.
    scopes: ScopeCache,
}

impl FileSync {
    fn new() -> Self {
        Self {
            files: StableVec::from_vec_with_reserve(Vec::new(), MAX_OVERFLOW_FILES),
            indexable_count: 0,
            base_count: 0,
            live_count: 0,
            dirs: StableVec::from_vec_with_reserve(Vec::new(), MAX_OVERFLOW_FILES),
            base_dirs_count: 0,
            live_dirs_count: 0,
            overflow_builder: None,
            git: None,
            shared_base: None,
            bigram_index: None,
            bigram_overlay: None,
            chunked_paths: None,
            ignore_rules: None,
            scopes: ScopeCache::default(),
        }
    }

    #[inline]
    fn arena_base_ptr(&self) -> ArenaPtr {
        self.chunked_paths
            .as_ref()
            .map(|s| s.as_arena_ptr())
            .unwrap_or(ArenaPtr::null())
    }

    #[inline]
    fn arena_overflow_ptr(&self) -> ArenaPtr {
        self.overflow_builder
            .as_ref()
            .map(|b| b.as_arena_ptr())
            .unwrap_or(ArenaPtr::null())
    }

    #[inline]
    fn arena_for_file(&self, file: &FileItem) -> ArenaPtr {
        if file.is_overflow() {
            self.arena_overflow_ptr()
        } else {
            self.arena_base_ptr()
        }
    }

    #[inline]
    fn files(&self) -> &[FileItem] {
        &self.files
    }

    #[inline]
    fn overflow_files(&self) -> &[FileItem] {
        &self.files[self.base_count..]
    }

    #[inline]
    fn get_file_mut(&mut self, index: usize) -> Option<(ArenaPtr, &mut FileItem)> {
        Some((
            if index < self.base_count {
                self.arena_base_ptr()
            } else {
                self.arena_overflow_ptr()
            },
            self.files.get_mut(index)?,
        ))
    }

    #[inline]
    fn find_file_index(&self, path: &Path, base_path: &Path) -> Option<usize> {
        self.find_file_index_with(path, base_path, |dir| self.find_base_dir_index(dir))
    }

    fn find_file_index_with(
        &self,
        path: &Path,
        base_path: &Path,
        find_dir: impl FnOnce(&str) -> Option<usize>,
    ) -> Option<usize> {
        // Strip base_path prefix to get the relative path. On Windows this
        // can fail for 8.3 short names or a different casing; fall back to
        // canonicalize-then-strip so watcher events still land on the right
        // `FileItem`.
        let rel_path = match path.strip_prefix(base_path) {
            Ok(r) => r.to_string_lossy(),
            Err(_) => {
                #[cfg(windows)]
                {
                    std::borrow::Cow::Owned(canonical_relative_path(path, base_path)?)
                }
                #[cfg(not(windows))]
                {
                    return None;
                }
            }
        };
        // The dir table and stored file paths are '/'-canonical; fold the
        // native relative path so the byte-wise comparisons below match.
        self.find_by_relative_path_with(
            &crate::path_utils::to_canonical_slashes(&rel_path),
            find_dir,
        )
    }

    fn find_base_dir_index(&self, dir: &str) -> Option<usize> {
        let arena = self.arena_base_ptr();
        let mut buf = [0; PATH_BUF_SIZE];
        self.dirs[..self.base_dirs_count]
            .binary_search_by(|d| d.read_relative_path(arena, &mut buf).cmp(dir))
            .ok()
    }

    fn find_by_relative_path(&self, rel_path: &str) -> Option<usize> {
        self.find_by_relative_path_with(rel_path, |dir| self.find_base_dir_index(dir))
    }

    fn find_by_relative_path_with(
        &self,
        rel_path: &str,
        find_dir: impl FnOnce(&str) -> Option<usize>,
    ) -> Option<usize> {
        let arena = self.arena_base_ptr();
        // Split into directory (with trailing '/') and filename.
        let parent_end = rel_path
            .rfind(std::path::is_separator)
            .map(|i| i + 1)
            .unwrap_or(0);
        let dir_rel = &rel_path[..parent_end];
        let filename = &rel_path[parent_end..];

        if let Some(dir_idx) = find_dir(dir_rel)
            && let Some(pos) = self.find_base_file_index(dir_idx as u32, filename, arena)
        {
            return Some(pos);
        }

        // Overflow region: linear scan by full relative path.
        if self.base_count < self.files.len() {
            let overflow_arena = self.arena_overflow_ptr();
            if let Some(pos) = self.files[self.base_count..]
                .iter()
                .position(|f| f.relative_path_eq(overflow_arena, rel_path))
            {
                return Some(self.base_count + pos);
            }
        }

        None
    }

    fn find_base_file_index(&self, dir_idx: u32, filename: &str, arena: ArenaPtr) -> Option<usize> {
        let mut fname = String::new();
        let mut cmp_key = |f: &FileItem| {
            f.parent_dir_index.cmp(&dir_idx).then_with(|| {
                fname.clear();
                f.write_file_name_from_arena(arena, &mut fname);
                fname.as_str().cmp(filename)
            })
        };

        if self.indexable_count > 0
            && let Ok(pos) = self.files[..self.indexable_count].binary_search_by(&mut cmp_key)
        {
            return Some(pos);
        }

        if self.indexable_count < self.base_count
            && let Ok(rel_pos) =
                self.files[self.indexable_count..self.base_count].binary_search_by(cmp_key)
        {
            return Some(self.indexable_count + rel_pos);
        }

        None
    }

    // Marks base files that `previous` already read as text with the same path, size and mtime,
    // so a rescan only reads changed files. Binary verdicts are always checked again.
    fn unchanged_text_files(&self, previous: &FileSync) -> Vec<u64> {
        if previous.base_count == 0 {
            return Vec::new();
        }
        let mut known_text = vec![0u64; self.base_count.div_ceil(64)];

        let arena = self.arena_base_ptr();
        let previous_arena = previous.arena_base_ptr();
        let mut buf = [0u8; PATH_BUF_SIZE];
        let previous_dirs: Vec<Option<u32>> = self.dirs[..self.base_dirs_count]
            .iter()
            .map(|dir| {
                let dir = dir.read_relative_path(arena, &mut buf);
                previous.find_base_dir_index(dir).map(|i| i as u32)
            })
            .collect();

        let mut name = String::new();
        for (index, file) in self.files[..self.base_count].iter().enumerate() {
            let Some(&Some(previous_dir)) = previous_dirs.get(file.parent_dir_index as usize)
            else {
                continue;
            };

            name.clear();
            file.write_file_name_from_arena(arena, &mut name);
            let Some(old) = previous
                .find_base_file_index(previous_dir, &name, previous_arena)
                .map(|i| &previous.files[i])
            else {
                continue;
            };

            if !old.is_deleted()
                && !old.is_binary()
                && old.size == file.size
                && old.modified == file.modified
            {
                known_text[index / 64] |= 1u64 << (index % 64);
            }
        }

        known_text
    }

    // TODO remove this function and make a better way to remove all files
    // from the directory without looping over the whole sync data list
    // Tombstones every matching arena file.
    fn tombstone_files_with_arena<F, T>(&mut self, mut predicate: F, mut on_tombstone: T) -> usize
    where
        F: FnMut(&FileItem, ArenaPtr) -> bool,
        T: FnMut(&mut FileItem, ArenaPtr),
    {
        let base_arena = self.arena_base_ptr();
        let overflow_arena = self.arena_overflow_ptr();
        let base_count = self.base_count;

        let mut tombstoned = 0usize;
        for (idx, file) in self.files.iter_mut().enumerate() {
            if file.is_deleted() {
                continue;
            }
            let arena = if idx < base_count {
                base_arena
            } else {
                overflow_arena
            };
            if predicate(file, arena) {
                on_tombstone(file, arena);
                file.set_deleted(true);
                tombstoned += 1;
            }
        }
        self.live_count -= tombstoned;
        tombstoned
    }

    /// Marks every dir matching `predicate` as deleted. Mirrors how dir-level
    /// FS events (remove/move-out) invalidate whole subtrees.
    fn tombstone_dirs_with_arena<F>(&mut self, mut predicate: F)
    where
        F: FnMut(&DirItem, ArenaPtr) -> bool,
    {
        let base_arena = self.arena_base_ptr();
        let overflow_arena = self.arena_overflow_ptr();
        let base_dirs_count = self.base_dirs_count;

        let mut removed = 0usize;
        for (idx, dir) in self.dirs.iter_mut().enumerate() {
            if dir.is_deleted() {
                continue;
            }
            let arena = if idx < base_dirs_count {
                base_arena
            } else {
                overflow_arena
            };
            if predicate(dir, arena) && dir.set_deleted(true) {
                removed += 1;
            }
        }
        self.live_dirs_count -= removed;
    }

    /// Restores a dir to the live state (file appeared under it again).
    fn revive_dir(&mut self, dir_idx: u32) {
        if let Some(dir) = self.dirs.get_mut(dir_idx as usize)
            && dir.set_deleted(false)
        {
            self.live_dirs_count += 1;
        }
    }

    /// Finds the dir index for a '/'-canonical relative dir path
    /// (with trailing '/', empty string for the base dir itself).
    fn find_dir_index(&self, dir_rel: &str) -> Option<usize> {
        let arena = self.arena_base_ptr();
        let mut dir_buf = [0u8; crate::simd_path::PATH_BUF_SIZE];
        if let Ok(idx) = self.dirs[..self.base_dirs_count]
            .binary_search_by(|d| d.read_relative_path(arena, &mut dir_buf).cmp(dir_rel))
        {
            return Some(idx);
        }

        // Watcher-appended region: unsorted, small (bounded by overflow cap).
        let overflow_arena = self.arena_overflow_ptr();
        self.dirs[self.base_dirs_count..]
            .iter()
            .position(|d| d.read_relative_path(overflow_arena, &mut dir_buf) == dir_rel)
            .map(|pos| self.base_dirs_count + pos)
    }

    /// Finds or appends the DirItem for `dir_rel`, returning its index.
    /// `None` when the dir table's overflow capacity is exhausted.
    fn find_or_add_dir(&mut self, dir_rel: &str) -> Option<u32> {
        if let Some(idx) = self.find_dir_index(dir_rel) {
            return Some(idx as u32);
        }
        if self.dirs.remaining_capacity() == 0 {
            return None;
        }

        let builder = self.overflow_builder.get_or_insert_with(|| {
            crate::simd_path::ChunkedPathStoreBuilder::new(MAX_OVERFLOW_FILES)
        });
        let chunked = builder.add_dir_immediate(dir_rel);

        let last_seg = if dir_rel.is_empty() {
            0
        } else {
            let trimmed = dir_rel.trim_end_matches(std::path::is_separator);
            trimmed
                .rfind(std::path::is_separator)
                .map(|i| i + 1)
                .unwrap_or(0) as u16
        };

        let idx = self.dirs.len();
        if !self.dirs.push(DirItem::new_overflow(chunked, last_seg)) {
            return None;
        }
        self.live_dirs_count += 1;
        Some(idx as u32)
    }
}

impl FileSync {
    /// `None` for the root scope, which needs no filtering.
    fn resolve_scope(&self, scope: &SearchScope) -> Option<ResolvedScope> {
        if scope.is_root() {
            return None;
        }
        let prefix = scope.dir();
        let base = self
            .scopes
            .get_or_insert(prefix, || self.scope_base(prefix));

        let overflow_arena = self.arena_overflow_ptr();
        let mut buf = [0u8; PATH_BUF_SIZE];
        let overflow_dirs = (self.base_dirs_count..self.dirs.len())
            .filter(|&index| {
                self.dirs[index]
                    .read_relative_path(overflow_arena, &mut buf)
                    .starts_with(prefix)
            })
            .map(|index| index as u32)
            .collect();
        Some(ResolvedScope::new(base, overflow_dirs, self.base_count))
    }

    // Scan-built dirs are sorted by path, so the dirs under a prefix form one
    // run; files are sorted by parent dir within each partition.
    fn scope_base(&self, prefix: &str) -> ScopeBase {
        let arena = self.arena_base_ptr();
        let mut buf = [0u8; PATH_BUF_SIZE];
        let base_dirs = &self.dirs[..self.base_dirs_count];
        let start = base_dirs.partition_point(|d| d.read_relative_path(arena, &mut buf) < prefix);
        let len = base_dirs[start..]
            .partition_point(|d| d.read_relative_path(arena, &mut buf).starts_with(prefix));
        let dirs = start as u32..(start + len) as u32;

        let run = |partition: Range<usize>| {
            let files = &self.files[partition.clone()];
            let first = files.partition_point(|f| f.parent_dir_index < dirs.start);
            let end = files.partition_point(|f| f.parent_dir_index < dirs.end);
            partition.start + first..partition.start + end
        };
        let files = [
            run(0..self.indexable_count),
            run(self.indexable_count..self.base_count),
        ];
        ScopeBase::new(prefix, dirs, files)
    }
}

impl FileItem {
    pub fn new(path: PathBuf, base_path: &Path, git_status: Option<Status>) -> (Self, String) {
        let metadata = std::fs::metadata(&path).ok();
        Self::new_with_metadata(path, base_path, git_status, metadata.as_ref())
    }

    /// Create a FileItem using pre-fetched metadata to avoid a redundant stat syscall.
    /// Returns `(FileItem, relative_path)`. The FileItem's `path` field is
    /// empty; callers must populate it via `set_path` or `build_chunked_path_store_and_assign`.
    fn new_with_metadata(
        path: PathBuf,
        base_path: &Path,
        git_status: Option<Status>,
        metadata: Option<&std::fs::Metadata>,
    ) -> (Self, String) {
        let path_buf = pathdiff::diff_paths(&path, base_path).unwrap_or_else(|| path.clone());
        // The index is '/'-canonical on every platform; fold native separators.
        let relative_path =
            crate::path_utils::to_canonical_slashes(&path_buf.to_string_lossy()).into_owned();

        let (size, modified) = match metadata {
            Some(metadata) => {
                let size = metadata.len();
                let modified = metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs());

                (size, modified)
            }
            None => (0, 0),
        };

        let is_binary = is_binary_file(&path);

        let filename_start = relative_path
            .rfind(std::path::is_separator)
            .map(|i| i + 1)
            .unwrap_or(0) as u16;

        let mut item = Self::new_raw(filename_start, size, modified, git_status, is_binary);
        item.ino = metadata.map_or(0, crate::types::inode_bits);
        item.mark_definition_language(&relative_path);
        (item, relative_path)
    }

    /// Create a FileItem with an empty ChunkedString from a path on disk.
    ///
    /// Returns `(file_item, relative_path_string)`. The relative path must be
    /// kept alongside the FileItem until `build_chunked_path_store_and_assign`
    /// populates each item's `path` field from the shared arena.
    pub fn new_from_walk(
        path: &Path,
        base_path: &Path,
        git_status: Option<Status>,
        metadata: Option<&std::fs::Metadata>,
    ) -> (Self, String) {
        let (size, modified) = match metadata {
            Some(metadata) => {
                let size = metadata.len();
                let modified = metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs());
                (size, modified)
            }
            None => (0, 0),
        };

        let (mut item, rel) =
            Self::new_from_walk_parts(path, base_path, git_status, size, modified);
        item.ino = metadata.map_or(0, crate::types::inode_bits);
        (item, rel)
    }

    /// Like [`Self::new_from_walk`] but takes already-extracted size and
    /// modification time (Unix seconds) instead of a `std::fs::Metadata`.
    /// Used by the zlob walker backend, which fetches metadata in bulk.
    pub fn new_from_walk_parts(
        path: &Path,
        base_path: &Path,
        git_status: Option<Status>,
        size: u64,
        modified: u64,
    ) -> (Self, String) {
        let is_binary = is_binary_file(path);

        let rel = pathdiff::diff_paths(path, base_path).unwrap_or_else(|| path.to_path_buf());
        // The index is '/'-canonical on every platform; fold native separators.
        let rel_str = crate::path_utils::to_canonical_slashes(&rel.to_string_lossy()).into_owned();
        let fname_offset = rel_str
            .rfind(std::path::is_separator)
            .map(|i| i + 1)
            .unwrap_or(0) as u16;

        let mut item = Self::new_raw(fname_offset, size, modified, git_status, is_binary);
        item.mark_definition_language(&rel_str);
        (item, rel_str)
    }

    /// Zlob-walker fast path: skip the `pathdiff::diff_paths` PathBuf alloc by
    /// taking the already-relative slice and the basename-offset that zlob's
    /// scanner computed during traversal. ~80–120 ms saved on a chromium scan
    /// (500k entries × one fewer alloc + no component walk).
    ///
    /// `relative_path` is root-relative bytes; `basename_offset` is the byte
    /// offset where the basename begins (e.g. zlob's `entry.path_bytes().len()
    /// - entry.file_name().as_os_str().as_encoded_bytes().len()` minus the
    /// `relative_offset`).
    pub fn new_from_walk_bytes(
        path: &Path,
        relative_path: &[u8],
        basename_offset: u16,
        git_status: Option<Status>,
        size: u64,
        modified: u64,
    ) -> (Self, String) {
        let is_binary = is_binary_file(path);
        // SAFETY-ish: paths on macOS/Linux are bytes; lossy conversion mirrors
        // the existing `to_string_lossy()` behavior on non-UTF8 names.
        let decoded = String::from_utf8_lossy(relative_path).into_owned();
        // The caller's offset indexes the raw bytes; re-measure it against the
        // decoded string so it never lands inside a U+FFFD (#799).
        let dir_bytes = relative_path
            .get(..basename_offset as usize)
            .unwrap_or(relative_path);
        let basename_offset = if std::str::from_utf8(dir_bytes).is_ok() {
            basename_offset
        } else {
            String::from_utf8_lossy(dir_bytes).len() as u16
        };
        let mut item = Self::new_raw(basename_offset, size, modified, git_status, is_binary);
        item.mark_definition_language(&decoded);
        (item, decoded)
    }

    pub(crate) fn update_frecency_scores(
        &mut self,
        reader: &FrecencyReader,
        arena: ArenaPtr,
        base_path: &Path,
        shared_base: Option<&Path>,
        mode: FFFMode,
    ) {
        let mut abs_buf = [0u8; crate::simd_path::PATH_BUF_SIZE];
        let abs = self.write_absolute_path(arena, base_path, &mut abs_buf);
        let access_score = match shared_base {
            Some(shared_base) => {
                let mut shared_buf = [0u8; crate::simd_path::PATH_BUF_SIZE];
                let shared = self.write_absolute_path(arena, shared_base, &mut shared_buf);
                reader.access_score(abs, Some(shared), mode)
            }
            None => reader.access_score(abs, None, mode),
        };

        self.access_frecency_score = access_score as i16;
        self.modification_frecency_score =
            reader.modification_score(self.modified, self.git_status, mode) as i16;
    }
}

/// Options for creating a [`FilePicker`].
pub struct FilePickerOptions {
    pub base_path: String,
    /// Pre-populate mmap caches for top-frecency files after the initial scan
    pub enable_mmap_cache: bool,
    /// Build content index after the initial scan for faster content-aware filtering
    pub enable_content_indexing: bool,
    /// Mode of the picker impact the way file watcher events are handled and the scoring logic
    pub mode: FFFMode,
    /// Explicit cache budget. When `None`, the budget is auto-computed from
    /// the repo size after the initial scan completes.
    pub cache_budget: Option<ContentCacheBudget>,
    /// When `false` no background watcher will be created
    pub watch: bool,
    /// Follow symbolic links during file indexing
    pub follow_symlinks: bool,
    /// Allow indexing the filesystem root (`/`)
    pub enable_fs_root_scanning: bool,
    /// Allow indexing the user's home directory. Off by default for the same
    /// reason as `enable_fs_root_scanning`
    pub enable_home_dir_scanning: bool,
    /// Ranking boost for files that participated in recent commits of the
    /// current branch. Enabled with default limits unless overridden.
    pub git_recency: GitRecencyConfig,
    /// Minimum delay between watcher-initiated rescans. `None` derives it from
    /// the index size.
    pub rescan_cooldown: Option<std::time::Duration>,
}

impl Default for FilePickerOptions {
    fn default() -> Self {
        Self {
            base_path: ".".into(),
            enable_mmap_cache: false,
            enable_content_indexing: false,
            mode: FFFMode::default(),
            cache_budget: None,
            watch: true,
            follow_symlinks: false,
            enable_fs_root_scanning: false,
            enable_home_dir_scanning: false,
            git_recency: GitRecencyConfig::default(),
            rescan_cooldown: None,
        }
    }
}

/// Controls which ignore rules a picker applies while indexing.
#[derive(Clone, Debug, Default)]
pub struct FilePickerIgnoreOptions {
    /// Extra root-relative rules. Requires the zlob walker.
    pub extra_ignore: Arc<[String]>,
    /// Skip Git integration and Git ignore rules.
    pub disable_git: bool,
    /// Skip rules from `.ignore` files.
    pub disable_ignore_files: bool,
}

pub struct FilePicker {
    pub mode: FFFMode,
    pub base_path: PathBuf,
    sync_data: FileSync,
    sync_generation: u64,
    coverage: Arc<CoverageListeners>,
    pub(crate) signals: ScanSignals,
    pub(crate) background_watcher: Option<BackgroundWatcher>,
    /// Single serialized writer for all git-status updates (scan, watcher,
    /// FFI). Owned by the picker so it exists before the first scan; its
    /// consumer thread is spawned lazily once a git workdir is discovered.
    pub(crate) git_status_worker: Arc<crate::git_status_worker::GitStatusWorker>,
    cache_budget: Arc<ContentCacheBudget>,
    has_explicit_cache_budget: bool,
    scanned_files_count: Arc<AtomicUsize>,
    enable_mmap_cache: bool,
    enable_content_indexing: bool,
    watch: bool,
    follow_symlinks: bool,
    ignore_options: FilePickerIgnoreOptions,
    enable_fs_root_scanning: bool,
    enable_home_dir_scanning: bool,
    git_recency_config: GitRecencyConfig,
    rescan_cooldown: Option<std::time::Duration>,
    trace_span: tracing::Span,
    trace_id: String,
}

impl std::fmt::Debug for FilePicker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilePicker")
            .field("base_path", &self.base_path)
            .field("sync_data", &self.sync_data)
            .field(
                "is_scanning",
                &self.signals.scanning.load(Ordering::Relaxed),
            )
            .field(
                "scanned_files_count",
                &self.scanned_files_count.load(Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

impl FFFStringStorage for &FilePicker {
    #[inline]
    fn arena_for(&self, file: &FileItem) -> crate::simd_path::ArenaPtr {
        self.sync_data.arena_for_file(file)
    }

    #[inline]
    fn base_arena(&self) -> crate::simd_path::ArenaPtr {
        self.sync_data.arena_base_ptr()
    }

    #[inline]
    fn overflow_arena(&self) -> crate::simd_path::ArenaPtr {
        self.sync_data.arena_overflow_ptr()
    }
}

impl FilePicker {
    /// Whether the index holds the scope directory as a live directory.
    pub fn contains_scope(&self, scope: &SearchScope) -> bool {
        scope.is_root()
            || self
                .sync_data
                .find_dir_index(scope.dir())
                .is_some_and(|index| !self.sync_data.dirs[index].is_deleted())
    }

    pub fn base_path(&self) -> &Path {
        &self.base_path
    }

    pub fn has_git_repo(&self) -> bool {
        self.sync_data.git.is_some()
    }

    /// Ignore rules the walker assembled during the last scan (zlob backend
    /// only). The background watcher uses these to filter events without
    /// libgit2. `None` when the backend doesn't surface rules or no ignore
    /// files were present.
    pub(crate) fn ignore_rules(&self) -> Option<Arc<crate::walk::WalkIgnoreRules>> {
        self.sync_data.ignore_rules.clone()
    }

    pub fn has_mmap_cache(&self) -> bool {
        self.enable_mmap_cache
    }

    pub fn has_content_indexing(&self) -> bool {
        self.enable_content_indexing
    }

    pub fn has_watcher(&self) -> bool {
        self.watch
    }

    pub fn is_watcher_ready(&self) -> bool {
        self.background_watcher.is_some() && self.signals.watcher_ready.load(Ordering::Acquire)
    }

    pub fn follows_symlinks(&self) -> bool {
        self.follow_symlinks
    }

    /// Returns the ignore options for this picker.
    pub fn ignore_options(&self) -> &FilePickerIgnoreOptions {
        &self.ignore_options
    }

    pub fn fs_root_scanning_enabled(&self) -> bool {
        self.enable_fs_root_scanning
    }

    pub fn home_dir_scanning_enabled(&self) -> bool {
        self.enable_home_dir_scanning
    }

    pub fn git_recency_config(&self) -> GitRecencyConfig {
        self.git_recency_config
    }

    pub(crate) fn rescan_cooldown(&self) -> std::time::Duration {
        self.rescan_cooldown.unwrap_or_else(|| {
            crate::rescan_throttle::rescan_cooldown(self.live_file_count(), self.has_git_repo())
        })
    }

    pub fn trace_id(&self) -> &str {
        &self.trace_id
    }

    pub fn trace_span(&self) -> tracing::Span {
        self.trace_span.clone()
    }

    pub fn mode(&self) -> FFFMode {
        self.mode
    }

    pub fn cache_budget(&self) -> &ContentCacheBudget {
        &self.cache_budget
    }

    pub fn bigram_index(&self) -> Option<&BigramFilter> {
        self.sync_data.bigram_index.as_deref()
    }

    pub fn bigram_overlay(&self) -> Option<&parking_lot::RwLock<BigramOverlay>> {
        self.sync_data.bigram_overlay.as_deref()
    }

    pub fn get_file_mut(&mut self, index: usize) -> Option<(ArenaPtr, &mut FileItem)> {
        self.sync_data.get_file_mut(index)
    }

    /// Absolute path to the repository root if the indexed tree lives
    /// inside a git working directory. `None` for non-git bases.
    pub fn git_root(&self) -> Option<&Path> {
        self.sync_data.git.as_ref().map(|git| git.workdir.as_path())
    }

    /// Path keying the project history: frecency, query history and combo boosts.
    /// Inside a linked git worktree it is the main checkout twin of the base path.
    pub fn shared_base_path(&self) -> &Path {
        self.sync_data
            .shared_base
            .as_deref()
            .unwrap_or(&self.base_path)
    }

    /// Main checkout twin of `path` when the picker indexes a linked git worktree.
    pub fn shared_path_for(&self, path: impl AsRef<Path>) -> Option<PathBuf> {
        let shared_base = self.sync_data.shared_base.as_deref()?;
        crate::git::shared_twin(path.as_ref(), &self.base_path, shared_base)
    }

    pub(crate) fn worktree_shared_base(&self) -> Option<&Path> {
        self.sync_data.shared_base.as_deref()
    }

    pub fn has_explicit_cache_budget(&self) -> bool {
        self.has_explicit_cache_budget
    }

    pub fn set_cache_budget(&mut self, budget: ContentCacheBudget) {
        self.cache_budget = Arc::new(budget);
    }

    /// Get all indexed files sorted by path.
    /// Note: Files are stored sorted by PATH for efficient insert/remove.
    /// For frecency-sorted results, use search() which sorts matched results.
    pub fn get_files(&self) -> &[FileItem] {
        self.sync_data.files()
    }

    /// Count of live (non-tombstoned) files. O(1).
    #[inline]
    pub fn live_file_count(&self) -> usize {
        self.sync_data.live_count
    }

    /// Count of live files inside `scope`.
    pub fn live_file_count_in(&self, scope: &SearchScope) -> usize {
        self.live_files_in(self.sync_data.resolve_scope(scope).as_ref())
    }

    pub fn get_overflow_files(&self) -> &[FileItem] {
        self.sync_data.overflow_files()
    }

    /// Get the directory table (sorted by path).
    pub fn get_dirs(&self) -> &[DirItem] {
        &self.sync_data.dirs
    }

    /// Actual heap bytes used: (chunked_path_store, 0, 0).
    /// The second element is 0 because leaked overflow stores aren't tracked.
    pub fn arena_bytes(&self) -> (usize, usize, usize) {
        let chunked = self
            .sync_data
            .chunked_paths
            .as_ref()
            .map_or(0, |s| s.heap_bytes());

        (chunked, 0, 0)
    }

    /// Create a new FilePicker from options.
    /// Always prefer new_with_shared_state for the consumer application, use this only if you know
    /// what you are doing. This won't spawn the backgraound watcher and won't walk the file tree.
    pub fn new(options: FilePickerOptions) -> Result<Self, Error> {
        Self::new_with_ignore_options(options, FilePickerIgnoreOptions::default())
    }

    fn new_with_ignore_options(
        options: FilePickerOptions,
        ignore_options: FilePickerIgnoreOptions,
    ) -> Result<Self, Error> {
        if !ignore_options.disable_git {
            crate::git::tune_libgit2_for_local_reads();
        }

        let path = PathBuf::from(&options.base_path);
        if !path.exists() {
            error!("Base path does not exist: {}", options.base_path);
            return Err(Error::InvalidPath(path));
        }
        // Relative bases (".", "sub/dir") are resolved against the cwd so
        // they can be compared with the absolute paths reported by the OS
        // watcher. Purely lexical: no symlinks are resolved. The
        // `components()` pass drops interior `.` segments ("/cwd/.").
        let path = if path.is_relative() {
            std::env::current_dir()
                .map(|cwd| cwd.join(&path).components().collect())
                .unwrap_or(path)
        } else {
            path
        };
        crate::path_utils::validate_base_path(
            &path,
            options.enable_fs_root_scanning,
            options.enable_home_dir_scanning,
        )
        .inspect_err(|e| error!("Refusing to index {}: {e}", path.display()))?;

        // Windows-only: canonicalize with dunce so the base path does NOT
        // have the `\\?\` UNC prefix that `std::fs::canonicalize` adds.
        // libgit2's `repo.workdir()`
        #[cfg(windows)]
        let path = crate::path_utils::canonicalize(&path).unwrap_or(path);

        let has_explicit_budget = options.cache_budget.is_some();
        let initial_budget = options.cache_budget.unwrap_or_default();

        let trace_id = crate::log::generate_trace_id();
        let trace_span = crate::log::trace_span(&trace_id, "picker");

        Ok(FilePicker {
            background_watcher: None,
            git_status_worker: crate::git_status_worker::GitStatusWorker::new(),
            base_path: path,
            cache_budget: Arc::new(initial_budget),
            has_explicit_cache_budget: has_explicit_budget,
            signals: crate::scan::ScanSignals::default(),
            mode: options.mode,
            scanned_files_count: Arc::new(AtomicUsize::new(0)),
            sync_data: FileSync::new(),
            sync_generation: next_sync_generation(),
            coverage: Arc::default(),
            enable_mmap_cache: options.enable_mmap_cache,
            enable_content_indexing: options.enable_content_indexing,
            watch: options.watch,
            follow_symlinks: options.follow_symlinks,
            ignore_options,
            enable_fs_root_scanning: options.enable_fs_root_scanning,
            enable_home_dir_scanning: options.enable_home_dir_scanning,
            git_recency_config: options.git_recency,
            rescan_cooldown: options.rescan_cooldown,
            trace_span,
            trace_id,
        })
    }

    /// Create a shared picker and start background indexing and watching.
    pub fn new_with_shared_state(
        shared_picker: SharedFilePicker,
        shared_frecency: SharedFrecency,
        options: FilePickerOptions,
    ) -> Result<(), Error> {
        Self::new_with_shared_state_and_ignore_options(
            shared_picker,
            shared_frecency,
            options,
            FilePickerIgnoreOptions::default(),
        )
    }

    /// Starts background indexing with the supplied ignore options.
    pub fn new_with_shared_state_and_ignore_options(
        shared_picker: SharedFilePicker,
        shared_frecency: SharedFrecency,
        options: FilePickerOptions,
        ignore_options: FilePickerIgnoreOptions,
    ) -> Result<(), Error> {
        let extra_ignore = &ignore_options.extra_ignore;
        if !cfg!(feature = "zlob") && !extra_ignore.is_empty() {
            return Err(Error::WalkFailed(
                "extra ignore rules require the zlob walker".into(),
            ));
        }
        if !cfg!(feature = "zlob")
            && (ignore_options.disable_git || ignore_options.disable_ignore_files)
        {
            return Err(Error::WalkFailed(
                "disabling Git or .ignore files requires the zlob walker".into(),
            ));
        }
        if extra_ignore.iter().any(|rule| rule.contains('\0')) {
            return Err(Error::WalkFailed(
                "extra ignore rules cannot contain NUL bytes".into(),
            ));
        }

        let mut picker = Self::new_with_ignore_options(options, ignore_options.clone())?;
        picker.coverage = Arc::clone(shared_picker.coverage_listeners());
        picker.git_status_worker = crate::git_status_worker::GitStatusWorker::with_pause_flag(
            shared_picker.git_status_pause_flag(),
        );

        info!(
            "Spawning background threads: base_path={}, warmup={}, content_indexing={}, mode={:?}",
            picker.base_path.display(),
            picker.enable_mmap_cache,
            picker.enable_content_indexing,
            picker.mode,
        );

        let warmup = picker.enable_mmap_cache;
        let content_indexing = picker.enable_content_indexing;
        let watch = picker.watch;
        let mode = picker.mode;
        let follow_symlinks = picker.follow_symlinks;
        let enable_fs_root_scanning = picker.enable_fs_root_scanning;
        let enable_home_dir_scanning = picker.enable_home_dir_scanning;

        let signals = picker.scan_signals();
        let scanned_files_counter = picker.scanned_files_counter();
        let path = picker.base_path.clone();
        let trace_span = picker.trace_span.clone();

        // Pre-arm `scanning` BEFORE publishing the new picker. `ScanJob::spawn`
        // also sets it, but that runs after this function returns; consumers
        // (e.g. lua `wait_for_initial_scan` after `restart_index_in_path`)
        // that grab the signal Arc between publish and spawn would otherwise
        // observe scanning=false and skip the wait, racing the walker. The
        // race is wide on Windows CI where notify is slow.
        signals
            .scanning
            .store(true, std::sync::atomic::Ordering::Release);
        // The initial scan owns the new picker's single scan job from the start.
        signals
            .scan_job_running
            .store(true, std::sync::atomic::Ordering::SeqCst);

        // Update the watch base before publishing the new picker.
        shared_picker.rebase_watches(&path);
        // A rescan deferred for the previous index has nothing left to recover.
        shared_picker.cancel_deferred_rescan();

        {
            let mut guard = shared_picker.write()?;
            *guard = Some(picker);
            // dropping old picker flips its `cancelled` flag → bg threads exit cleanly
        }
        shared_picker.coverage_listeners().notify(&());

        ScanJob::new_initial(
            shared_picker,
            shared_frecency,
            path,
            mode,
            signals,
            scanned_files_counter,
            trace_span,
            ScanConfig {
                warmup,
                content_indexing,
                watch,
                auto_cache_budget: true,
                install_watcher: true,
                follow_symlinks,
                ignore_options,
                enable_fs_root_scanning,
                enable_home_dir_scanning,
            },
        )
        .spawn();

        Ok(())
    }

    /// Synchronous filesystem scan — populates `self` with indexed files.
    ///
    /// Use this when you need direct access to the picker without shared state:
    /// ```ignore
    /// let mut picker = FilePicker::new(options)?;
    /// picker.collect_files()?;
    /// // picker.get_files() is now populated
    /// ```
    pub fn collect_files(&mut self) -> Result<(), Error> {
        self.signals.scanning.store(true, Ordering::Relaxed);
        self.scanned_files_count.store(0, Ordering::Relaxed);

        let git = FileSync::discover_git(&self.base_path, self.ignore_options.disable_git);
        let git_handle = git
            .as_ref()
            .map(|git| FileSync::spawn_git_status(git.workdir.clone()));

        let empty_frecency = SharedFrecency::default();
        let sync = FileSync::walk_filesystem(
            &self.base_path,
            git,
            &self.scanned_files_count,
            &empty_frecency,
            self.mode,
            crate::walk::WalkPolicy {
                follow_symlinks: self.follow_symlinks,
                ignore: &self.ignore_options,
            },
            None,
        )?;

        self.replace_sync(sync);
        self.coverage.notify(&());

        if !self.has_explicit_cache_budget {
            let file_count = self.sync_data.files().len();
            self.cache_budget = Arc::new(ContentCacheBudget::new_for_repo(file_count));
        } else {
            self.cache_budget.reset();
        }

        if let Some(handle) = git_handle
            && let Ok(Some(git_cache)) = handle.join()
        {
            let mut path_buf = [0u8; crate::simd_path::PATH_BUF_SIZE];

            let arena = self.arena_base_ptr();
            for file in self.sync_data.files.iter_mut() {
                file.git_status = git_cache.lookup_status(file.write_absolute_path(
                    arena,
                    &self.base_path,
                    &mut path_buf,
                ));
            }
        }

        if let Some(workdir) = self.git_root().map(Path::to_path_buf)
            && let Ok(repo) = Repository::open(&workdir)
                .inspect_err(|e| debug!(?e, ?workdir, "git recency: failed to open repo"))
        {
            let recency =
                git_recency::compute_git_recency(&repo, &self.git_recency_config, &self.base_path);
            self.apply_git_recency(recency.as_ref());
        }

        self.signals.scanning.store(false, Ordering::Relaxed);
        Ok(())
    }

    /// Perform fuzzy search on files with a pre-parsed query.
    ///
    /// The query should be parsed using [`crate::FFFQuery`] before calling
    /// this function. If a [`crate::QueryTracker`] is provided, the search will
    /// automatically look up the last selected file for this query and boost it
    #[tracing::instrument(skip_all, name = "Fuzzy file search", fields(query = query.raw_query))]
    pub fn fuzzy_search<'q>(
        &self,
        query: &'q FFFQuery<'q>,
        query_tracker: Option<&QueryTracker>,
        options: FuzzySearchOptions<'q>,
    ) -> SearchResult<'_> {
        let scope = self.sync_data.resolve_scope(options.scope);
        let files = self.get_files();
        let max_threads = if options.max_threads == 0 {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
        } else {
            options.max_threads
        };

        debug!(
            raw_query = ?query.raw_query,
            pagination = ?options.pagination,
            ?max_threads,
            current_file = ?options.current_file,
            "Fuzzy search",
        );

        let total_files = self.live_files_in(scope.as_ref());
        let location = query.location;

        // Get effective query for max_typos calculation (without location suffix)
        let effective_query = match &query.fuzzy_query {
            fff_query_parser::FuzzyQuery::Text(t) => *t,
            fff_query_parser::FuzzyQuery::Parts(parts) if !parts.is_empty() => parts[0],
            _ => query.raw_query.trim(),
        };

        // small queries with a large number of results can match absolutely everything
        let max_typos = (effective_query.len() as u16 / 4).clamp(2, 6);
        // Look up the last file selected for this query (combo-boost scoring)
        let last_same_query_entry =
            query_tracker
                .zip(options.project_path)
                .and_then(|(tracker, project_path)| {
                    tracker
                        .get_last_query_entry(
                            query.raw_query,
                            project_path,
                            options.min_combo_count,
                        )
                        .ok()
                        .flatten()
                });

        let context = ScoringContext {
            query,
            max_typos,
            max_threads,
            project_path: options.project_path,
            current_file: options.current_file,
            last_same_query_match: last_same_query_entry,
            combo_boost_score_multiplier: options.combo_boost_score_multiplier,
            min_combo_count: options.min_combo_count,
            pagination: options.pagination,
            exclude_frecency: options.exclude_frecency,
        };

        let time = std::time::Instant::now();

        let arenas = self.arenas();
        let (items, scores, total_matched) = fuzzy_match_and_score_files(
            files,
            &context,
            self.sync_data.base_count,
            arenas,
            scope.as_ref(),
        );
        let match_byte_offsets =
            fuzzy_match_byte_offsets_for_page(query, &items, max_typos, arenas, scope.as_ref());

        info!(
            ?query,
            completed_in = ?time.elapsed(),
            total_matched,
            returned_count = items.len(),
            pagination = ?options.pagination,
            "Fuzzy search completed",
        );

        SearchResult {
            items,
            scores,
            match_byte_offsets,
            total_matched,
            total_files,
            location,
        }
    }

    /// Perform fuzzy search on indexed directories.
    ///
    /// Returns directories ranked by fuzzy match quality + frecency.
    pub fn fuzzy_search_directories<'q>(
        &self,
        query: &'q FFFQuery<'q>,
        options: FuzzySearchOptions<'q>,
    ) -> DirSearchResult<'_> {
        let scope = self.sync_data.resolve_scope(options.scope);
        let dirs = self.get_dirs();
        let max_threads = if options.max_threads == 0 {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
        } else {
            options.max_threads
        };

        let total_dirs = scope
            .as_ref()
            .map_or(self.sync_data.live_dirs_count, |scope| {
                scope.live_dir_count(dirs)
            });

        let effective_query = match &query.fuzzy_query {
            fff_query_parser::FuzzyQuery::Text(t) => *t,
            fff_query_parser::FuzzyQuery::Parts(parts) if !parts.is_empty() => parts[0],
            _ => query.raw_query.trim(),
        };

        let max_typos = (effective_query.len() as u16 / 4).clamp(2, 6);

        let context = ScoringContext {
            query,
            max_typos,
            max_threads,
            project_path: options.project_path,
            current_file: options.current_file,
            last_same_query_match: None,
            combo_boost_score_multiplier: 0,
            min_combo_count: 0,
            pagination: options.pagination,
            exclude_frecency: options.exclude_frecency,
        };

        let time = std::time::Instant::now();

        let (items, scores, total_matched) = fuzzy_match_and_score_dirs(
            dirs,
            self.sync_data.base_dirs_count,
            &context,
            self.arenas(),
            scope.as_ref(),
        );

        info!(
            ?query,
            completed_in = ?time.elapsed(),
            total_matched,
            returned_count = items.len(),
            "Directory search completed",
        );

        DirSearchResult {
            items,
            scores,
            total_matched,
            total_dirs,
        }
    }

    /// Perform a mixed fuzzy search across both files and directories.
    ///
    /// Returns a single flat list where files and directories are interleaved
    /// by total score in descending order.
    ///
    /// If the raw query ends with a path separator (`/`), only directories
    /// are searched — files are skipped entirely. The caller should parse the
    /// query with `DirSearchConfig` so that trailing `/` is kept as fuzzy
    /// text instead of becoming a `PathSegment` constraint.
    pub fn fuzzy_search_mixed<'q>(
        &self,
        query: &'q FFFQuery<'q>,
        query_tracker: Option<&QueryTracker>,
        options: FuzzySearchOptions<'q>,
    ) -> MixedSearchResult<'_> {
        let location = query.location;
        let page_offset = options.pagination.offset;
        let page_limit = if options.pagination.limit > 0 {
            options.pagination.limit
        } else {
            100
        };

        let dirs_only =
            query.raw_query.ends_with(std::path::MAIN_SEPARATOR) || query.raw_query.ends_with('/');

        // Run file search and dir search with no pagination (we merge then paginate).
        let internal_limit = page_offset.saturating_add(page_limit).saturating_mul(2);

        let dir_options = FuzzySearchOptions {
            pagination: PaginationArgs {
                offset: 0,
                limit: internal_limit,
            },
            ..options
        };
        let dir_results = self.fuzzy_search_directories(query, dir_options);

        if dirs_only {
            let total_matched = dir_results.total_matched;
            let total_dirs = dir_results.total_dirs;
            let total_files =
                self.live_files_in(self.sync_data.resolve_scope(options.scope).as_ref());

            let mut merged: Vec<(MixedItemRef<'_>, Score)> =
                Vec::with_capacity(dir_results.items.len());
            for (dir, score) in dir_results.items.into_iter().zip(dir_results.scores) {
                merged.push((MixedItemRef::Dir(dir), score));
            }

            if page_offset >= merged.len() {
                return MixedSearchResult {
                    items: vec![],
                    scores: vec![],
                    total_matched,
                    total_files,
                    total_dirs,
                    location,
                };
            }

            let end = (page_offset + page_limit).min(merged.len());
            let page = merged.drain(page_offset..end);
            let (items, scores): (Vec<_>, Vec<_>) = page.unzip();

            return MixedSearchResult {
                items,
                scores,
                total_matched,
                total_files,
                total_dirs,
                location,
            };
        }

        let file_options = FuzzySearchOptions {
            pagination: PaginationArgs {
                offset: 0,
                limit: internal_limit,
            },
            ..options
        };
        let file_results = self.fuzzy_search(query, query_tracker, file_options);

        // Merge by score descending.
        let total_matched = file_results.total_matched + dir_results.total_matched;
        let total_files = file_results.total_files;
        let total_dirs = dir_results.total_dirs;

        let mut merged: Vec<(MixedItemRef<'_>, Score)> =
            Vec::with_capacity(file_results.items.len() + dir_results.items.len());

        for (file, score) in file_results.items.into_iter().zip(file_results.scores) {
            merged.push((MixedItemRef::File(file), score));
        }
        for (dir, score) in dir_results.items.into_iter().zip(dir_results.scores) {
            merged.push((MixedItemRef::Dir(dir), score));
        }

        if crate::score::orders_by_path(query, options.exclude_frecency) {
            merged.sort_by_cached_key(|(item, _)| match item {
                MixedItemRef::File(file) => file.relative_path(self),
                MixedItemRef::Dir(dir) => dir.relative_path(self),
            });
        } else {
            // Sort merged results by total score descending.
            merged.sort_unstable_by_key(|b| std::cmp::Reverse(b.1.total));
        }

        // Paginate.
        if page_offset >= merged.len() {
            return MixedSearchResult {
                items: vec![],
                scores: vec![],
                total_matched,
                total_files,
                total_dirs,
                location,
            };
        }

        let end = (page_offset + page_limit).min(merged.len());
        let page = merged.drain(page_offset..end);
        let (items, scores): (Vec<_>, Vec<_>) = page.unzip();

        MixedSearchResult {
            items,
            scores,
            total_matched,
            total_files,
            total_dirs,
            location,
        }
    }

    /// Glob search: filter indexed files by a single glob pattern, rank by
    /// frecency, and paginate. Bypasses the regular query parser entirely —
    /// useful when callers already have a literal glob (`*.rs`, `**/*.test.ts`)
    /// and want neither fuzzy matching nor multi-token constraint parsing.
    ///
    /// Pipeline: `apply_constraints(Glob) → score_filtered_by_frecency → sort_and_paginate`.
    /// Same ranking semantics as `fuzzy_search` when the fuzzy query is empty.
    pub fn glob<'p>(
        &'p self,
        pattern: &'p str,
        options: FuzzySearchOptions<'p>,
    ) -> SearchResult<'p> {
        let query = FFFQuery {
            raw_query: pattern,
            constraints: vec![fff_query_parser::Constraint::Glob(pattern)],
            fuzzy_query: fff_query_parser::FuzzyQuery::Empty,
            location: None,
        };

        // `fuzzy_search` short-circuits to `score_filtered_by_frecency` when
        // `fuzzy_query` is `Empty`, then runs the same `sort_and_paginate`
        // path. Reusing it keeps the ranking guarantees identical without
        // exposing the private scoring helpers.
        self.fuzzy_search(&query, None, options)
    }

    /// Perform a live grep search across indexed files.
    ///
    /// If `options.abort_signal` is set it overrides the picker's internal
    /// cancellation flag, giving the caller full control over when to stop.
    pub fn grep(&self, query: &FFFQuery<'_>, options: &GrepSearchOptions) -> GrepResult<'_> {
        let scope = self.sync_data.resolve_scope(&options.scope);
        let overlay_guard = self.sync_data.bigram_overlay.as_ref().map(|o| o.read());
        let cancel = options
            .abort_signal
            .as_deref()
            .unwrap_or(&self.signals.cancelled);

        grep_search(
            self.get_files(),
            self.live_files_in(scope.as_ref()),
            query,
            options,
            self.cache_budget(),
            self.sync_data.bigram_index.as_deref(),
            overlay_guard.as_deref(),
            cancel,
            &self.base_path,
            self.arenas(),
            scope.as_ref(),
        )
    }

    /// Multi-pattern grep search across indexed files. `options.scope` applies
    /// independently of `constraints`.
    pub fn multi_grep(
        &self,
        patterns: &[&str],
        constraints: &[fff_query_parser::Constraint<'_>],
        options: &GrepSearchOptions,
    ) -> GrepResult<'_> {
        let scope = self.sync_data.resolve_scope(&options.scope);
        let overlay_guard = self.sync_data.bigram_overlay.as_ref().map(|o| o.read());
        let cancel = options
            .abort_signal
            .as_deref()
            .unwrap_or(&self.signals.cancelled);

        multi_grep_search(
            self.get_files(),
            patterns,
            constraints,
            options,
            self.cache_budget(),
            self.sync_data.bigram_index.as_deref(),
            overlay_guard.as_deref(),
            cancel,
            &self.base_path,
            self.arenas(),
            scope.as_ref(),
        )
    }

    fn arenas(&self) -> Arenas {
        Arenas::new(
            self.sync_data.arena_base_ptr(),
            self.sync_data.arena_overflow_ptr(),
        )
    }

    fn live_files_in(&self, scope: Option<&ResolvedScope>) -> usize {
        scope.map_or(self.live_file_count(), |scope| {
            scope.live_file_count(self.get_files())
        })
    }

    // Returns an ongoing or finisshed scan progress
    pub fn get_scan_progress(&self) -> ScanProgress {
        let scanned_count = self.scanned_files_count.load(Ordering::Relaxed);
        let is_scanning = self.signals.scanning.load(Ordering::Relaxed);

        ScanProgress {
            scanned_files_count: scanned_count,
            is_scanning,
            is_watcher_ready: self.signals.watcher_ready.load(Ordering::Relaxed),
            is_warmup_complete: !self.enable_content_indexing
                || self.sync_data.bigram_index.is_some(),
        }
    }

    pub(crate) fn set_bigram_index(&mut self, index: BigramFilter) {
        self.sync_data.bigram_index = Some(Arc::new(index));
        // A committed scan's overlay already holds the changes made while the index was built.
        let indexable_count = self.sync_data.indexable_count;
        self.sync_data
            .bigram_overlay
            .get_or_insert_with(|| new_bigram_overlay(indexable_count));
    }

    pub(crate) fn scan_signals(&self) -> crate::scan::ScanSignals {
        self.signals.clone()
    }

    pub(crate) fn scanned_files_counter(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.scanned_files_count)
    }

    /// Capture raw pointers to the picker's internal arrays for off-lock use.
    ///
    /// Sets `post_scan_indexing_active` and returns a snapshot that clears it
    /// on drop. This is the ONLY approved way to escape the lock for
    /// long-running parallel work (git status, warmup, bigram).
    ///
    /// Returns `None` if `post_scan_indexing_active` is already set — this
    /// means another post-scan is in flight and we must not create a second
    /// set of dangling pointers.
    ///
    /// # Safety
    /// 1. `walk_filesystem` reserved `MAX_OVERFLOW_FILES` capacity on the
    ///    files Vec at creation — watcher pushes cannot reallocate it.
    /// 2. `post_scan_indexing_active` is set — prevents `commit_new_sync`
    ///    from replacing the Vec (the scan job holding the snapshot is the only one).
    /// 3. Only `[..base_count]` is accessed — base files use the immutable
    ///    base arena. Overflow files use a different arena.
    pub(crate) unsafe fn post_scan_snapshot(&self) -> Option<PostScanUnsafeSnapshot> {
        if self
            .signals
            .post_scan_indexing_active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            tracing::error!(
                "Can not acquire post scan unsafe snapshot, someone already acquired it"
            );
            return None;
        }

        Some(PostScanUnsafeSnapshot {
            files: self.sync_data.files.clone(),
            arena: self.sync_data.chunked_paths.as_ref().map(Arc::clone),
            base_count: self.sync_data.base_count,
            indexable_count: self.sync_data.indexable_count,
            base_path: self.base_path.clone(),
            post_scan_flag: Arc::clone(&self.signals.post_scan_indexing_active),
            _budget: Arc::clone(&self.cache_budget),
        })
    }

    pub(crate) fn unchanged_text_files(&self, previous: &FileSync) -> Vec<u64> {
        self.sync_data.unchanged_text_files(previous)
    }

    // Returns the replaced index so the caller can compare against it and drop it off the lock
    pub(crate) fn commit_new_sync(&mut self, mut sync: FileSync) -> FileSync {
        // Files may change after the content index has read them; the overlay keeps those edits.
        if self.enable_content_indexing {
            sync.bigram_overlay = Some(new_bigram_overlay(sync.indexable_count));
        }
        let previous = self.replace_sync(sync);
        self.cache_budget.reset();
        self.coverage.notify(&());
        previous
    }

    // Applies a dir-table change and reports it when the set of live dirs changed.
    fn update_dirs<R>(&mut self, update: impl FnOnce(&mut FileSync) -> R) -> R {
        let live_dirs = self.sync_data.live_dirs_count;
        let result = update(&mut self.sync_data);
        if self.sync_data.live_dirs_count != live_dirs {
            self.coverage.notify(&());
        }
        result
    }

    #[inline]
    pub(crate) fn arena_base_ptr(&self) -> ArenaPtr {
        self.sync_data.arena_base_ptr()
    }

    /// Update git statuses for files, using the provided shared frecency tracker.
    pub(crate) fn update_git_statuses(
        &mut self,
        status_cache: GitStatusCache,
        shared_frecency: &SharedFrecency,
    ) -> Result<(), Error> {
        debug!(
            statuses_count = status_cache.statuses_len(),
            "Updating git status",
        );

        let mode = self.mode;
        let bp = self.base_path.clone();
        let shared_base = self.sync_data.shared_base.clone();
        let frecency = shared_frecency.read()?;
        let reader = frecency.as_ref().map(FrecencyTracker::reader);

        let mut directories =
            (status_cache.statuses_len() >= 8192).then(AHashMap::<String, Option<usize>>::new);
        status_cache
            .into_iter()
            .try_for_each(|(path, status)| -> Result<(), Error> {
                let index = if let Some(cache) = directories.as_mut() {
                    self.sync_data.find_file_index_with(&path, &bp, |dir| {
                        if let Some(&index) = cache.get(dir) {
                            return index;
                        }
                        let index = self.sync_data.find_base_dir_index(dir);
                        cache.insert(dir.to_owned(), index);
                        index
                    })
                } else {
                    self.sync_data.find_file_index(&path, &bp)
                };
                if let Some((arena, file)) = index.and_then(|i| self.sync_data.get_file_mut(i)) {
                    file.git_status = Some(status);
                    if let Some(reader) = reader.as_ref() {
                        file.update_frecency_scores(
                            reader,
                            arena,
                            &bp,
                            shared_base.as_deref(),
                            mode,
                        );
                    }
                    // Update parent dir frecency inline. `DirItem` has an
                    // interior-mutable atomic score, so `&self` access is
                    // enough — no write aliasing against Arc clones.
                    let score = file.access_frecency_score as i32;
                    let dir_idx = file.parent_dir_index as usize;
                    if let Some(dir) = self.sync_data.dirs.get(dir_idx) {
                        dir.update_frecency_if_larger(score);
                    }
                } else {
                    // Expected on sparse checkouts: git reports a status for
                    // a path that isn't materialized on disk and therefore
                    // isn't in the file index. Don't spam the log (#404).
                    debug!(?path, "Git status for path not in index, skipping");
                }
                Ok(())
            })?;

        Ok(())
    }

    // Replaces every recency score with a freshly computed set. `None` zeroes
    // them, so a vanished window (orphan HEAD, repo gone) leaves no stale boost.
    pub(crate) fn apply_git_recency(&mut self, scores: Option<&AHashMap<String, i16>>) {
        if !self.git_recency_config.enabled {
            return;
        }

        for file in self.sync_data.files.iter_mut() {
            file.git_recency_score = 0;
        }

        let Some(scores) = scores else { return };

        let mut applied = 0usize;
        for (relative_path, score) in scores {
            if let Some(index) = self.sync_data.find_by_relative_path(relative_path)
                && let Some((_, file)) = self.sync_data.get_file_mut(index)
            {
                file.git_recency_score = *score;
                applied += 1;
            }
        }

        debug!(files_scored = applied, "Git recency scores applied");
    }

    pub fn update_single_file_frecency(
        &mut self,
        file_path: impl AsRef<Path>,
        frecency_tracker: &FrecencyTracker,
    ) -> Result<(), Error> {
        let path = file_path.as_ref();

        let Some(index) = self.sync_data.find_file_index(path, &self.base_path) else {
            return Ok(());
        };

        let shared_base = self.sync_data.shared_base.clone();
        if let Some((arena, file)) = self.sync_data.get_file_mut(index) {
            file.update_frecency_scores(
                &frecency_tracker.reader(),
                arena,
                &self.base_path,
                shared_base.as_deref(),
                self.mode,
            );

            // Update parent dir frecency inline (atomic, &self access).
            let score = file.access_frecency_score as i32;
            let dir_idx = file.parent_dir_index as usize;
            if let Some(dir) = self.sync_data.dirs.get(dir_idx) {
                dir.update_frecency_if_larger(score);
            }
        }

        Ok(())
    }

    pub fn get_file_by_path(&self, path: impl AsRef<Path>) -> Option<&FileItem> {
        self.sync_data
            .find_file_index(path.as_ref(), &self.base_path)
            .and_then(|index| self.sync_data.files().get(index))
    }

    pub fn get_mut_file_by_path(
        &mut self,
        path: impl AsRef<Path>,
    ) -> Option<(ArenaPtr, &mut FileItem)> {
        let path = path.as_ref();
        let index = self.sync_data.find_file_index(path, &self.base_path);
        index.and_then(|i| self.sync_data.get_file_mut(i))
    }

    /// Handle the event of certain file being modified or adds a neww file if it is not added
    /// If this function returns `None` it means that picker is in the invalid state, or the capacity
    /// of index is exhausted and a new rescan needs to be triggered.
    pub fn handle_create_or_modify(&mut self, path: impl AsRef<Path> + Debug) -> Option<&FileItem> {
        self.apply_create_or_modify(path.as_ref()).ok()
    }

    #[tracing::instrument(skip(self), level = Level::DEBUG)]
    pub(crate) fn apply_create_or_modify(
        &mut self,
        path: &Path,
    ) -> Result<&FileItem, IndexUpdateError> {
        if let Some(idx) = self.sync_data.find_file_index(path, &self.base_path) {
            let slot = if idx < self.sync_data.base_count {
                FileSlot::Base(idx)
            } else {
                FileSlot::Overflow(idx)
            };

            return self.handle_file_modify(path, slot);
        }

        self.insert_new_file(path)
    }

    #[tracing::instrument(skip_all, fields(path = ?path), level = Level::DEBUG)]
    fn handle_file_modify(
        &mut self,
        path: &Path,
        slot: FileSlot,
    ) -> Result<&FileItem, IndexUpdateError> {
        let overlay = self.sync_data.bigram_overlay.as_ref().map(Arc::clone);
        let pos = slot.index();

        // this is the only way to actually know if the file is on disk, we CAN NOT
        // rely on the watcher to proive the latest state of the file, do the actual check
        let metadata = match std::fs::metadata(path) {
            Ok(m) => {
                self.untombstone_file(pos);

                m
            }
            Err(_) => {
                self.tombstone_file(pos);
                return Err(IndexUpdateError::Vanished);
            }
        };

        let (_arena, file) = self
            .sync_data
            .get_file_mut(pos)
            .ok_or(IndexUpdateError::Vanished)?;

        let size = metadata.len();
        let modified_time = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());

        file.update_metadata(&self.cache_budget, modified_time, Some(size));
        file.ino = crate::types::inode_bits(&metadata);

        // Re-classify binary status from current content (chunked, fixed
        // buffer). Already-binary files are left alone.
        if !file.is_binary() {
            let mut chunk = [0u8; crate::types::BINARY_CLASSIFICATION_CHUNK_SIZE];
            file.detect_binary_per_byte(path, &mut chunk);
        }

        // Indexable base-region files feed fresh content to the bigram overlay.
        if matches!(slot, FileSlot::Base(_))
            && let Some(ref overlay) = overlay
        {
            let in_indexable = {
                let guard = overlay.read();
                pos < guard.base_file_count()
            };

            if in_indexable && let Ok(content) = std::fs::read(path) {
                overlay.write().modify_file(pos, &content);
            }
        }

        self.sync_data
            .files()
            .get(pos)
            .ok_or(IndexUpdateError::Vanished)
    }

    /// Adds a new file to picker, if the file can not be added returns `None`
    /// which indicates that it's time to trigger a new sync
    pub fn add_new_file(&mut self, path: &Path) -> Option<&FileItem> {
        self.insert_new_file(path).ok()
    }

    // Nothing is written to the overflow arena unless the file lands.
    #[tracing::instrument(skip(self))]
    fn insert_new_file(&mut self, path: &Path) -> Result<&FileItem, IndexUpdateError> {
        // The overflow arena is sized for MAX_OVERFLOW_FILES; the files vector
        // may have more spare room than that.
        if self.get_overflow_files().len() >= MAX_OVERFLOW_FILES
            || self.sync_data.files.remaining_capacity() == 0
        {
            return Err(IndexUpdateError::CapacityExhausted);
        }

        // On Windows `pathdiff::diff_paths` is byte-wise, so a short-name
        // input never shares a prefix with the canonicalized base_path and
        // the resulting relative path becomes absolute. Canonicalize first.
        #[cfg(windows)]
        let canonical_buf: Option<PathBuf> = if path.starts_with(&self.base_path) {
            None
        } else if let Ok(c) = crate::path_utils::canonicalize(path) {
            Some(c)
        } else {
            tracing::error!(path = ?path.display(), "Failed to canonicalize file path to add");
            return Err(IndexUpdateError::Vanished);
        };

        #[cfg(windows)]
        let path_for_index: &Path = canonical_buf.as_deref().unwrap_or(path);
        #[cfg(not(windows))]
        let path_for_index: &Path = path;

        let (mut file_item, rel_path) =
            FileItem::new(path_for_index.to_path_buf(), &self.base_path, None);

        // we have to perform manual classification for every new file this will be
        // batched during the scan, this is the path when the file is ad-hoc added to the sync
        file_item.detect_binary_per_byte(
            path_for_index,
            // inline chunk buf
            &mut [0u8; crate::types::BINARY_CLASSIFICATION_CHUNK_SIZE],
        );

        // Keep the dir table consistent: register (or revive) the parent dir
        // so directory search reflects watcher-added files immediately.
        let dir_rel = crate::path_utils::to_canonical_slashes(
            &rel_path[..file_item.path.filename_offset as usize],
        );
        let parent_dir = self
            .update_dirs(|sync| sync.find_or_add_dir(&dir_rel))
            .ok_or(IndexUpdateError::CapacityExhausted)?;

        let builder = self.sync_data.overflow_builder.get_or_insert_with(|| {
            // we know that overflow would never create more files during the file
            crate::simd_path::ChunkedPathStoreBuilder::new(MAX_OVERFLOW_FILES)
        });
        file_item.set_path(builder.add_file_immediate(&rel_path, file_item.path.filename_offset));
        file_item.set_overflow(true);
        file_item.parent_dir_index = parent_dir;

        if !self.sync_data.files.push(file_item) {
            return Err(IndexUpdateError::CapacityExhausted);
        }

        self.sync_data.live_count += 1;
        // Dir may have been tombstoned by an earlier removal; a new file
        // under it proves it exists again.
        self.update_dirs(|sync| sync.revive_dir(parent_dir));
        self.sync_data
            .files
            .last()
            .ok_or(IndexUpdateError::CapacityExhausted)
    }

    pub(crate) fn handle_create_directory(&mut self, path: &Path) -> Result<(), IndexUpdateError> {
        let mut relative = self
            .to_relative_path(path)
            .ok_or(IndexUpdateError::OutsideIndex)?
            .into_owned();
        if !relative.is_empty() && !relative.ends_with('/') {
            relative.push('/');
        }
        self.update_dirs(|sync| {
            let index = sync.find_or_add_dir(&relative)?;
            sync.revive_dir(index);
            Some(())
        })
        .ok_or(IndexUpdateError::CapacityExhausted)?;
        Ok(())
    }

    fn tombstone_file(&mut self, index: usize) {
        let file = &mut self.sync_data.files[index];
        if file.is_deleted() {
            return;
        }

        file.set_deleted(true);
        file.invalidate_mmap(&self.cache_budget);
        file.git_status = None;

        // Only base-region files participate in the bigram overlay
        if index < self.sync_data.base_count
            && let Some(ref overlay) = self.sync_data.bigram_overlay
        {
            overlay.write().delete_file(index);
        }

        self.sync_data.live_count -= 1;
    }

    fn untombstone_file(&mut self, index: usize) {
        let file = &mut self.sync_data.files[index];
        if !file.is_deleted() {
            return;
        }
        file.set_deleted(false);
        let parent_dir = file.parent_dir_index;

        self.sync_data.live_count += 1;
        // The path exists on disk again, so its parent dir does too.
        self.update_dirs(|sync| sync.revive_dir(parent_dir));
    }

    /// Marks file as deleted, make sure that if you call this yourself these changes can be reverted
    /// by the internal mechanics if the file actually exists on the disk, use only if you know that
    /// the file going to be disapperaed or if you do not have the watcher installed
    pub fn remove_file_by_path(&mut self, path: impl AsRef<Path>) -> bool {
        let path = path.as_ref();
        if let Some(index) = self.sync_data.find_file_index(path, &self.base_path) {
            self.tombstone_file(index);
            true
        } else {
            false
        }
    }

    // TODO make this O(n)
    pub fn remove_all_files_in_dir(&mut self, dir: impl AsRef<Path>) -> usize {
        self.remove_all_files_in_dirs_inner(std::iter::once(dir.as_ref()), None)
    }

    /// Tombstones files under any of `dirs` in a single index scan.
    pub(crate) fn remove_all_files_in_dirs_with_callback<'a>(
        &mut self,
        dirs: impl IntoIterator<Item = &'a Path>,
        mut callback: impl FnMut(&Path),
    ) -> usize {
        self.remove_all_files_in_dirs_inner(dirs, Some(&mut callback))
    }

    pub(crate) fn remove_all_files_in_dirs<'a>(
        &mut self,
        dirs: impl IntoIterator<Item = &'a Path>,
    ) -> usize {
        self.remove_all_files_in_dirs_inner(dirs, None)
    }

    fn remove_all_files_in_dirs_inner<'a>(
        &mut self,
        dirs: impl IntoIterator<Item = &'a Path>,
        callback: Option<&mut dyn FnMut(&Path)>,
    ) -> usize {
        let prefixes = dirs
            .into_iter()
            .enumerate()
            .filter_map(|(owner, dir)| Some((self.dir_prefix(dir)?, owner)))
            .collect();
        self.tombstone_under(prefixes, |_, _| false, |_, _| false, callback)
    }

    // Tombstones what the index holds in each of `dirs`, them included, that the keep
    // predicates reject, in one pass. They get the position of the owning dir in `dirs` and
    // a '/'-canonical relative path, dirs ending with '/'. `dirs` must not nest.
    pub(crate) fn retain_in_dirs(
        &mut self,
        dirs: &[&Path],
        keep_file: impl Fn(usize, &str) -> bool,
        keep_dir: impl Fn(usize, &str) -> bool,
        mut on_removed: impl FnMut(&Path),
    ) -> usize {
        let prefixes = dirs
            .iter()
            .enumerate()
            .filter_map(|(owner, dir)| Some((self.dir_prefix(dir)?, owner)))
            .collect();
        self.tombstone_under(prefixes, keep_file, keep_dir, Some(&mut on_removed))
    }

    // Whether `dir` has a live entry, so the index may hold files below it.
    pub(crate) fn knows_dir(&self, dir: &Path) -> bool {
        match self.dir_prefix(dir) {
            Some(prefix) if prefix.is_empty() => true,
            Some(prefix) => self
                .sync_data
                .find_dir_index(&prefix)
                .and_then(|index| self.sync_data.dirs.get(index))
                .is_some_and(|dir| !dir.is_deleted()),
            None => false,
        }
    }

    // Identifies the snapshot the index was built from; every scan's commit changes it.
    pub(crate) fn sync_generation(&self) -> u64 {
        self.sync_generation
    }

    fn replace_sync(&mut self, sync: FileSync) -> FileSync {
        self.sync_generation = next_sync_generation();
        std::mem::replace(&mut self.sync_data, sync)
    }

    fn dir_prefix(&self, dir: &Path) -> Option<String> {
        let relative = self.to_relative_path(dir)?;
        // Stored relative paths are '/'-canonical on every platform.
        Some(if relative.is_empty() {
            String::new()
        } else {
            format!("{relative}/")
        })
    }

    fn tombstone_under(
        &mut self,
        mut prefixes: Vec<(String, usize)>,
        keep_file: impl Fn(usize, &str) -> bool,
        keep_dir: impl Fn(usize, &str) -> bool,
        mut callback: Option<&mut dyn FnMut(&Path)>,
    ) -> usize {
        if prefixes.is_empty() {
            return 0;
        }
        // Sorted and not nested, the greatest prefix not above a path is the only one
        // that can own it.
        prefixes.sort_unstable();
        prefixes.dedup_by(|nested, outer| nested.0.starts_with(outer.0.as_str()));
        let owner = |rel: &str| {
            let index = prefixes.partition_point(|(prefix, _)| prefix.as_str() <= rel);
            let (prefix, owner) = prefixes.get(index.checked_sub(1)?)?;
            rel.starts_with(prefix.as_str()).then_some(*owner)
        };

        let base_path = self.base_path.clone();
        let cache_budget = &self.cache_budget;
        let mut rel_buf = [0u8; crate::simd_path::PATH_BUF_SIZE];
        let mut path_buf = [0u8; crate::simd_path::PATH_BUF_SIZE];
        let tombstoned = self.sync_data.tombstone_files_with_arena(
            |file, arena| {
                let rel = file.path.read_to_buf(arena, &mut rel_buf);
                owner(rel).is_some_and(|owner| !keep_file(owner, rel))
            },
            |file, arena| {
                file.invalidate_mmap(cache_budget);
                if let Some(callback) = callback.as_mut() {
                    callback(file.write_absolute_path(arena, &base_path, &mut path_buf));
                }
            },
        );

        // Dirs follow their files out so directory search stops surfacing them.
        let mut dir_buf = [0u8; crate::simd_path::PATH_BUF_SIZE];
        self.update_dirs(|sync| {
            sync.tombstone_dirs_with_arena(|dir, arena| {
                let rel = dir.read_relative_path(arena, &mut dir_buf);
                owner(rel).is_some_and(|owner| !keep_dir(owner, rel))
            })
        });

        tombstoned
    }

    /// Use this to prevent any substantial background threads from acquiring the locks
    pub fn cancel(&self) {
        self.signals.cancelled.store(true, Ordering::Release);
    }

    /// Stop the background filesystem watcher. Non-blocking.
    pub fn stop_background_monitor(&mut self) {
        if let Some(mut watcher) = self.background_watcher.take() {
            watcher.stop();
        }
        self.signals.watcher_ready.store(false, Ordering::Release);
    }

    /// Quick way to check if scan is going without acquiring a lock for [Self::get_scan_progress]
    pub fn is_scan_active(&self) -> bool {
        self.signals.scanning.load(Ordering::Relaxed)
    }

    pub fn is_post_scan_active(&self) -> bool {
        self.signals
            .post_scan_indexing_active
            .load(Ordering::Acquire)
    }

    /// Return a clone of the watcher-ready flag so callers can poll it without
    /// holding a lock on the picker.
    pub fn watcher_signal(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.signals.watcher_ready)
    }

    /// Returns a lock-free view of the background scan and watcher progress.
    pub fn background_status(&self) -> crate::scan::BackgroundStatus {
        crate::scan::BackgroundStatus::new(
            self.signals.clone(),
            self.watch,
            Arc::clone(&self.scanned_files_count),
        )
    }

    /// Convert an absolute path to a relative path string (relative to base_path).
    /// Returns None if the path doesn't start with base_path.
    ///
    /// On Windows the picker canonicalizes its base via `dunce`, so caller
    /// paths that still carry 8.3 short names or a different casing would
    /// fail a naive prefix check. Fall back to canonicalizing (or, when the
    /// file was just deleted, canonicalizing its parent) before stripping.
    fn to_relative_path<'a>(&self, path: &'a Path) -> Option<std::borrow::Cow<'a, str>> {
        if let Ok(stripped) = path.strip_prefix(&self.base_path)
            && let Some(s) = stripped.to_str()
        {
            // Callers compare against '/'-canonical stored paths.
            return Some(crate::path_utils::to_canonical_slashes(s));
        }

        #[cfg(windows)]
        {
            let rel = canonical_relative_path(path, &self.base_path)?;
            return Some(std::borrow::Cow::Owned(rel));
        }

        #[cfg(not(windows))]
        None
    }
}

/// Resolve a possibly-short-name Windows path to the picker's canonical base.
/// Used by the Windows-only fallbacks in `to_relative_path` and
/// `find_file_index` so events still match tombstoned entries.
#[cfg(windows)]
fn canonical_relative_path(path: &Path, base: &Path) -> Option<String> {
    if let Ok(canonical) = crate::path_utils::canonicalize(path)
        && let Ok(stripped) = canonical.strip_prefix(base)
        && let Some(s) = stripped.to_str()
    {
        return Some(crate::path_utils::to_canonical_slashes(s).into_owned());
    }

    // Deleted files can't be canonicalized — canonicalize the parent and
    // re-attach the filename.
    let parent = path.parent()?;
    let file_name = path.file_name()?;
    let canonical_parent = crate::path_utils::canonicalize(parent).ok()?;
    let stripped_parent = canonical_parent.strip_prefix(base).ok()?;
    let mut rel = stripped_parent.to_path_buf();
    rel.push(file_name);
    rel.to_str()
        .map(|s| crate::path_utils::to_canonical_slashes(s).into_owned())
}

impl Drop for FilePicker {
    fn drop(&mut self) {
        // Cancel any in-flight ScanJob bound to this picker's signals so
        // it cannot mutate the replacement picker after a swap.
        self.signals.cancelled.store(true, Ordering::Release);
        // Wake the git-status consumer so it exits; never joined (it takes
        // the picker write lock, a blocking join here could deadlock).
        self.git_status_worker.signal_shutdown();
    }
}

/// Why an incremental index update did not land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IndexUpdateError {
    /// The path no longer exists; its entry, if any, is tombstoned.
    Vanished,
    /// The path is not under the index root.
    OutsideIndex,
    /// The watcher-appended region is full; only a rescan can absorb the change.
    CapacityExhausted,
}

#[derive(Debug, Clone, Copy)]
enum FileSlot {
    Base(usize),
    Overflow(usize),
}

impl FileSlot {
    fn index(self) -> usize {
        match self {
            FileSlot::Base(i) | FileSlot::Overflow(i) => i,
        }
    }
}

/// Snapshot of FilePicker state for off-lock post-scan work.
///
/// Each data field is an Arc-shared clone of the picker's backing
/// allocation, so dropping the `FilePicker` (e.g. via
/// `SharedFilePicker::write().take()`) cannot free memory this
/// snapshot is still reading — UAF is impossible by construction.
///
/// Implements `Drop` to clear `post_scan_indexing_active`. Since only
/// one snapshot can exist at a time (enforced by the flag check in
/// `post_scan_snapshot`) and it is always created/dropped within
/// `ScanJob::run`, `scan_job_running == false` implies no live snapshot.
pub(crate) struct PostScanUnsafeSnapshot {
    pub files: StableVec<FileItem>,
    pub arena: Option<Arc<crate::simd_path::ChunkedPathStore>>,
    // TODO figure this out
    pub _budget: Arc<crate::types::ContentCacheBudget>,
    pub base_count: usize,
    pub indexable_count: usize,
    pub base_path: PathBuf,
    post_scan_flag: Arc<AtomicBool>,
}

impl Drop for PostScanUnsafeSnapshot {
    fn drop(&mut self) {
        self.post_scan_flag.store(false, Ordering::Release);
    }
}

// SAFETY: every data field is Arc-shared and outlives the snapshot
// via its own refcount. The mutable cast in `apply_git_status_and_frecency`
// is consumed on the scan thread under the single-writer discipline.
unsafe impl Send for PostScanUnsafeSnapshot {}
unsafe impl Sync for PostScanUnsafeSnapshot {}

/// A point-in-time snapshot of the file-scanning progress.
///
/// Returned by [`FilePicker::get_scan_progress`]. Useful for displaying
/// a progress indicator while the initial scan is running.
#[derive(Debug, Clone)]
pub struct ScanProgress {
    pub scanned_files_count: usize,
    pub is_scanning: bool,
    pub is_watcher_ready: bool,
    pub is_warmup_complete: bool,
}

impl FileSync {
    pub(crate) fn discover_git(base_path: &Path, disabled: bool) -> Option<GitDiscovery> {
        if disabled {
            return None;
        }
        let git = GitDiscovery::discover(base_path);
        match &git {
            Some(git) => {
                debug!(workdir = ?git.workdir, git_dir = ?git.git_dir, "Git repository found")
            }
            None => warn!("No git repository found for path: {}", base_path.display()),
        }

        git
    }

    pub(crate) fn spawn_git_status(git_workdir: PathBuf) -> JoinHandle<Option<GitStatusCache>> {
        std::thread::spawn(move || {
            GitStatusCache::read_git_status(
                Some(git_workdir.as_path()),
                &mut crate::git::initial_scan_status_options(),
            )
        })
    }

    /// Returns files immediately (searchable) and a handle to the in-progress
    /// git status computation. This avoids blocking on `git status` which can
    /// take 10+ seconds on very large repos (e.g. chromium).
    #[tracing::instrument(skip_all, name = "walk_filesystem", level = Level::INFO)]
    pub(crate) fn walk_filesystem(
        base_path: &Path,
        git: Option<GitDiscovery>,
        synced_files_count: &Arc<AtomicUsize>,
        shared_frecency: &SharedFrecency,
        mode: FFFMode,
        policy: crate::walk::WalkPolicy<'_>,
        on_dir: Option<&crate::walk::DirHook>,
    ) -> Result<FileSync, Error> {
        let scan_start = std::time::Instant::now();
        info!("SCAN: Starting filesystem walk and git status (async)");

        // Walk files (the fast part, typically 2-3s even on huge repos).
        let is_git_repo = git.is_some();
        let shared_base = git.as_ref().and_then(|git| git.shared_base(base_path));
        if let Some(shared_base) = &shared_base {
            info!(
                ?shared_base,
                "Linked git worktree, sharing the main checkout history"
            );
        }
        let bg_threads = BACKGROUND_THREAD_POOL.current_num_threads();

        let WalkOutput {
            dirs: mut walked_dirs,
            mut pairs,
            ignore_rules,
            ..
        } = crate::walk::walk_collect_files(
            base_path,
            None,
            is_git_repo,
            policy,
            bg_threads,
            synced_files_count,
            on_dir,
        )?;
        let ignore_rules = ignore_rules.map(Arc::new);

        // group walked dirs and files with a dir part to the same order
        BACKGROUND_THREAD_POOL.install(|| {
            rayon::join(
                || {
                    pairs.par_sort_unstable_by(|(a, path_a), (b, path_b)| {
                        // SAFETY: `filename_offset` is always at a character boundary
                        let (a_dir, a_file) = path_a.split_at(a.path.filename_offset as usize);
                        let (b_dir, b_file) = path_b.split_at(b.path.filename_offset as usize);
                        a_dir.cmp(b_dir).then_with(|| a_file.cmp(b_file))
                    });
                },
                || walked_dirs.par_sort_unstable(),
            );
        });
        walked_dirs.dedup();

        let mut builder = crate::simd_path::ChunkedPathStoreBuilder::new(pairs.len());
        let dirs = populates_dirs_files_chunked_storage(&mut pairs, &walked_dirs, &mut builder);
        drop(walked_dirs);

        let mut files: Vec<FileItem> = pairs.into_iter().map(|(file, _)| file).collect();
        let chunked_paths = builder.finish();
        let arena = chunked_paths.as_arena_ptr();

        // Apply frecency scores (access-based only — git status not yet available).
        // DirItem.max_access_frecency is AtomicI32, so parallel threads write directly.
        let frecency = shared_frecency
            .read()
            .map_err(|_| Error::AcquireFrecencyLock)?;

        if let Some(frecency) = frecency.as_ref() {
            let dirs_ref = &dirs;
            let shared_base = shared_base.as_deref();
            BACKGROUND_THREAD_POOL.install(|| {
                files.par_iter_mut().for_each_init(
                    || frecency.reader(),
                    |reader, file| {
                        file.update_frecency_scores(reader, arena, base_path, shared_base, mode);
                        let score = file.access_frecency_score as i32;
                        if score > 0 {
                            let dir_idx = file.parent_dir_index as usize;
                            if let Some(dir) = dirs_ref.get(dir_idx) {
                                dir.update_frecency_if_larger(score);
                            }
                        }
                    },
                );
            });
        }
        drop(frecency);

        // un-indexable files that are binary or not fitting the size cap has to beplaced in the end
        let is_indexable = |f: &FileItem| {
            !f.is_binary()
                && f.size > 0
                && f.size <= crate::constants::MAX_INDEXABLE_FILE_SIZE as u64
        };

        BACKGROUND_THREAD_POOL.install(|| {
            files.par_sort_unstable_by(|a, b| {
                (!is_indexable(a))
                    .cmp(&!is_indexable(b))
                    // this just makes it faster in terms of allocation - we store the dir indexes
                    .then_with(|| a.parent_dir_index.cmp(&b.parent_dir_index))
                    .then_with(|| a.file_name(arena).cmp(&b.file_name(arena)))
            });
        });
        let indexable_count = files.partition_point(is_indexable);

        // Ask the allocator to return freed pages to the OS.
        hint_allocator_collect();

        let file_item_size = std::mem::size_of::<FileItem>();
        let files_vec_bytes = files.len() * file_item_size;
        let dir_table_bytes = dirs.len() * std::mem::size_of::<DirItem>()
            + dirs
                .iter()
                .map(|d| d.relative_path(arena).len())
                .sum::<usize>();

        let total_time = scan_start.elapsed();
        info!(
            "SCAN: Walk completed in {:?} ({} files, {} dirs, \
         chunked_store={:.2}MB, files_vec={:.2}MB, dirs={:.2}MB, FileItem={}B)",
            total_time,
            files.len(),
            dirs.len(),
            chunked_paths.heap_bytes() as f64 / 1_048_576.0,
            files_vec_bytes as f64 / 1_048_576.0,
            dir_table_bytes as f64 / 1_048_576.0,
            file_item_size,
        );

        let base_count = files.len();
        let base_dirs_count = dirs.len();

        Ok(FileSync {
            files: StableVec::from_vec_with_reserve(files, MAX_OVERFLOW_FILES),
            indexable_count,
            base_count,
            live_count: base_count,
            dirs: StableVec::from_vec_with_reserve(dirs, MAX_OVERFLOW_FILES),
            base_dirs_count,
            live_dirs_count: base_dirs_count,
            overflow_builder: None,
            git,
            shared_base,
            bigram_index: None,
            bigram_overlay: None,
            chunked_paths: Some(Arc::new(chunked_paths)),
            ignore_rules,
            scopes: ScopeCache::default(),
        })
    }
}

/// Pre-populate mmap caches for cold tail files so the first grep search
/// doesn't pay the mmap creation + page fault cost.
#[allow(dead_code)]
#[tracing::instrument(skip(files), name = "warmup_mmaps", level = Level::DEBUG)]
pub(crate) fn warmup_mmaps(
    files: &[FileItem],
    budget: &ContentCacheBudget,
    base_path: &Path,
    arena: ArenaPtr,
) {
    // for most of the use cases mmaps limit would be significantly smaller than arepo
    for file in files.iter() {
        if file.is_likely_hot()
            || file.is_binary()
            || file.size == 0
            || file.size > budget.max_file_size
        {
            continue;
        }

        let _ = file.get_cached_content(arena, base_path, budget);

        if budget.is_exhausted() {
            break;
        }
    }
}

/// This does both thing (yes sorry all the OOP morons)
/// in one go: populates files chunked storage and builds the dir table from
/// `walked_dirs` (every dir the walker visited: sorted, '/'-terminated,
/// deduped), merging file parents in a single lockstep sweep so dirs with no
/// files (empty subtrees, pure ancestors) are indexed and searchable too.
fn populates_dirs_files_chunked_storage<'a>(
    pairs: &'a mut [(FileItem, String)],
    walked_dirs: &[String],
    chunk_storage: &mut crate::simd_path::ChunkedPathStoreBuilder,
) -> Vec<DirItem> {
    let mut dirs: Vec<DirItem> = Vec::with_capacity(walked_dirs.len() + 1);
    let mut dir_iter = walked_dirs.iter().peekable();

    // Root-level files sort first and their "" parent is never a walker dir.
    if pairs
        .first()
        .is_some_and(|(f, _)| f.path.filename_offset == 0)
    {
        push_dir_item(&mut dirs, chunk_storage, "");
    }

    // Detects contiguous same-dir runs (pairs are sorted by dir) so the
    // merge below runs once per directory, not once per file.
    let mut prev_dir: &'a str = "";
    let mut current_dir_idx: u32 = 0;

    for (file, rel) in pairs.iter_mut() {
        let rel: &'a str = rel;
        let dir_part: &'a str = &rel[..file.path.filename_offset as usize];

        if prev_dir != dir_part {
            // Flush walked dirs up to and including this file's parent,
            // keeping the table sorted for the find_dir_index binary search.
            while let Some(dir) = dir_iter.peek()
                && dir.as_str() < dir_part
            {
                push_dir_item(&mut dirs, chunk_storage, dir);
                dir_iter.next();
            }

            match dir_iter.peek() {
                Some(dir) if dir.as_str() == dir_part => {
                    push_dir_item(&mut dirs, chunk_storage, dir);
                    dir_iter.next();
                }
                // Parents the walker reported with a non-dir kind
                // (e.g. followed symlinks) aren't in the list.
                _ => push_dir_item(&mut dirs, chunk_storage, dir_part),
            }

            current_dir_idx = (dirs.len() - 1) as u32;
            prev_dir = dir_part;
        }

        file.path = chunk_storage.add_file_immediate(rel, file.path.filename_offset);
        file.parent_dir_index = current_dir_idx;
    }

    for dir in dir_iter {
        push_dir_item(&mut dirs, chunk_storage, dir);
    }

    dirs
}

fn push_dir_item(
    dirs: &mut Vec<DirItem>,
    chunk_storage: &mut crate::simd_path::ChunkedPathStoreBuilder,
    dir_part: &str,
) {
    let dir_string = chunk_storage.add_dir_immediate(dir_part);

    // Compute last-segment offset: for "src/components/" -> 4 (points to "components/")
    let last_seg = if dir_part.is_empty() {
        0
    } else {
        let trimmed = dir_part.trim_end_matches(std::path::is_separator);
        trimmed
            .rfind(std::path::is_separator)
            .map(|i| i + 1)
            .unwrap_or(0) as u16
    };

    dirs.push(DirItem::new(dir_string, last_seg));
}

/// Fast extension-based binary detection. Avoids opening files during scan.
/// Covers the vast majority of binary files in typical repositories.
#[inline]
#[doc(hidden)]
pub fn is_known_binary_extension(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    is_binary_extension_str(ext)
}

// Extension check, then a content sniff for extensions shared by text and binary formats.
pub(crate) fn is_binary_file(path: &Path) -> bool {
    is_known_binary_extension(path)
        && (!path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_ambiguous_binary_name)
            || starts_with_binary_content(path))
}

// `.blp` names both GTK Blueprint text and a binary texture format.
#[inline]
pub(crate) fn is_ambiguous_binary_name(name: &str) -> bool {
    let name = name.as_bytes();
    name.len() > 4 && name[name.len() - 4..].eq_ignore_ascii_case(b".blp")
}

pub(crate) fn starts_with_binary_content(path: &Path) -> bool {
    use std::io::Read;
    let mut chunk = [0u8; 4096];
    std::fs::File::open(path)
        .and_then(|mut file| file.read(&mut chunk))
        .is_ok_and(|read| crate::types::detect_binary_content(&chunk[..read]))
}

/// Like [`is_known_binary_extension`] but takes a basename string directly,
/// avoiding `Path::extension()` overhead. Mirrors `Path::extension()`
/// semantics: dotfiles with no other dots → no extension. Used by the zlob
/// walker, which already has the basename slice from traversal.
#[cfg(feature = "zlob")]
#[inline]
pub(crate) fn is_known_binary_extension_basename(name: &str) -> bool {
    match name.rfind('.') {
        Some(pos) if pos > 0 && pos < name.len() - 1 => is_binary_extension_str(&name[pos + 1..]),
        _ => false,
    }
}

#[inline]
fn is_binary_extension_str(ext: &str) -> bool {
    matches!(
        ext,
        // Images
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "ico" | "webp" | "tiff" | "tif" | "avif" |
        "heic" | "heif" | "jxl" | "jp2" | "j2k" | "psd" | "icns" | "cur" | "cr2" |
        "nef" | "dng" | "tga" |
        // GPU / VFX texture formats
        "rgbe" | "hdr" | "exr" | "dds" | "ktx" | "ktx2" | "pvr" | "astc" |
        // Adobe Illustrator (PDF wrapper) / Apple webarchive / MIME HTML archive
        "ai" | "webarchive" | "mhtml" |
        // Video/Audio
        "mp4" | "avi" | "mov" | "wmv" | "mkv" | "mp3" | "wav" | "flac" | "ogg" | "m4a" |
        "aac" | "webm" | "flv" | "mpg" | "mpeg" | "wma" | "opus" | "pcm" | "reapeaks" |
        // Compressed/Archives
        "zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "rar" | "zst" | "lz4" | "lzma" |
        "cab" | "cpio" | "jsonlz4" |
        // Packages/Installers
        "deb" | "rpm" | "apk" | "dmg" | "msi" | "iso" | "nupkg" | "whl" | "egg" |
        "appimage" | "flatpak" | "crx" | "pak" |
        // Executables/Libraries
        "exe" | "dll" | "so" | "dylib" | "o" | "a" | "lib" | "bin" | "elf" |
        // Documents (binary office formats)
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" |
        // Databases
        "db" | "sqlite" | "sqlite3" | "mdb" |
        // SQLite / LevelDB auxiliary files
        "sqlite-wal" | "sqlite-shm" | "sqlite3-wal" | "sqlite3-shm" |
        "db-wal" | "db-shm" | "ldb" |
        // Fonts
        "ttf" | "otf" | "woff" | "woff2" | "eot" |
        // Compiled/Runtime
        "class" | "pyc" | "pyo" | "wasm" | "dex" | "jar" | "war" |
        // OCaml / Swift / Objective-C build artefacts
        "cmi" | "cmt" | "cmti" | "cmx" | "nib" |
        "swiftdeps" | "swiftdeps~" | "swiftdoc" | "swiftmodule" | "swiftsourceinfo" |
        // ML/Data Science
        "npy" | "npz" | "h5" | "hdf5" | "pt" | "onnx" |
        "safetensors" | "tfrecord" | "tflite" | "gguf" | "ggml" | "joblib" |
        // 3D/Game assets (indexing also sniffs `.blp`, see `is_ambiguous_binary_name`)
        "glb" | "blend" | "blp" |
        // Gzipped-XML / binary maps
        "dia" | "bcmap" |
        // Protobuf wire format
        "pb" |
        // Data/serialized
        "parquet" | "arrow" |
        // IDE/OS metadata
        "suo"
    )
}

/// Keep mimalloc off 2 MiB huge pages: with THP the arena is resident at
/// 2 MiB granularity and idle index memory inflates RSS by ~2x. Env overrides win.
/// Must run before the first allocation (see `fff_nvim`'s init-array hook).
#[cfg(feature = "mimalloc-collect")]
pub extern "C" fn tune_mimalloc() {
    // SAFETY: getenv/mi_option_set touch static tables only; no allocation happens here.
    unsafe {
        let user_set = !libc::getenv(c"MIMALLOC_ALLOW_LARGE_OS_PAGES".as_ptr()).is_null()
            || !libc::getenv(c"MIMALLOC_LARGE_OS_PAGES".as_ptr()).is_null();
        if !user_set {
            libmimalloc_sys::mi_option_set(libmimalloc_sys::mi_option_large_os_pages, 0);
        }
    }
}

/// Ask the global allocator to return freed pages to the OS.
/// Enabled via the `mimalloc-collect` feature (set by fff-nvim).
/// No-op when the feature is off (tests, system allocator).
pub(crate) fn hint_allocator_collect() {
    #[cfg(feature = "mimalloc-collect")]
    {
        // Collect BACKGROUND_THREAD_POOL workers — that's where the bigram
        // builder allocated memory. `rayon::broadcast` would target the global
        // pool, which is the wrong set of threads.
        BACKGROUND_THREAD_POOL.broadcast(|_| unsafe { libmimalloc_sys::mi_collect(true) });

        // Main thread too.
        unsafe { libmimalloc_sys::mi_collect(true) };
    }
}

// Unique across pickers, so a picker that replaced another never repeats its value.
fn next_sync_generation() -> u64 {
    static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    GENERATION.fetch_add(1, Ordering::Relaxed) + 1
}

fn new_bigram_overlay(indexable_count: usize) -> Arc<parking_lot::RwLock<BigramOverlay>> {
    Arc::new(parking_lot::RwLock::new(BigramOverlay::new(
        indexable_count,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blueprint_text_is_searchable_while_blp_textures_stay_binary() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("window.blp"),
            "template $Window : Gtk.Window {\n  tooltip-text: \"Needle\";\n}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("texture.blp"), b"BLP2\0\0\0Needle\0").unwrap();
        let mut picker = FilePicker::new(FilePickerOptions {
            base_path: dir.path().to_string_lossy().into_owned(),
            enable_content_indexing: false,
            enable_mmap_cache: false,
            watch: false,
            ..Default::default()
        })
        .unwrap();
        picker.collect_files().unwrap();
        let query =
            fff_query_parser::QueryParser::new(fff_query_parser::AiGrepConfig).parse("Needle");
        let options = crate::grep::GrepSearchOptions {
            time_budget_ms: 0,
            ..Default::default()
        };
        let result = picker.grep(&query, &options);
        let files: Vec<_> = result
            .files
            .iter()
            .map(|f| f.relative_path(&picker))
            .collect();
        assert_eq!(files, ["window.blp"]);

        // The public extension helper keeps reporting `.blp` as binary.
        assert!(is_known_binary_extension(Path::new("window.blp")));
        for (name, binary) in [("texture.blp", true), ("window.blp", false)] {
            let (item, _) = crate::types::FileItem::new(dir.path().join(name), dir.path(), None);
            assert_eq!(item.is_binary(), binary, "{name}");
        }
    }

    fn indexed_picker(files: &[&str]) -> (tempfile::TempDir, FilePicker) {
        let dir = tempfile::tempdir().unwrap();
        let base = crate::path_utils::canonicalize(dir.path()).unwrap();
        for file in files {
            let path = base.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "x").unwrap();
        }
        let mut picker = FilePicker::new(FilePickerOptions {
            base_path: base.to_string_lossy().into_owned(),
            watch: false,
            ..Default::default()
        })
        .unwrap();
        picker.collect_files().unwrap();
        (dir, picker)
    }

    fn overflow_chunks(picker: &FilePicker) -> usize {
        picker
            .sync_data
            .overflow_builder
            .as_ref()
            .map_or(0, |builder| builder.written_chunks())
    }

    #[test]
    fn full_overflow_rejects_new_files_without_writing_paths() {
        let (_dir, mut picker) = indexed_picker(&["kept.rs"]);
        let base = picker.base_path().to_path_buf();
        assert!(picker.sync_data.files.remaining_capacity() >= MAX_OVERFLOW_FILES);
        for i in 0..MAX_OVERFLOW_FILES {
            let path = base.join(format!("added{i}.rs"));
            std::fs::write(&path, "x").unwrap();
            assert!(picker.apply_create_or_modify(&path).is_ok());
        }

        let written = overflow_chunks(&picker);
        let extra = base.join("extra.rs");
        std::fs::write(&extra, "x").unwrap();
        assert_eq!(
            picker.apply_create_or_modify(&extra).err(),
            Some(IndexUpdateError::CapacityExhausted)
        );
        assert_eq!(overflow_chunks(&picker), written);
        assert!(picker.get_file_by_path(&extra).is_none());
    }

    #[test]
    fn full_dir_table_rejects_files_in_new_dirs_without_writing_paths() {
        let (_dir, mut picker) = indexed_picker(&["kept.rs"]);
        let base = picker.base_path().to_path_buf();
        for i in 0..picker.sync_data.dirs.remaining_capacity() {
            let dir = base.join(format!("dir{i}"));
            std::fs::create_dir(&dir).unwrap();
            assert_eq!(picker.handle_create_directory(&dir), Ok(()));
        }

        let written = overflow_chunks(&picker);
        let file = base.join("late/file.rs");
        std::fs::create_dir(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "x").unwrap();
        assert_eq!(
            picker.apply_create_or_modify(&file).err(),
            Some(IndexUpdateError::CapacityExhausted)
        );
        assert_eq!(overflow_chunks(&picker), written);
        assert_eq!(picker.get_overflow_files().len(), 0);
        // Existing dirs still accept files.
        let kept = base.join("dir0/file.rs");
        std::fs::write(&kept, "x").unwrap();
        assert!(picker.apply_create_or_modify(&kept).is_ok());
    }

    #[test]
    fn vanished_files_are_tombstoned_not_rejected() {
        let (_dir, mut picker) = indexed_picker(&["gone.rs", "kept.rs"]);
        let gone = picker.base_path().join("gone.rs");
        std::fs::remove_file(&gone).unwrap();
        assert_eq!(
            picker.apply_create_or_modify(&gone).err(),
            Some(IndexUpdateError::Vanished)
        );
        assert!(picker.get_file_by_path(&gone).unwrap().is_deleted());
        assert_eq!(picker.live_file_count(), 1);
    }

    #[test]
    fn refuses_home_dir_with_trailing_separator() {
        let Some(home) = dirs::home_dir() else { return };
        for base in [home.clone(), home.join("")] {
            let result = FilePicker::new(FilePickerOptions {
                base_path: base.to_string_lossy().into_owned(),
                watch: false,
                ..Default::default()
            });
            assert!(
                matches!(result, Err(Error::FilesystemRoot(_))),
                "home dir accepted as {}",
                base.display()
            );
        }
    }

    #[test]
    fn dir_table_merges_walked_dirs_with_file_parents() {
        let mut pairs: Vec<(FileItem, String)> = ["src/main.rs", "src/deep/lib.rs", "root.txt"]
            .iter()
            .map(|p| {
                let (item, rel) = FileItem::new(PathBuf::from(p), Path::new(""), None);
                (item, rel)
            })
            .collect();
        pairs.sort_by(|(a, pa), (b, pb)| {
            pa[..a.path.filename_offset as usize]
                .cmp(&pb[..b.path.filename_offset as usize])
                .then_with(|| pa.cmp(pb))
        });

        // Sorted '/'-terminated walker output: file parents + an empty dir +
        // a sibling sharing a prefix with a file parent.
        let walked: Vec<String> = ["empty/", "src/", "src/deep/", "src/deeper/"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let mut builder = crate::simd_path::ChunkedPathStoreBuilder::new(pairs.len());
        let dirs = populates_dirs_files_chunked_storage(&mut pairs, &walked, &mut builder);
        let store = builder.finish();
        let arena = store.as_arena_ptr();

        let table: Vec<String> = dirs.iter().map(|d| d.relative_path(arena)).collect();
        // Sorted: "" (root files) first, all walked dirs present exactly once.
        assert_eq!(table, ["", "empty/", "src/", "src/deep/", "src/deeper/"]);

        // Every file's parent_dir_index points at its own dir entry.
        for (file, _) in &pairs {
            let dir = &dirs[file.parent_dir_index as usize];
            let rel = file.relative_path(arena);
            assert!(
                rel.starts_with(&dir.relative_path(arena)),
                "file {rel} must live under its parent dir",
            );
        }
    }

    #[test]
    fn directory_removal_collects_each_tombstoned_path() {
        let dir = tempfile::tempdir().unwrap();
        let base = crate::path_utils::canonicalize(dir.path()).unwrap();
        let removed_dir = base.join("removed");
        let kept = base.join("kept.txt");
        let first = removed_dir.join("a.txt");
        let second = removed_dir.join("nested/b.txt");
        std::fs::create_dir_all(second.parent().unwrap()).unwrap();
        std::fs::write(&first, b"a").unwrap();
        std::fs::write(&second, b"b").unwrap();
        std::fs::write(&kept, b"kept").unwrap();

        let mut picker = FilePicker::new(FilePickerOptions {
            base_path: base.to_string_lossy().into_owned(),
            watch: false,
            ..Default::default()
        })
        .unwrap();
        picker.collect_files().unwrap();

        let mut removed = Vec::new();
        assert_eq!(
            picker.remove_all_files_in_dirs_with_callback(
                std::iter::once(removed_dir.as_path()),
                |path| {
                    removed.push(path.to_path_buf());
                }
            ),
            2
        );
        removed.sort_unstable();
        assert_eq!(removed, vec![first, second]);
        assert!(picker.get_file_by_path(&kept).is_some());

        let outside = base.parent().unwrap().join("outside");
        assert_eq!(picker.remove_all_files_in_dir(&outside), 0);
        assert!(picker.get_file_by_path(&kept).is_some());
    }

    #[test]
    fn coverage_listeners_hear_directory_set_changes() {
        use std::sync::atomic::AtomicUsize;

        let dir = tempfile::tempdir().unwrap();
        let base = crate::path_utils::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(base.join("kept")).unwrap();
        std::fs::create_dir_all(base.join("gone")).unwrap();
        std::fs::write(base.join("kept/a.txt"), "a").unwrap();
        std::fs::write(base.join("gone/b.txt"), "b").unwrap();

        let shared = SharedFilePicker::default();
        let heard = Arc::new(AtomicUsize::new(0));
        let listener = Arc::clone(&heard);
        let subscription = shared.subscribe_coverage(move || {
            listener.fetch_add(1, Ordering::SeqCst);
        });
        FilePicker::new_with_shared_state(
            shared.clone(),
            SharedFrecency::default(),
            FilePickerOptions {
                base_path: base.to_string_lossy().into_owned(),
                watch: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(shared.wait_for_scan(std::time::Duration::from_secs(10)));
        let scope = |rel: &str| crate::scope::SearchScope::new(&base, &base.join(rel)).unwrap();
        let mut guard = shared.write().unwrap();
        let picker = guard.as_mut().unwrap();
        assert!(picker.contains_scope(&scope("gone")));
        let mut expected = heard.load(Ordering::SeqCst);
        assert!(expected >= 2, "publish and scan commit: {expected}");
        let mut assert_heard = |changed: bool| {
            if changed {
                expected += 1;
            }
            assert_eq!(heard.load(Ordering::SeqCst), expected);
        };

        std::fs::write(base.join("kept/a.txt"), "changed").unwrap();
        assert!(
            picker
                .handle_create_or_modify(base.join("kept/a.txt"))
                .is_some()
        );
        assert_heard(false);

        std::fs::create_dir_all(base.join("new/empty")).unwrap();
        picker
            .handle_create_directory(&base.join("new/empty"))
            .unwrap();
        assert!(picker.contains_scope(&scope("new/empty")));
        assert_heard(true);
        picker
            .handle_create_directory(&base.join("new/empty"))
            .unwrap();
        assert_heard(false);

        std::fs::create_dir_all(base.join("fresh")).unwrap();
        std::fs::write(base.join("fresh/c.txt"), "c").unwrap();
        assert!(
            picker
                .handle_create_or_modify(base.join("fresh/c.txt"))
                .is_some()
        );
        assert_heard(true);

        assert_eq!(picker.remove_all_files_in_dir(base.join("gone")), 1);
        assert!(!picker.contains_scope(&scope("gone")));
        assert_heard(true);
        assert_eq!(picker.remove_all_files_in_dir(base.join("gone")), 0);
        assert_heard(false);

        drop(subscription);
        std::fs::create_dir_all(base.join("unheard")).unwrap();
        picker
            .handle_create_directory(&base.join("unheard"))
            .unwrap();
        assert_heard(false);
    }
}
