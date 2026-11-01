use crate::shared::{SharedFilePicker, related};
use git2::Repository;
use notify::event::ModifyKind;
use notify::{EventKind, PathOp, RecursiveMode};
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// Where a repository keeps its refs, as its common dir's config names it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RefStorage {
    Files,
    Reftable,
}

impl RefStorage {
    pub(crate) fn of(common_dir: &Path) -> Self {
        let format = git2::Config::open(&common_dir.join("config"))
            .and_then(|config| config.get_string("extensions.refstorage"));
        match format {
            Ok(format) if format.eq_ignore_ascii_case("reftable") => RefStorage::Reftable,
            _ => RefStorage::Files,
        }
    }
}

// A repository whose refs a listener follows.
#[derive(Clone, Debug)]
struct Followed {
    git_dir: PathBuf,
    work_tree: PathBuf,
    common_dir: PathBuf,
    storage: RefStorage,
}

// The repository containing the index's root, which the walk registers only at its root.
struct OwnRepository {
    git_dir: PathBuf,
    work_tree: PathBuf,
    common_dir: PathBuf,
}

// Watches of the refs of the repositories related to a ref listener's scope, kept while one
// is subscribed. Planned only under the watcher's debouncer lock.
pub(crate) struct RefWatches {
    base_path: PathBuf,
    recursive: bool,
    // Watches stay once added, as removing one restarts the backend's stream.
    keep: bool,
    own: Option<OwnRepository>,
    state: Mutex<RefState>,
}

#[derive(Default)]
struct RefState {
    followed: Arc<[Followed]>,
    // Every path these watches added, also one whose watch failed, as a recursive watch can
    // fail halfway: all go with the last listener.
    watched: Arc<[(PathBuf, RecursiveMode)]>,
    // Ref stores a listener follows that did not exist yet.
    missing: Vec<PathBuf>,
    // The ref storage of each common dir, read once until its config may have changed.
    storages: Vec<(PathBuf, RefStorage)>,
}

// The watch changes bringing the ref watches in line with the listeners, and the work trees
// whose ref store appeared since: their refs changed before it was watched.
pub(crate) struct RefPlan {
    pub(crate) ops: Vec<PathOp>,
    pub(crate) appeared: Vec<PathBuf>,
    // The paths the ref watches consider, watched or not there yet.
    pub(crate) candidates: usize,
}

impl RefWatches {
    pub(crate) fn new(
        base_path: &Path,
        recursive: bool,
        git_workdir: Option<&Path>,
        keep: bool,
    ) -> Self {
        Self {
            base_path: base_path.to_path_buf(),
            recursive,
            keep,
            own: own_repository(base_path, git_workdir),
            state: Mutex::default(),
        }
    }

    // The own repository's ref watches, set up with the stream where watches stay, so no
    // listener restarts it.
    pub(crate) fn own_targets(&self, picker: &SharedFilePicker) -> Vec<PathOp> {
        let Some(own) = self.own.as_ref().filter(|_| self.keep) else {
            return Vec::new();
        };
        let followed = [Followed {
            git_dir: own.git_dir.clone(),
            work_tree: own.work_tree.clone(),
            common_dir: own.common_dir.clone(),
            storage: RefStorage::of(&own.common_dir),
        }];
        let targets: Vec<_> = (ref_targets(&followed, self.keep).into_iter())
            .filter(|(path, _, _)| !self.covered_otherwise(path, picker) && path.is_dir())
            .map(|(path, mode, _)| (path, mode))
            .collect();
        let ops = (targets.iter())
            .map(|(path, mode)| PathOp::Watch(path.clone(), notify::WatchPathConfig::new(*mode)))
            .collect();
        self.state.lock().watched = targets.into();
        ops
    }

    pub(crate) fn own_git_dir(&self) -> Option<PathBuf> {
        self.own.as_ref().map(|own| own.git_dir.clone())
    }

    // Whether the own git dir, whose index and HEAD decide the git status, lies outside the tree.
    pub(crate) fn own_git_dir_outside(&self) -> bool {
        (self.own.as_ref()).is_some_and(|own| !own.git_dir.starts_with(&self.base_path))
    }

    #[cfg(all(test, target_os = "macos"))]
    pub(crate) fn watched(&self) -> Vec<PathBuf> {
        (self.state.lock().watched.iter())
            .map(|(path, _)| path.clone())
            .collect()
    }

    // The own work tree, where its git state lies outside the tree.
    pub(crate) fn own_work_tree_outside(&self) -> Option<PathBuf> {
        (self.own.as_ref())
            .filter(|own| {
                !own.git_dir.starts_with(&self.base_path)
                    || !own.common_dir.starts_with(&self.base_path)
            })
            .map(|own| own.work_tree.clone())
    }

    // `renew` is a repository created again, whose watches went with the old one.
    pub(crate) fn plan(&self, picker: &SharedFilePicker, renew: Option<&Path>) -> RefPlan {
        let scopes = picker.ref_scopes();
        let mut state = self.state.lock();
        let followed = if scopes.is_empty() {
            state.storages.clear();
            Vec::new()
        } else {
            self.followed(picker, &scopes, &mut state.storages)
        };
        let mut ops = Vec::new();
        let mut appeared = Vec::new();
        let mut watched = Vec::new();
        let mut missing = Vec::new();
        for (path, mode, work_trees) in ref_targets(&followed, self.keep) {
            if self.covered_otherwise(&path, picker) {
                continue;
            }
            if !path.is_dir() {
                missing.push(path);
                continue;
            }
            let known = state.watched.iter().any(|(known, _)| *known == path);
            if !known || renew.is_some_and(|dir| path.starts_with(dir)) {
                ops.push(PathOp::Watch(
                    path.clone(),
                    notify::WatchPathConfig::new(mode),
                ));
            }
            if state.missing.contains(&path) {
                appeared.extend(work_trees);
            }
            watched.push((path, mode));
        }
        for (path, mode) in state.watched.iter() {
            if watched.iter().any(|(kept, _)| kept == path) {
                continue;
            }
            if self.keep {
                watched.push((path.clone(), *mode));
            } else if !self.covered_otherwise(path, picker) {
                ops.push(PathOp::unwatch(path.clone()));
            }
        }
        appeared.sort_unstable();
        appeared.dedup();
        state.followed = followed.into();
        state.watched = watched.into();
        state.missing = missing;
        let candidates = state.watched.len() + state.missing.len();
        RefPlan {
            ops,
            appeared,
            candidates,
        }
    }

