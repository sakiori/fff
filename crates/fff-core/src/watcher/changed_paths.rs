use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::{Path, PathBuf};

use ahash::{AHashMap, AHashSet};
use notify::EventKind;

use super::background_watcher::MAX_BATCH_PATHS;

// Syncthing's aggregator turns more changes than this in one directory into the directory.
const MAX_ENTRIES_PER_DIR: usize = 128;
const MAX_IGNORE_FILES: usize = 256;
const MAX_GIT_EVENTS: usize = 1024;

// One change a watcher event stands for, its kind only a hint of what to check again.
pub(crate) enum Change {
    // A path the watcher named, checked again whatever its stat says; `recursive` when it may
    // be a directory created, removed or renamed, whose subtree is walked again.
    Path { path: PathBuf, recursive: bool },
    // Events may have been lost anywhere below the root.
    Root,
    // Events were lost below this directory: it is walked again, its ignore rules with it.
    Lost(PathBuf),
    // An ignore file whose rules may have changed; its path is recorded as well.
    IgnoreFile(PathBuf),
    // A rename the backend paired, from and to, whose paths come as entries of their own.
    Renamed(PathBuf, PathBuf),
    // An event for one path of git state, classified when the set is reconciled.
    Git(notify::Event),
    // Events were lost for this path of git state: its repositories are announced again.
    LostGit(PathBuf),
}

// Paths to check and directories to walk again, as Watchman's pending collection: a directory
// absorbs its descendants, and past the bounds entries become their parents.
pub(crate) struct ChangedPaths {
    root: PathBuf,
    // `true` for a recursive entry; never below a recursive entry.
    entries: BTreeMap<PathBuf, bool>,
    // How many entries each directory holds directly.
    children: AHashMap<PathBuf, usize>,
    whole_root: bool,
    // Every path the watcher named, up to a bound, also below a recursive entry: Watchman's
    // pending collection keeps each, so a walk reports it changed whatever its stat says.
    named: AHashSet<PathBuf>,
    ignore_files: AHashSet<PathBuf>,
    // Ignore files, and directories whose events were lost, past their bound: the root's rules
    // are read again.
    rules_lost: bool,
    lost_dirs: AHashSet<PathBuf>,
    renames: Vec<(PathBuf, PathBuf)>,
    git: Vec<notify::Event>,
    git_seen: AHashSet<(PathBuf, EventKind, bool)>,
    lost_git: AHashSet<PathBuf>,
    git_lost: bool,
}

// What a set asks of the index once drained.
pub(crate) struct Pending {
    // Sorted; `true` for a recursive entry. The root alone when the whole tree is checked again.
    pub(crate) entries: Vec<(PathBuf, bool)>,
    pub(crate) named: AHashSet<PathBuf>,
    pub(crate) ignore_files: Vec<PathBuf>,
    // Directories whose walk replaces their rules even where no ignore file is known.
    pub(crate) rule_dirs: Vec<PathBuf>,
    pub(crate) renames: Vec<(PathBuf, PathBuf)>,
    pub(crate) git: Vec<notify::Event>,
    // Paths of git state whose events were lost, and whether more were than are kept.
    pub(crate) lost_git: Vec<PathBuf>,
    pub(crate) git_lost: bool,
}

