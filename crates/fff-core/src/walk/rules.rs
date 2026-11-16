use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use parking_lot::Mutex;

use super::WalkIgnoreRules;
use crate::file_picker::FilePickerIgnoreOptions;

// The coarsest file system timestamp, FAT's: a write may be dated up to this much off.
const TIMESTAMP_GRANULARITY: Duration = Duration::from_secs(2);

// A walk's rules under those of subtrees walked again since, the deepest deciding, as Git's
// untracked cache invalidates only the subtree of a changed `.gitignore`.
#[derive(Clone, Default, Debug)]
pub(crate) struct IgnoreRules {
    base: Option<Arc<WalkIgnoreRules>>,
    // Base-relative directories, none below another.
    subtrees: BTreeMap<PathBuf, Option<Arc<WalkIgnoreRules>>>,
}

impl IgnoreRules {
    pub(crate) fn new(base: Option<WalkIgnoreRules>) -> Self {
        Self {
            base: base.map(Arc::new),
            subtrees: BTreeMap::new(),
        }
    }

    // A walk of `relative` read every ignore file that decides for the paths below it.
    pub(crate) fn replace_subtree(&mut self, relative: &Path, rules: Option<WalkIgnoreRules>) {
        if relative.as_os_str().is_empty() {
            *self = Self::new(rules);
            return;
        }
        let below: Vec<PathBuf> = self.layers_within(relative).cloned().collect();
        for dir in below {
            self.subtrees.remove(&dir);
        }
        (self.subtrees).insert(relative.to_path_buf(), rules.map(Arc::new));
    }

    // Whether rules of a walk at or below `relative` lie over the base's.
    pub(crate) fn has_layer_within(&self, relative: &Path) -> bool {
        self.layers_within(relative).next().is_some()
    }

    // `None` when the rules that decide for the path are not known, as libgit2 then decides.
    pub(crate) fn is_ignored(&mut self, relative: &Path, is_dir: bool) -> Option<bool> {
        let deepest = (relative.ancestors())
            .find(|dir| self.subtrees.contains_key(*dir))
            .map(Path::to_path_buf);
        let layer = match deepest {
            Some(dir) => self.subtrees.get_mut(&dir).and_then(Option::as_mut),
            None => self.base.as_mut(),
        }?;
        #[cfg(feature = "zlob")]
        return Some(layer.is_ignored(relative, is_dir));
        // Ripgrep caches directory rules: each batch starts from the unloaded matcher.
        #[cfg(not(feature = "zlob"))]
        return Some(Arc::make_mut(layer).is_ignored(relative, is_dir));
    }

    // Descendants sort right after their ancestor, components compared in order.
    fn layers_within<'a>(&'a self, relative: &'a Path) -> impl Iterator<Item = &'a PathBuf> {
        (self.subtrees)
            .range::<Path, _>((Bound::Included(relative), Bound::Unbounded))
            .map(|(dir, _)| dir)
            .take_while(move |dir| dir.starts_with(relative))
    }
}

// What an ignore file held when a walk read its rules, and when that walk ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IgnoreSource {
    // `None` when the file changed while the walk read it.
    hash: Option<u64>,
    walk: (SystemTime, SystemTime),
}

impl IgnoreSource {
    // Whether the file at `path` may now give other rules than `recorded` did: its content
    // decides, not its metadata, as Git's untracked cache compares a `.gitignore` by hash.
    pub(crate) fn changed(recorded: Option<Self>, path: &Path) -> bool {
        let (Some(recorded), Some(content)) = (recorded, crate::stat::read_regular(path)) else {
            // Unknown or gone: rules may have changed.
            return true;
        };
        let modified = std::fs::metadata(path).and_then(|meta| meta.modified());
        recorded.hash != Some(hash(&content)) || modified.ok().is_none_or(|at| recorded.racy(at))
    }

    // Git's racy rule at the coarsest granularity: a write dated within the walk may have
    // come between the read of its hash and the walker's, and been undone since.
    fn racy(&self, modified: SystemTime) -> bool {
        let (started, ended) = self.walk;
        modified + TIMESTAMP_GRANULARITY >= started && modified <= ended + TIMESTAMP_GRANULARITY
    }
}

// Reads the ignore files of each directory a walk enters just before the walker does, on the
// walker's own thread, so the rules it reads are never older than the hash.
pub(crate) struct IgnoreSources {
    names: Vec<&'static str>,
    started: SystemTime,
    read: Mutex<Vec<(PathBuf, u64)>>,
}

impl IgnoreSources {
    pub(crate) fn new(ignore: &FilePickerIgnoreOptions) -> Self {
        let names = [
            (!ignore.disable_git).then_some(".gitignore"),
            (!ignore.disable_ignore_files).then_some(".ignore"),
        ];
        Self {
            names: names.into_iter().flatten().collect(),
            started: SystemTime::now(),
            read: Mutex::default(),
        }
    }

    // Hidden or ignored as files or not, the rules come from these.
    pub(crate) fn read_in(&self, dir: &Path) {
        for name in &self.names {
            self.read_file(dir.join(name));
        }
    }

    // A repository's exclude file, whose rules a walk of its whole work tree reads.
    pub(crate) fn read_file(&self, path: PathBuf) {
        if let Some(content) = crate::stat::read_regular(&path) {
            self.read.lock().push((path, hash(&content)));
        }
    }

    // What each file held when first read, with the time the walk ran. A file that holds
    // something else once the walk ended may have given the walker either: it is unknown.
    pub(crate) fn finish(&self) -> Vec<(PathBuf, IgnoreSource)> {
        let walk = (self.started, SystemTime::now());
        let mut read = std::mem::take(&mut *self.read.lock());
        read.sort_by(|(a, _), (b, _)| a.cmp(b));
        read.dedup_by(|later, first| later.0 == first.0);
        (read.into_iter())
            .map(|(path, first)| {
                let last = crate::stat::read_regular(&path).map(|content| hash(&content));
                let hash = Some(first).filter(|first| last == Some(*first));
                (path, IgnoreSource { hash, walk })
            })
            .collect()
    }
}

fn hash(content: &[u8]) -> u64 {
    crate::types::fixed_hasher().hash_one(content)
}