    // A ref store appeared or a config changed: storages are read again.
    pub(crate) fn reread_storages(&self) {
        self.state.lock().storages.clear();
    }

    // What a batch's ref changes are told by; none while nothing is followed or watched.
    pub(crate) fn signals(&self) -> Option<RefSignals> {
        let state = self.state.lock();
        (!state.followed.is_empty() || !state.watched.is_empty()).then(|| RefSignals {
            unusual: state.followed.iter().any(|followed| {
                !crate::watch::is_git_file(&followed.common_dir)
                    && followed.common_dir.starts_with(&self.base_path)
            }),
            followed: Arc::clone(&state.followed),
            watched: Arc::clone(&state.watched),
            own_work_tree: self.own.as_ref().map(|own| own.work_tree.clone()),
        })
    }

    // The index's own repository and those registered inside the index whose work tree is
    // related to a listener's scope.
    fn followed(
        &self,
        picker: &SharedFilePicker,
        scopes: &[PathBuf],
        storages: &mut Vec<(PathBuf, RefStorage)>,
    ) -> Vec<Followed> {
        let wanted = |work_tree: &Path| scopes.iter().any(|scope| related(scope, work_tree));
        let own = (self.own.iter())
            .filter(|own| wanted(&own.work_tree))
            .map(|own| {
                (
                    own.git_dir.clone(),
                    own.work_tree.clone(),
                    own.common_dir.clone(),
                )
            });
        let registered = (picker.registered_git_dirs().into_iter())
            .filter(|known| {
                known.work_tree.starts_with(&self.base_path) && wanted(&known.work_tree)
            })
            .map(|known| (known.git_dir, known.work_tree, known.common_dir));
        let repositories: Vec<_> = own.chain(registered).collect();
        let mut followed: Vec<Followed> = Vec::new();
        for (git_dir, work_tree, common_dir) in repositories {
            if followed.iter().any(|known| known.git_dir == git_dir) {
                continue;
            }
            let storage = match storages.iter().find(|(dir, _)| *dir == common_dir) {
                Some((_, storage)) => *storage,
                None => {
                    let storage = RefStorage::of(&common_dir);
                    storages.push((common_dir.clone(), storage));
                    storage
                }
            };
            followed.push(Followed {
                git_dir,
                work_tree: work_tree.components().collect(),
                common_dir,
                storage,
            });
        }
        followed
    }

    // Whether the tree's or a git state watch delivers `dir` already; a backend keeps one
    // watch per path, so unwatching it would end that one too.
    fn covered_otherwise(&self, dir: &Path, picker: &SharedFilePicker) -> bool {
        self.recursive && dir.starts_with(&self.base_path)
            || self.own.as_ref().is_some_and(|own| own.git_dir == dir)
            || picker
                .registered_work_tree(dir)
                .is_some_and(|work_tree| work_tree.starts_with(&self.base_path))
    }
}

// The ref changes of one batch: the work trees whose refs changed, and whether a ref store or
// its format may have changed, which the watches follow.
#[derive(Default)]
pub(crate) struct RefChanges {
    pub(crate) work_trees: ahash::AHashSet<PathBuf>,
    pub(crate) resync: bool,
}

pub(crate) struct RefSignals {
    followed: Arc<[Followed]>,
    watched: Arc<[(PathBuf, RecursiveMode)]>,
    own_work_tree: Option<PathBuf>,
    // A followed common dir inside the index without a `.git` component, as a bare one.
    unusual: bool,
}

impl RefSignals {
    // Refs live only in git dirs, which sit under a `.git` or outside the index.
    pub(crate) fn may_hold(&self, path: &Path, base_path: &Path) -> bool {
        crate::watch::is_git_file(path)
            || !path.starts_with(base_path)
            || self.unusual
                && (self.followed.iter()).any(|followed| path.starts_with(&followed.common_dir))
    }

    // Git writes every ref, HEAD, table list and config through a `.lock` file it renames into
    // place.
    pub(crate) fn classify(&self, path: &Path, kind: EventKind, changes: &mut RefChanges) {
        if path
            .extension()
            .is_some_and(|extension| extension == "lock")
        {
            return;
        }
        for followed in self.followed.iter() {
            let changed = |dir: &Path| path.strip_prefix(dir).ok();
            if path.file_name().is_some_and(|name| name == "HEAD")
                && path.parent() == Some(followed.git_dir.as_path())
            {
                changes.work_trees.insert(followed.work_tree.clone());
            }
            if let Some(inside) = changed(&followed.common_dir) {
                let ref_changed = match followed.storage {
                    RefStorage::Files => {
                        inside == Path::new("packed-refs")
                            || ["refs/heads", "refs/tags", "refs/remotes"]
                                .iter()
                                .any(|namespace| inside.starts_with(namespace))
                                && !namespace_touched(path, kind)
                    }
                    RefStorage::Reftable => inside == Path::new("reftable/tables.list"),
                };
                // Upstreams and remotes live in the shared config, ref state to consumers.
                let config = inside == Path::new("config");
                // An admin dir touched by the files of its worktree changes no set.
                let worktrees = worktrees_change(inside).filter(|_| !namespace_touched(path, kind));
                if ref_changed || config || worktrees.is_some() {
                    changes.work_trees.insert(followed.work_tree.clone());
                }
                changes.resync |= ["refs", "reftable", "config"]
                    .iter()
                    .any(|store| inside == Path::new(store))
                    || worktrees == Some(WorktreesChange::Set);
            }
            // A work tree's own config, where `extensions.worktreeConfig` is set.
            if path
                .file_name()
                .is_some_and(|name| name == "config.worktree")
                && path.parent() == Some(followed.git_dir.as_path())
            {
                changes.work_trees.insert(followed.work_tree.clone());
            }
            // A linked worktree of a reftable repository keeps its HEAD in tables of its own.
            if followed.storage == RefStorage::Reftable
                && followed.git_dir != followed.common_dir
                && let Some(inside) = changed(&followed.git_dir)
            {
                if inside == Path::new("reftable/tables.list") {
                    changes.work_trees.insert(followed.work_tree.clone());
                }
                changes.resync |= inside == Path::new("reftable");
            }
        }
    }