impl ChangedPaths {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            entries: BTreeMap::new(),
            children: AHashMap::new(),
            whole_root: false,
            named: AHashSet::new(),
            ignore_files: AHashSet::new(),
            rules_lost: false,
            lost_dirs: AHashSet::new(),
            renames: Vec::new(),
            git: Vec::new(),
            git_seen: AHashSet::new(),
            lost_git: AHashSet::new(),
            git_lost: false,
        }
    }

    pub(crate) fn add(&mut self, changes: impl IntoIterator<Item = Change>) {
        for change in changes {
            match change {
                Change::Path { path, recursive } => {
                    self.insert(&path, recursive);
                    self.name(path);
                }
                Change::Renamed(from, to) => {
                    if self.renames.len() < MAX_BATCH_PATHS {
                        self.renames.push((from, to));
                    }
                }
                Change::Root => self.reach_root(),
                Change::IgnoreFile(path) => {
                    if self.ignore_files.len() < MAX_IGNORE_FILES {
                        self.ignore_files.insert(path.clone());
                    } else if !self.ignore_files.contains(&path) {
                        self.rules_lost = true;
                    }
                    self.insert(&path, false);
                    self.name(path);
                }
                Change::Lost(dir) => {
                    if self.lost_dirs.len() < MAX_IGNORE_FILES {
                        self.lost_dirs.insert(dir.clone());
                    } else if !self.lost_dirs.contains(&dir) {
                        self.rules_lost = true;
                    }
                    self.insert(&dir, true);
                }
                Change::LostGit(path) => {
                    if self.lost_git.len() < MAX_IGNORE_FILES {
                        self.lost_git.insert(path);
                    } else if !self.lost_git.contains(&path) {
                        self.git_lost = true;
                    }
                }
                Change::Git(event) => {
                    let Some(path) = event.paths.first() else {
                        continue;
                    };
                    let key = (path.clone(), event.kind, event.need_rescan());
                    if self.git_seen.len() >= MAX_GIT_EVENTS {
                        self.git_lost |= !self.git_seen.contains(&key);
                    } else if self.git_seen.insert(key) {
                        self.git.push(event);
                    }
                }
            }
            while self.entries.len() > MAX_BATCH_PATHS && !self.whole_root {
                self.widen_deepest();
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn entry_paths(&self) -> Vec<PathBuf> {
        let root = self.whole_root.then(|| self.root.clone());
        root.into_iter()
            .chain(self.entries.keys().cloned())
            .collect()
    }

    pub(crate) fn is_empty(&self) -> bool {
        !self.whole_root
            && self.entries.is_empty()
            && self.git.is_empty()
            && self.lost_git.is_empty()
            && !self.git_lost
    }

    fn name(&mut self, path: PathBuf) {
        if self.named.len() < MAX_BATCH_PATHS * 4 {
            self.named.insert(path);
        }
    }

    fn reach_root(&mut self) {
        self.whole_root = true;
        self.entries.clear();
        self.children.clear();
    }

    fn insert(&mut self, path: &Path, dir: bool) {
        if self.whole_root {
            return;
        }
        if path == self.root {
            return self.reach_root();
        }
        let Some(parent) = path
            .parent()
            .filter(|parent| parent.starts_with(&self.root))
        else {
            return;
        };
        let absorbed = (parent.ancestors())
            .take_while(|ancestor| ancestor.starts_with(&self.root))
            .any(|ancestor| self.entries.get(ancestor) == Some(&true));
        if absorbed {
            return;
        }
        match self.entries.get(path) {
            Some(&known) if known || !dir => return,
            Some(_) => {}
            None => *self.children.entry(parent.to_path_buf()).or_default() += 1,
        }
        if dir {
            self.remove_below(path);
        }
        self.entries.insert(path.to_path_buf(), dir);

        if self
            .children
            .get(parent)
            .is_some_and(|&count| count > MAX_ENTRIES_PER_DIR)
        {
            self.insert(parent, true);
        }
    }

    fn remove_below(&mut self, dir: &Path) {
        let below: Vec<PathBuf> = (self.entries)
            .range::<Path, _>((Bound::Excluded(dir), Bound::Unbounded))
            .map(|(path, _)| path)
            .take_while(|path| path.starts_with(dir))
            .cloned()
            .collect();
        for path in below {
            self.remove(&path);
        }
    }

    fn remove(&mut self, path: &Path) {
        self.entries.remove(path);
        if let Some(parent) = path.parent()
            && let Some(count) = self.children.get_mut(parent)
        {
            *count -= 1;
            if *count == 0 {
                self.children.remove(parent);
            }
        }
    }

    // Replaces the deepest entries with their parent directories.
    fn widen_deepest(&mut self) {
        let depth = |path: &Path| {
            path.strip_prefix(&self.root)
                .map_or(0, |rel| rel.iter().count())
        };
        let deepest = self
            .entries
            .keys()
            .map(|path| depth(path))
            .max()
            .unwrap_or(0);
        if deepest <= 1 {
            return self.reach_root();
        }
        let widened: Vec<PathBuf> = (self.entries.keys())
            .filter(|path| depth(path) == deepest)
            .cloned()
            .collect();
        for path in widened {
            if let Some(parent) = path.parent() {
                let parent = parent.to_path_buf();
                self.insert(&parent, true);
            }
        }
    }

    pub(crate) fn drain(self) -> Pending {
        let mut entries: Vec<(PathBuf, bool)> = match self.whole_root {
            true => vec![(self.root.clone(), true)],
            false => self.entries.into_iter().collect(),
        };
        let mut rule_dirs: Vec<PathBuf> = self.lost_dirs.into_iter().collect();
        if self.rules_lost || self.whole_root {
            rule_dirs.push(self.root.clone());
            entries = vec![(self.root, true)];
        }
        rule_dirs.sort_unstable();
        let mut ignore_files: Vec<PathBuf> = self.ignore_files.into_iter().collect();
        ignore_files.sort_unstable();
        Pending {
            entries,
            named: self.named,
            ignore_files,
            rule_dirs,
            renames: self.renames,
            git: self.git,
            lost_git: self.lost_git.into_iter().collect(),
            git_lost: self.git_lost,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::CreateKind;

    fn root() -> PathBuf {
        PathBuf::from("/w")
    }

    fn files(rels: impl IntoIterator<Item = String>) -> Vec<Change> {
        rels.into_iter()
            .map(|rel| Change::Path {
                path: root().join(rel),
                recursive: false,
            })
            .collect()
    }

    fn entries(changed: &ChangedPaths) -> Vec<(String, bool)> {
        (changed.entries.iter())
            .map(|(path, dir)| {
                let rel = path.strip_prefix(root()).unwrap();
                (rel.to_string_lossy().into_owned(), *dir)
            })
            .collect()
    }

    #[test]
    fn a_directory_absorbs_the_paths_below_it() {
        let mut changed = ChangedPaths::new(&root());
        changed.add(files(["a/b/x.rs".into(), "a/c.rs".into(), "d.rs".into()]));
        changed.add([Change::Path {
            path: root().join("a"),
            recursive: true,
        }]);
        changed.add(files(["a/b/y.rs".into()]));
        assert_eq!(
            entries(&changed),
            [("a".into(), true), ("d.rs".into(), false)]
        );
    }

    #[test]
    fn more_than_128_changes_in_a_directory_become_the_directory() {
        let mut changed = ChangedPaths::new(&root());
        changed.add(files(
            (0..=MAX_ENTRIES_PER_DIR).map(|i| format!("gen/f{i}.rs")),
        ));
        changed.add(files(["src/main.rs".into()]));
        assert_eq!(
            entries(&changed),
            [("gen".into(), true), ("src/main.rs".into(), false)]
        );
    }

    #[test]
    fn five_thousand_paths_aggregate_into_their_parent_directories() {
        let mut changed = ChangedPaths::new(&root());
        let paths: Vec<String> = (0..5000)
            .map(|i| format!("pkg{}/lib/m{}.rs", i / 100, i % 100))
            .collect();
        changed.add(files(paths.iter().cloned()));
        assert!(!changed.whole_root);
        let entries = entries(&changed);
        assert!(entries.len() <= MAX_BATCH_PATHS, "{}", entries.len());
        assert!(
            entries
                .iter()
                .any(|(rel, dir)| *dir && rel.ends_with("/lib"))
        );
        // Every path is an entry or lies in a directory entry.
        for path in &paths {
            let covered = (entries.iter())
                .any(|(rel, dir)| rel == path || *dir && Path::new(path).starts_with(rel.as_str()));
            assert!(covered, "{path} is lost");
        }
    }

    #[test]
    fn aggregation_reaching_the_root_reconciles_the_root() {
        let mut changed = ChangedPaths::new(&root());
        changed.add(files((0..=MAX_BATCH_PATHS).map(|i| format!("d{i}/f.rs"))));
        assert!(changed.whole_root);
        assert!(changed.entries.is_empty());
        let pending = changed.drain();
        assert_eq!(pending.entries, [(root(), true)]);
        assert_eq!(pending.rule_dirs, [root()]);
    }

    #[test]
    fn a_change_at_the_root_reconciles_the_root() {
        let mut changed = ChangedPaths::new(&root());
        changed.add(files(["a.rs".into()]));
        changed.add([Change::Root]);
        changed.add(files(["b.rs".into()]));
        assert!(changed.whole_root && changed.entries.is_empty());
    }

    #[test]
    fn git_state_is_kept_apart_and_bounded() {
        let mut changed = ChangedPaths::new(&root());
        let git = |rel: &str| {
            let event = notify::Event::new(EventKind::Create(CreateKind::File));
            Change::Git(event.add_path(root().join(rel)))
        };
        changed.add((0..MAX_GIT_EVENTS).map(|i| git(&format!(".git/r{i}"))));
        changed.add([git(".git/r0")]);
        assert!(!changed.git_lost && changed.entries.is_empty());
        changed.add([git(".git/HEAD")]);
        assert!(changed.git_lost);
    }
}