    // A path only the ref watches deliver, outside the index: no git state of the index.
    // A followed linked worktree's git dir, as the own one, lies under `worktrees` but holds
    // git state.
    pub(crate) fn ref_only(&self, path: &Path, base_path: &Path) -> bool {
        !path.starts_with(base_path)
            && !(self.followed.iter()).any(|followed| {
                followed.git_dir != followed.common_dir && path.starts_with(&followed.git_dir)
            })
            && self.watched.iter().any(|(dir, mode)| match mode {
                RecursiveMode::Recursive => path.starts_with(dir),
                RecursiveMode::NonRecursive => path.parent() == Some(dir.as_path()),
            })
    }

    // A file of another worktree's admin dir that only the ref watches deliver, as its index,
    // HEAD, logs or locks: it changes no worktree set, so a batch drops it by its prefix alone.
    // The watch is the followed common dir's `worktrees` itself or one admin dir in it.
    pub(crate) fn foreign_admin_file(&self, path: &Path, base_path: &Path) -> bool {
        if path.starts_with(base_path)
            || (self.followed.iter()).any(|followed| {
                followed.git_dir != followed.common_dir && path.starts_with(&followed.git_dir)
            })
        {
            return false;
        }
        self.followed.iter().any(|followed| {
            let worktrees = followed.common_dir.join("worktrees");
            let Ok(rest) = path.strip_prefix(&worktrees) else {
                return false;
            };
            let mut parts = rest.components().map(|part| part.as_os_str());
            let (Some(id), Some(name)) = (parts.next(), parts.next()) else {
                return false;
            };
            let deeper = parts.next().is_some();
            let admin_dir = worktrees.join(id);
            let delivered = self.watched.iter().any(|(dir, mode)| match mode {
                RecursiveMode::Recursive => *dir == worktrees,
                RecursiveMode::NonRecursive => *dir == admin_dir && !deeper,
            });
            delivered && (deeper || (name != "gitdir" && name != "locked"))
        })
    }

    pub(crate) fn is_own(&self, work_tree: &Path) -> bool {
        self.own_work_tree.as_deref() == Some(work_tree)
    }
}

// Which linked worktrees a repository has, and where they are: its `worktrees`, each admin dir
// in it and the admin dir's `gitdir`. Other files there belong to the worktree itself.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WorktreesChange {
    Set,
    Location,
}

fn worktrees_change(inside: &Path) -> Option<WorktreesChange> {
    let mut parts = inside.components().map(|part| part.as_os_str());
    if parts.next()? != "worktrees" {
        return None;
    }
    match (parts.next(), parts.next(), parts.next()) {
        (None, None, None) | (Some(_), None, None) => Some(WorktreesChange::Set),
        // `worktree move` and repair write `gitdir`, `worktree lock` and `unlock` `locked`.
        (Some(_), Some(file), None) if file == "gitdir" || file == "locked" => {
            Some(WorktreesChange::Location)
        }
        _ => None,
    }
}

// A namespace directory changes with every ref and lock file in it; only its creation or
// removal is a ref change, as a namespaced branch's directory may come before its watch.
fn namespace_touched(path: &Path, kind: EventKind) -> bool {
    matches!(kind, EventKind::Modify(modify) if !matches!(modify, ModifyKind::Name(_)))
        && path.is_dir()
}

// What to watch for the refs of the followed repositories, with the work trees each serves:
// each common dir itself, which holds `packed-refs` and sees its ref store appear, and its
// `refs` tree or reftable tables, and the tables of a reftable repository's linked worktree.
// Where watches stay, `worktrees` is one recursive watch, so a worktree added changes no
// watched path; elsewhere it costs a watch for each admin dir in it.
fn ref_targets(followed: &[Followed], keep: bool) -> Vec<(PathBuf, RecursiveMode, Vec<PathBuf>)> {
    let mut targets: Vec<(PathBuf, RecursiveMode, Vec<PathBuf>)> = Vec::new();
    let mut add = |path: PathBuf, mode, work_tree: &Path| match targets
        .iter_mut()
        .find(|(known, _, _)| *known == path)
    {
        Some((_, _, work_trees)) => work_trees.push(work_tree.to_path_buf()),
        None => targets.push((path, mode, vec![work_tree.to_path_buf()])),
    };
    for followed in followed {
        let common_dir = &followed.common_dir;
        add(
            common_dir.clone(),
            RecursiveMode::NonRecursive,
            &followed.work_tree,
        );
        // The linked worktrees' admin dirs, whose set and `gitdir` files are ref state.
        let worktrees = common_dir.join("worktrees");
        if keep {
            add(worktrees, RecursiveMode::Recursive, &followed.work_tree);
        } else {
            let admin_dirs = std::fs::read_dir(&worktrees)
                .into_iter()
                .flatten()
                .flatten();
            for admin_dir in
                admin_dirs.filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            {
                add(
                    admin_dir.path(),
                    RecursiveMode::NonRecursive,
                    &followed.work_tree,
                );
            }
            add(worktrees, RecursiveMode::NonRecursive, &followed.work_tree);
        }
        match followed.storage {
            RefStorage::Files => add(
                common_dir.join("refs"),
                RecursiveMode::Recursive,
                &followed.work_tree,
            ),
            RefStorage::Reftable => {
                add(
                    common_dir.join("reftable"),
                    RecursiveMode::NonRecursive,
                    &followed.work_tree,
                );
                if followed.git_dir != *common_dir {
                    add(
                        followed.git_dir.join("reftable"),
                        RecursiveMode::NonRecursive,
                        &followed.work_tree,
                    );
                }
            }
        }
    }
    targets
}

// The index's own repository: the one git's library found, or a reftable one, which it
// cannot open.
fn own_repository(base_path: &Path, git_workdir: Option<&Path>) -> Option<OwnRepository> {
    match git_workdir {
        Some(workdir) => {
            let repo = Repository::open(workdir).ok()?;
            Some(OwnRepository {
                git_dir: repo.path().components().collect(),
                work_tree: workdir.components().collect(),
                common_dir: canonical_dir(repo.commondir()),
            })
        }
        None => reftable_repository(base_path),
    }
}

// libgit2 refuses a reftable repository for its format only after git's discovery found it,
// with its ceilings and filesystem boundary, so that discovery decides, and the ownership
// check libgit2 would run after the format's is made here.
fn reftable_repository(base_path: &Path) -> Option<OwnRepository> {
    let flags = git2::RepositoryOpenFlags::FROM_ENV;
    match Repository::open_ext(base_path, flags, std::iter::empty::<&std::ffi::OsStr>()) {
        Ok(_) => return None,
        Err(error)
            if matches!(
                error.code(),
                git2::ErrorCode::NotFound | git2::ErrorCode::Owner
            ) =>
        {
            return None;
        }
        Err(_) => {}
    }
    // Discovery stops at the first `.git`, also one that names no git dir.
    let work_tree = base_path
        .ancestors()
        .find(|dir| dir.join(".git").symlink_metadata().is_ok())?;
    let git_dir = crate::watch::git_dir_of(work_tree)?;
    let common_dir = common_dir_of(&git_dir);
    if !crate::watch::is_git_dir(&git_dir) || RefStorage::of(&common_dir) != RefStorage::Reftable {
        return None;
    }
    let dot_git = work_tree.join(".git");
    let gitlink = dot_git.is_file().then_some(dot_git.as_path());
    trusted(work_tree, gitlink, &git_dir).then(|| OwnRepository {
        git_dir,
        work_tree: work_tree.to_path_buf(),
        common_dir,
    })
}

// git's ownership rule (git v2.47.0, setup.c `ensure_valid_ownership`): the `.git` file, the
// work tree and the git dir belong to the current user, or the work tree is a safe directory.
#[cfg(unix)]
fn trusted(work_tree: &Path, gitlink: Option<&Path>, git_dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let euid = unsafe { libc::geteuid() };
    let sudo_uid = sudo_uid(std::env::var("SUDO_UID").ok().as_deref());
    let owned = |path: &Path| {
        (path.symlink_metadata()).is_ok_and(|metadata| owned_by(metadata.uid(), euid, sudo_uid))
    };
    let mut paths = gitlink.into_iter().chain([work_tree, git_dir]);
    paths.all(owned) || {
        let env = |name: &str| std::env::var(name).ok();
        // git's compiled-in system file is known to its own build; libgit2's stands in for it.
        let system = git2::Config::find_system().ok();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let cwd = std::env::current_dir().ok();
        // git refuses to go on when it cannot read its config.
        safe_directories(&env, system).is_some_and(|entries| {
            safe_directory(work_tree, &entries, home.as_deref(), cwd.as_deref())
        })
    }
}

// Ownership elsewhere is not something git's rule here can tell.
#[cfg(not(unix))]
fn trusted(_work_tree: &Path, _gitlink: Option<&Path>, _git_dir: &Path) -> bool {
    false
}

// git-compat-util.h `is_path_owned_by_current_uid`: root owns what root owns, and under sudo
// also what the user who ran it owns.
#[cfg(unix)]
fn owned_by(owner: u32, euid: u32, sudo_uid: Option<u32>) -> bool {
    match euid {
        0 if owner == 0 => true,
        0 => sudo_uid.is_some_and(|uid| uid == owner),
        euid => owner == euid,
    }
}

// git reads `SUDO_UID` only when it is a whole decimal number.
#[cfg(unix)]
fn sudo_uid(value: Option<&str>) -> Option<u32> {
    value.filter(|value| !value.is_empty())?.parse().ok()
}

// The `safe.directory` values of git's protected config in its order (git v2.47.0, config.c
// `do_git_config_sequence` without a repository): the system file unless
// `GIT_CONFIG_NOSYSTEM`, the one `GIT_CONFIG_GLOBAL` names or else the XDG and home files,
// then `GIT_CONFIG_COUNT` entries and `GIT_CONFIG_PARAMETERS`. `None` where git would fail;
// a value is `None` for a key without one.
#[cfg(unix)]
fn safe_directories(
    env: &dyn Fn(&str) -> Option<String>,
    system: Option<PathBuf>,
) -> Option<Vec<Option<String>>> {
    let mut files = Vec::new();
    let no_system = match env("GIT_CONFIG_NOSYSTEM") {
        Some(value) => git_bool(&value)?,
        None => false,
    };
    if !no_system {
        files.extend(env("GIT_CONFIG_SYSTEM").map(PathBuf::from).or(system));
    }
    match env("GIT_CONFIG_GLOBAL") {
        Some(global) => files.push(PathBuf::from(global)),
        None => {
            let home = env("HOME").map(PathBuf::from);
            let xdg = env("XDG_CONFIG_HOME").filter(|xdg| !xdg.is_empty());
            let xdg = xdg
                .map(PathBuf::from)
                .or_else(|| Some(home.as_ref()?.join(".config")));
            files.extend(xdg.map(|xdg| xdg.join("git/config")));
            files.extend(home.map(|home| home.join(".gitconfig")));
        }
    }
    let mut values = Vec::new();
    for file in files.iter().filter(|file| file.is_file()) {
        let config = git2::Config::open(file).ok()?;
        let Ok(mut entries) = config.multivar("safe.directory", None) else {
            continue;
        };
        while let Some(entry) = entries.next() {
            values.push(entry.ok()?.value().ok().map(str::to_owned));
        }
    }
    for (key, value) in command_line_config(env)? {
        if key.eq_ignore_ascii_case("safe.directory") {
            values.push(value);
        }
    }
    Some(values)
}

// config.c `git_config_from_parameters`: `GIT_CONFIG_COUNT` pairs, then the single-quoted
// `'key'='value'` or `'key=value'` list of `GIT_CONFIG_PARAMETERS`.
#[cfg(unix)]
fn command_line_config(
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<Vec<(String, Option<String>)>> {
    let mut pairs = Vec::new();
    if let Some(count) = env("GIT_CONFIG_COUNT") {
        let count: usize = if count.is_empty() {
            0
        } else {
            count.parse().ok()?
        };
        for n in 0..count {
            let key = env(&format!("GIT_CONFIG_KEY_{n}"))?;
            let value = env(&format!("GIT_CONFIG_VALUE_{n}"))?;
            pairs.push((key, Some(value)));
        }
    }
    let Some(parameters) = env("GIT_CONFIG_PARAMETERS") else {
        return Some(pairs);
    };
    let ends_word = |rest: &str| rest.is_empty() || rest.starts_with(char::is_whitespace);
    let mut rest = parameters.trim_start();
    while !rest.is_empty() {
        let (key, after) = sq_dequote_step(rest)?;
        let after = match after.strip_prefix('=') {
            Some(value) if value.starts_with('\'') => {
                let (value, after) = sq_dequote_step(value)?;
                pairs.push((key, Some(value)));
                after
            }
            Some(after) if ends_word(after) => {
                pairs.push((key, None));
                after
            }
            None if ends_word(after) => {
                let (key, value) = match key.split_once('=') {
                    Some((key, value)) => (key, Some(value.to_owned())),
                    None => (key.as_str(), None),
                };
                pairs.push((key.trim().to_owned(), value));
                after
            }
            _ => return None,
        };
        if !ends_word(after) {
            return None;
        }
        rest = after.trim_start();
    }
    Some(pairs)
}

// quote.c `sq_dequote_step`: one single-quoted word, where `'\''` and `'\!'` quote those
// characters; returns it and what follows it.
#[cfg(unix)]
fn sq_dequote_step(text: &str) -> Option<(String, &str)> {
    let mut rest = text.strip_prefix('\'')?;
    let mut word = String::new();
    loop {
        let end = rest.find('\'')?;
        word.push_str(&rest[..end]);
        rest = &rest[end + 1..];
        match rest.as_bytes() {
            [b'\\', quoted @ (b'\'' | b'!'), b'\'', ..] => {
                word.push(char::from(*quoted));
                rest = &rest[3..];
            }
            _ => return Some((word, rest)),
        }
    }
}

// config.c `git_parse_maybe_bool`: words or a number; anything else makes git fail.
#[cfg(unix)]
fn git_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" => Some(true),
        "false" | "no" | "off" | "" => Some(false),
        number => number.parse::<i64>().ok().map(|number| number != 0),
    }
}

// git v2.47.0, setup.c `safe_directory_cb`: an empty entry resets, `*` allows all, and any
// other is a path, `~` expanded, absolute or `.`, compared by its real path, where a
// trailing `/*` allows everything below it.
#[cfg(unix)]
fn safe_directory(
    work_tree: &Path,
    entries: &[Option<String>],
    home: Option<&Path>,
    cwd: Option<&Path>,
) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::fs::canonicalize(work_tree) else {
        return false;
    };
    let mut safe = false;
    for entry in entries {
        match entry.as_deref() {
            None | Some("") => safe = false,
            Some("*") => safe = true,
            Some(value) => {
                let Some(allowed) = interpolate_path(value, home) else {
                    continue;
                };
                let allowed = match allowed == Path::new(".") {
                    true => match cwd {
                        Some(cwd) => cwd.to_path_buf(),
                        None => continue,
                    },
                    false if allowed.is_absolute() => allowed,
                    false => continue,
                };
                let Some(normalized) = real_path(&allowed) else {
                    continue;
                };
                let normalized = normalized.as_os_str().as_bytes();
                safe |= match normalized.strip_suffix(b"*") {
                    Some(prefix) if prefix.ends_with(b"/") => {
                        path.as_os_str().as_bytes().starts_with(prefix)
                    }
                    _ => path.as_os_str().as_bytes() == normalized,
                };
            }
        }
    }
    safe
}

// path.c `interpolate_path`: `~` and `~user` name home directories, and `%(prefix)/` git's
// install prefix, which is known here only for an absolute path behind it.
#[cfg(unix)]
fn interpolate_path(value: &str, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(rest) = value.strip_prefix("%(prefix)/") {
        return Path::new(rest).is_absolute().then(|| PathBuf::from(rest));
    }
    let Some(user_path) = value.strip_prefix('~') else {
        return Some(PathBuf::from(value));
    };
    let (user, rest) = user_path.split_at(user_path.find('/').unwrap_or(user_path.len()));
    let dir = match user {
        "" => home?.to_path_buf(),
        user => home_of(user)?,
    };
    let mut expanded = dir.into_os_string();
    expanded.push(rest);
    Some(PathBuf::from(expanded))
}

#[cfg(unix)]
fn home_of(user: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(user).ok()?;
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found = std::ptr::null_mut();
    let status = unsafe {
        libc::getpwnam_r(
            name.as_ptr(),
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut found,
        )
    };
    if status != 0 || found.is_null() || entry.pw_dir.is_null() {
        return None;
    }
    let dir = unsafe { std::ffi::CStr::from_ptr(entry.pw_dir) };
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes())))
}

// abspath.c `strbuf_realpath`: every component resolved, only the last one may be missing.
#[cfg(unix)]
fn real_path(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok().or_else(|| {
        let parent = std::fs::canonicalize(path.parent()?).ok()?;
        Some(parent.join(path.file_name()?))
    })
}

// Where the refs of the repository with git dir `git_dir` live: the common dir its
// `commondir` file names, as a linked worktree's does, or the git dir itself.
pub(crate) fn common_dir_of(git_dir: &Path) -> PathBuf {
    std::fs::read_to_string(git_dir.join("commondir"))
        .ok()
        .map(|target| canonical_dir(&git_dir.join(target.trim())))
        .unwrap_or_else(|| git_dir.to_path_buf())
}

fn canonical_dir(dir: &Path) -> PathBuf {
    crate::path_utils::canonicalize(dir).unwrap_or_else(|_| dir.components().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_watches_follow_only_the_repositories_of_the_listeners_scopes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = crate::path_utils::canonicalize(tmp.path()).unwrap();
        let picker = SharedFilePicker::default();
        for name in ["a", "b"] {
            git2::Repository::init(root.join(name)).unwrap();
            picker.register_git_dir(&root.join(name).join(".git"), &root.join(name));
        }
        let refs = RefWatches::new(&root, false, None, false);
        let a_refs = root.join("a/.git/refs");
        let b_refs = root.join("b/.git/refs");
        assert!(ops(&refs.plan(&picker, None)).is_empty());

        // The git dirs themselves are watched for their state already.
        let only_a = picker.subscribe_ref_changes(&root.join("a/src"), |_| {});
        assert_eq!(ops(&refs.plan(&picker, None)), [(true, a_refs.clone())]);
        assert_eq!(heard(&refs, &b_refs.join("heads/x")), Vec::<PathBuf>::new());
        assert_eq!(heard(&refs, &a_refs.join("heads/x")), [root.join("a")]);

        let everything = picker.subscribe_ref_changes(&root, |_| {});
        assert_eq!(ops(&refs.plan(&picker, None)), [(true, b_refs.clone())]);
        drop(only_a);
        assert!(ops(&refs.plan(&picker, None)).is_empty());
        drop(everything);
        assert_eq!(
            ops(&refs.plan(&picker, None)),
            [(false, a_refs), (false, b_refs)]
        );
        assert!(refs.signals().is_none());
    }

    #[test]
    fn a_ref_store_created_after_its_repository_is_watched_once_it_appears() {
        let tmp = tempfile::tempdir().unwrap();
        let root = crate::path_utils::canonicalize(tmp.path()).unwrap();
        let repo = root.join("repo");
        let git_dir = repo.join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let picker = SharedFilePicker::default();
        picker.register_git_dir(&git_dir, &repo);
        let _listener = picker.subscribe_ref_changes(&root, |_| {});
        let refs = RefWatches::new(&root, false, None, false);

        let plan = refs.plan(&picker, None);
        assert!(ops(&plan).is_empty() && plan.appeared.is_empty());
        let mut changes = RefChanges::default();
        refs.signals()
            .unwrap()
            .classify(&git_dir.join("refs"), modified(), &mut changes);
        assert!(changes.resync, "the ref store's appearance is followed");

        std::fs::create_dir_all(git_dir.join("refs/heads")).unwrap();
        let plan = refs.plan(&picker, None);
        assert_eq!(ops(&plan), [(true, git_dir.join("refs"))]);
        assert_eq!(
            plan.appeared,
            [repo],
            "refs written before the watch changed"
        );
        let plan = refs.plan(&picker, None);
        assert!(ops(&plan).is_empty() && plan.appeared.is_empty());
    }

    #[test]
    fn reftable_repositories_signal_through_their_table_lists() {
        let tmp = tempfile::tempdir().unwrap();
        let root = crate::path_utils::canonicalize(tmp.path()).unwrap();
        let main = root.join("main");
        if let Err(reason) = reftable_repository(&main) {
            eprintln!("skipped: the installed git cannot create a reftable repository: {reason}");
            return;
        }
        git(
            &main,
            &["worktree", "add", "-q", "-b", "linked", "../linked"],
        );
        let linked = root.join("linked");
        let common_dir = main.join(".git");
        let linked_git_dir = crate::watch::git_dir_of(&linked).unwrap();
        assert_eq!(RefStorage::of(&common_dir), RefStorage::Reftable);
        let picker = SharedFilePicker::default();
        picker.register_git_dir(&common_dir, &main);
        picker.register_git_dir(&linked_git_dir, &linked);
        let _listener = picker.subscribe_ref_changes(&root, |_| {});
        let refs = RefWatches::new(&root, false, None, false);

        assert_eq!(
            ops(&refs.plan(&picker, None)),
            [
                (true, common_dir.join("reftable")),
                (true, common_dir.join("worktrees")),
                (true, linked_git_dir.join("reftable")),
            ]
        );
        let tables = common_dir.join("reftable/tables.list");
        assert_eq!(heard(&refs, &tables), [linked.clone(), main.clone()]);
        let own_tables = linked_git_dir.join("reftable/tables.list");
        assert_eq!(heard(&refs, &own_tables), [linked]);
        for unrelated in [
            common_dir.join("reftable/tables.list.lock"),
            common_dir.join("refs/heads"),
            common_dir.join("packed-refs"),
        ] {
            assert!(heard(&refs, &unrelated).is_empty(), "{unrelated:?}");
        }
    }

    #[test]
    fn a_pruned_linked_worktree_is_no_repository_for_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let main = crate::path_utils::canonicalize(tmp.path())
            .unwrap()
            .join("main");
        std::fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "-q", "-b", "main"]);
        git(&main, &["commit", "-q", "--allow-empty", "-m", "init"]);
        git(
            &main,
            &["worktree", "add", "-q", "-b", "wt", ".worktrees/wt"],
        );
        let worktree = main.join(".worktrees/wt");
        std::fs::remove_dir_all(main.join(".git/worktrees/wt")).unwrap();

        // Git stops at the worktree's `.git` file, so the main checkout is not its repository.
        assert!(RefWatches::new(&worktree, false, None, false).own.is_none());
    }

    #[test]
    fn only_a_reftable_repository_is_found_where_git_s_library_finds_none() {
        let tmp = tempfile::tempdir().unwrap();
        let root = crate::path_utils::canonicalize(tmp.path()).unwrap();
        // libgit2 refuses an unknown extension as it refuses reftable; git refuses it too.
        let refused = root.join("refused");
        std::fs::create_dir_all(&refused).unwrap();
        git(&refused, &["init", "-q"]);
        git(&refused, &["config", "core.repositoryformatversion", "1"]);
        git(&refused, &["config", "extensions.fffunknown", "true"]);
        assert!(Repository::open(&refused).is_err());
        assert!(
            RefWatches::new(&refused.join("src"), false, None, false)
                .own
                .is_none()
        );

        let reftable = root.join("reftable");
        if let Err(reason) = reftable_repository(&reftable) {
            eprintln!("skipped: the installed git cannot create a reftable repository: {reason}");
            return;
        }
        std::fs::create_dir(reftable.join("src")).unwrap();
        let own = RefWatches::new(&reftable.join("src"), false, None, false)
            .own
            .unwrap();
        assert_eq!(own.work_tree, reftable);
        assert_eq!(own.common_dir, reftable.join(".git"));
    }

    #[cfg(unix)]
    #[test]
    fn a_reftable_repository_of_another_user_is_refused_as_git_refuses_it() {
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skipped: only root can give a repository to another user");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = crate::path_utils::canonicalize(tmp.path())
            .unwrap()
            .join("repo");
        if let Err(reason) = reftable_repository(&repo) {
            eprintln!("skipped: the installed git cannot create a reftable repository: {reason}");
            return;
        }
        assert!(RefWatches::new(&repo, false, None, false).own.is_some());
        let path = std::ffi::CString::new(repo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::chown(path.as_ptr(), 65534, 65534) }, 0);
        assert!(RefWatches::new(&repo, false, None, false).own.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn ownership_is_the_current_user_s_as_git_decides_it() {
        assert!(owned_by(1000, 1000, None));
        assert!(
            !owned_by(0, 1000, None),
            "root's repository is not another user's"
        );
        assert!(owned_by(0, 0, None));
        assert!(owned_by(1000, 0, Some(1000)), "the user who ran sudo");
        assert!(!owned_by(1000, 0, None));
        assert!(!owned_by(1001, 0, Some(1000)));
        assert_eq!(sudo_uid(Some("1000")), Some(1000));
        for unusable in [Some(""), Some("10x"), Some("-1"), None] {
            assert_eq!(sudo_uid(unusable), None, "{unusable:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn safe_directories_match_as_git_matches_them() {
        let tmp = tempfile::tempdir().unwrap();
        let root = crate::path_utils::canonicalize(tmp.path()).unwrap();
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        std::os::unix::fs::symlink(&repo, root.join("link")).unwrap();
        let display = |path: &Path| Some(path.display().to_string());
        let safe = |entries: &[Option<String>]| safe_directory(&repo, entries, Some(&root), None);

        assert!(safe(&[Some("*".into())]));
        assert!(
            !safe(&[Some("*".into()), Some(String::new())]),
            "an empty entry resets"
        );
        assert!(!safe(&[None]));
        assert!(safe(&[display(&repo)]));
        assert!(
            safe(&[display(&root.join("link"))]),
            "compared by real path"
        );
        assert!(
            safe(&[Some(format!("{}/*", root.display()))]),
            "below a prefix"
        );
        assert!(
            !safe(&[Some(format!("{}/*", repo.display()))]),
            "the prefix itself is not"
        );
        assert!(
            safe(&[Some("~/repo".into())]),
            "the home directory expanded"
        );
        assert!(safe(&[Some(format!("%(prefix)/{}", repo.display()))]));
        assert!(!safe(&[Some("repo".into())]), "a relative entry");
        assert!(!safe(&[Some(format!("{}/other", root.display()))]));
        assert!(safe_directory(
            &repo,
            &[Some(".".into())],
            None,
            Some(&repo)
        ));
        assert!(!safe_directory(
            &repo,
            &[Some(".".into())],
            None,
            Some(&root)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn safe_directories_come_from_git_s_protected_config() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let config = |name: &str, dirs: &[&str]| {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let lines: String = dirs
                .iter()
                .map(|dir| format!("\tdirectory = {dir}\n"))
                .collect();
            std::fs::write(&path, format!("[safe]\n{lines}")).unwrap();
            path.display().to_string()
        };
        let system = config("etc/gitconfig", &["/system"]);
        let named_system = config("named-system", &["/named-system"]);
        let global = config("global", &["/global"]);
        config("home/.gitconfig", &["/home"]);
        config("home/.config/git/config", &["/xdg"]);
        let home = root.join("home").display().to_string();
        let read = |vars: &[(&str, &str)]| {
            let vars: Vec<(String, String)> = (vars.iter())
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect();
            let env = |name: &str| {
                (vars.iter())
                    .find(|(known, _)| known == name)
                    .map(|(_, value)| value.clone())
            };
            safe_directories(&env, Some(PathBuf::from(&system)))
        };
        let values = |dirs: &[&str]| Some(dirs.iter().map(|dir| Some(dir.to_string())).collect());

        assert_eq!(
            read(&[("HOME", &home)]),
            values(&["/system", "/xdg", "/home"])
        );
        assert_eq!(
            read(&[("HOME", &home), ("GIT_CONFIG_GLOBAL", &global)]),
            values(&["/system", "/global"]),
            "GIT_CONFIG_GLOBAL replaces both global files"
        );
        assert_eq!(
            read(&[("HOME", &home), ("GIT_CONFIG_SYSTEM", &named_system)]),
            values(&["/named-system", "/xdg", "/home"])
        );
        assert_eq!(
            read(&[("HOME", &home), ("GIT_CONFIG_NOSYSTEM", "yes")]),
            values(&["/xdg", "/home"])
        );
        assert_eq!(read(&[("GIT_CONFIG_NOSYSTEM", "0")]), values(&["/system"]));
        assert_eq!(read(&[("GIT_CONFIG_NOSYSTEM", "maybe")]), None, "git fails");

        let command_line = read(&[
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_COUNT", "2"),
            ("GIT_CONFIG_KEY_0", "Safe.Directory"),
            ("GIT_CONFIG_VALUE_0", "/count"),
            ("GIT_CONFIG_KEY_1", "core.editor"),
            ("GIT_CONFIG_VALUE_1", "vi"),
            (
                "GIT_CONFIG_PARAMETERS",
                r"'safe.directory'='/a b' 'core.x'='y' 'safe.directory=/it'\''s' 'safe.directory'=",
            ),
        ]);
        let expected = vec![
            Some("/count".to_string()),
            Some("/a b".to_string()),
            Some("/it's".to_string()),
            None,
        ];
        assert_eq!(command_line, Some(expected));
        for bogus in [
            vec![("GIT_CONFIG_COUNT", "1")],
            vec![("GIT_CONFIG_COUNT", "x")],
            vec![("GIT_CONFIG_PARAMETERS", "safe.directory=/x")],
            vec![("GIT_CONFIG_PARAMETERS", "'safe.directory'='/x'y")],
            vec![("GIT_CONFIG_PARAMETERS", "'safe.directory")],
        ] {
            let mut vars = vec![("GIT_CONFIG_NOSYSTEM", "1")];
            vars.extend(bogus.iter().copied());
            assert_eq!(read(&vars), None, "{bogus:?}");
        }
    }

    #[test]
    fn the_set_of_linked_worktrees_and_their_locations_are_ref_state() {
        let tmp = tempfile::tempdir().unwrap();
        let root = crate::path_utils::canonicalize(tmp.path()).unwrap();
        let repo = root.join("repo");
        git2::Repository::init(&repo).unwrap();
        let worktrees = repo.join(".git/worktrees");
        std::fs::create_dir_all(worktrees.join("wt")).unwrap();
        let picker = SharedFilePicker::default();
        picker.register_git_dir(&repo.join(".git"), &repo);
        let _listener = picker.subscribe_ref_changes(&root, |_| {});
        let refs = RefWatches::new(&root, false, None, false);
        let watched: Vec<_> = ops(&refs.plan(&picker, None))
            .into_iter()
            .map(|(_, path)| path)
            .collect();
        assert!(watched.contains(&worktrees) && watched.contains(&worktrees.join("wt")));

        // Where watches stay, one recursive watch of `worktrees` serves every admin dir.
        let kept = RefWatches::new(&root, false, None, true);
        let kept_ops = kept.plan(&picker, None).ops;
        let recursive = |path: &Path| {
            (kept_ops.iter())
                .find(|op| op.as_path() == path)
                .map(|op| match op {
                    PathOp::Watch(_, config) => config.recursive_mode() == RecursiveMode::Recursive,
                    PathOp::Unwatch(_) => false,
                })
        };
        assert_eq!(recursive(&worktrees), Some(true));
        assert_eq!(recursive(&worktrees.join("wt")), None);

        let created = EventKind::Create(notify::event::CreateKind::Folder);
        let removed = EventKind::Remove(notify::event::RemoveKind::Any);
        let classify = |path: &Path, kind| {
            let mut changes = RefChanges::default();
            refs.signals().unwrap().classify(path, kind, &mut changes);
            (
                changes.work_trees.into_iter().collect::<Vec<_>>(),
                changes.resync,
            )
        };
        assert_eq!(
            classify(&worktrees.join("wt"), created),
            (vec![repo.clone()], true)
        );
        assert_eq!(
            classify(&worktrees.join("gone"), removed),
            (vec![repo.clone()], true)
        );
        assert_eq!(classify(&worktrees, created), (vec![repo.clone()], true));
        assert_eq!(
            classify(&worktrees.join("wt/locked"), removed),
            (vec![repo.clone()], false)
        );
        assert_eq!(
            classify(&worktrees.join("wt/gitdir"), modified()),
            (vec![repo.clone()], false)
        );
        // A worktree's own files, and its admin dir touched by them, are no change of the set.
        assert_eq!(classify(&worktrees.join("wt"), modified()), (vec![], false));
        for own in ["wt/HEAD", "wt/index", "wt/gitdir.lock", "wt/logs/HEAD"] {
            assert_eq!(
                classify(&worktrees.join(own), modified()),
                (vec![], false),
                "{own}"
            );
        }
    }

    #[test]
    fn other_worktrees_admin_files_outside_the_index_are_dropped_by_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let root = crate::path_utils::canonicalize(tmp.path()).unwrap();
        let repo = root.join("repo");
        let base = root.join("index");
        let repository = git2::Repository::init(&repo).unwrap();
        let signature = git2::Signature::now("fff", "fff@example.invalid").unwrap();
        let tree = repository
            .find_tree(repository.index().unwrap().write_tree().unwrap())
            .unwrap();
        repository
            .commit(Some("HEAD"), &signature, &signature, "init", &tree, &[])
            .unwrap();
        std::fs::create_dir(&base).unwrap();
        repository.worktree("wt", &base.join("wt"), None).unwrap();
        repository
            .worktree("other", &root.join("other"), None)
            .unwrap();
        let worktrees = repo.join(".git/worktrees");
        let picker = SharedFilePicker::default();
        picker.register_git_dir(&worktrees.join("wt"), &base.join("wt"));
        let _listener = picker.subscribe_ref_changes(&base, |_| {});

        // With FSEvents' one recursive watch and with one watch per admin dir alike.
        for keep in [true, false] {
            let refs = RefWatches::new(&base, false, None, keep);
            refs.plan(&picker, None);
            let signals = refs.signals().unwrap();
            let dropped = |path: &Path| signals.foreign_admin_file(path, &base);
            for busy in [
                "other/index",
                "other/HEAD",
                "other/index.lock",
                "other/gitdir.lock",
            ] {
                assert!(dropped(&worktrees.join(busy)), "{busy}, keep {keep}");
            }
            if keep {
                assert!(dropped(&worktrees.join("other/logs/HEAD")));
            }
            for kept in [
                "other",
                "other/gitdir",
                "other/locked",
                "added",
                "wt/HEAD",
                "wt/index",
            ] {
                assert!(!dropped(&worktrees.join(kept)), "{kept}, keep {keep}");
            }
            assert!(!dropped(&worktrees));
            assert!(!dropped(&repo.join(".git/refs/heads/main")));
            assert!(!dropped(&base.join("wt/file.txt")));
        }
    }

    #[test]
    fn directories_named_worktrees_are_no_admin_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = crate::path_utils::canonicalize(tmp.path()).unwrap();
        let commit = |repo: &git2::Repository| {
            let signature = git2::Signature::now("fff", "fff@example.invalid").unwrap();
            let tree = repo
                .find_tree(repo.index().unwrap().write_tree().unwrap())
                .unwrap();
            repo.commit(Some("HEAD"), &signature, &signature, "init", &tree, &[])
                .unwrap();
        };
        // A main checkout in a directory named `worktrees`, and an index of its linked worktree
        // in another directory of that name.
        let main = root.join("worktrees");
        let repository = git2::Repository::init(&main).unwrap();
        commit(&repository);
        std::fs::create_dir(root.join("elsewhere")).unwrap();
        let linked = root.join("elsewhere/worktrees");
        repository.worktree("linked", &linked, None).unwrap();
        repository
            .worktree("other", &root.join("other"), None)
            .unwrap();
        let common_dir = main.join(".git");
        let picker = SharedFilePicker::default();
        let _listener = picker.subscribe_ref_changes(&linked, |_| {});

        for keep in [true, false] {
            let refs = RefWatches::new(&linked, false, Some(&linked), keep);
            refs.plan(&picker, None);
            let signals = refs.signals().unwrap();
            let dropped = |path: &Path| signals.foreign_admin_file(path, &linked);
            for shared in [
                "packed-refs",
                "config",
                "HEAD",
                "refs/heads/main",
                "worktrees",
            ] {
                assert!(!dropped(&common_dir.join(shared)), "{shared}, keep {keep}");
            }
            for own in [
                "worktrees/linked/HEAD",
                "worktrees/linked/index",
                "worktrees/other/gitdir",
            ] {
                assert!(!dropped(&common_dir.join(own)), "{own}, keep {keep}");
            }
            assert!(
                dropped(&common_dir.join("worktrees/other/index")),
                "keep {keep}"
            );
        }
    }

    fn modified() -> EventKind {
        EventKind::Modify(ModifyKind::Any)
    }

    fn ops(plan: &RefPlan) -> Vec<(bool, PathBuf)> {
        let mut ops: Vec<_> = (plan.ops.iter())
            .map(|op| (matches!(op, PathOp::Watch(..)), op.as_path().to_path_buf()))
            .collect();
        ops.sort();
        ops
    }

    fn heard(refs: &RefWatches, path: &Path) -> Vec<PathBuf> {
        let mut changes = RefChanges::default();
        refs.signals()
            .unwrap()
            .classify(path, modified(), &mut changes);
        let mut heard: Vec<_> = changes.work_trees.into_iter().collect();
        heard.sort();
        heard
    }

    fn reftable_repository(dir: &Path) -> Result<(), String> {
        let output = std::process::Command::new("git")
            .args(["init", "-q", "--ref-format=reftable", "-b", "main"])
            .arg(dir)
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }
        git(dir, &["commit", "-q", "--allow-empty", "-m", "init"]);
        Ok(())
    }

    fn git(dir: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=fff",
                "-c",
                "user.email=fff@example.invalid",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
