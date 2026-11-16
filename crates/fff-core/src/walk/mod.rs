//! Filesystem traversal backend. Selects one implementation at compile time:
//! - `zlob`: zlob's native parallel walker (requires the Zig toolchain).
//! - `ripgrep`: the `ignore` crate (ripgrep's walker), used by default.
//!
//! Both expose [`walk_collect_files`] with identical semantics so the rest of
//! the crate stays backend-agnostic, including walks limited to a subtree.

use crate::file_picker::FilePickerIgnoreOptions;
use crate::types::FileItem;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

mod rules;
use rules::IgnoreSources;
pub(crate) use rules::{IgnoreRules, IgnoreSource};

#[cfg(feature = "zlob")]
mod zlob;
#[cfg(feature = "zlob")]
use zlob::walk_files;

#[cfg(all(not(feature = "zlob"), feature = "ripgrep"))]
mod ripgrep;
#[cfg(all(not(feature = "zlob"), feature = "ripgrep"))]
use ripgrep::walk_files;

// Each directory's ignore files are hashed as the walker reaches it, before it reads them and on
// its own threads: no second pass, no shared pool thread. Fails once `hooks.cancelled` is set.
pub(crate) fn walk_collect_files(
    base_path: &Path,
    subtree: Option<&Path>,
    is_git_repo: bool,
    policy: WalkPolicy<'_>,
    threads: usize,
    synced_files_count: &Arc<AtomicUsize>,
    hooks: WalkHooks<'_>,
) -> crate::Result<WalkOutput> {
    let sources = Arc::new(IgnoreSources::new(policy.ignore));
    let walk_root = subtree.unwrap_or(base_path);
    sources.read_in(walk_root);
    if is_git_repo
        && !policy.ignore.disable_git
        && let Some(exclude) = exclude_file_of(base_path, walk_root)
    {
        sources.read_file(exclude);
    }
    let on_dir: DirHook = {
        let (sources, on_dir) = (Arc::clone(&sources), hooks.on_dir.cloned());
        Arc::new(move |dir: &Path| {
            if let Some(on_dir) = &on_dir {
                on_dir(dir);
            }
            sources.read_in(dir);
        })
    };
    let walk_hooks = WalkHooks::new(Some(&on_dir), hooks.cancelled);
    let walked = walk_files(
        base_path,
        subtree,
        is_git_repo,
        policy,
        threads,
        synced_files_count,
        walk_hooks,
    );
    if hooks.cancelled() {
        return Err(crate::Error::WalkFailed("the walk was cancelled".into()));
    }
    let mut output = walked?;
    output.ignore_sources = sources.finish();
    Ok(output)
}

// The exclude file of the repository whose whole work tree within the index the walk covers:
// the index's own for a walk of the root, a nested one's for a walk of its work tree.
fn exclude_file_of(base_path: &Path, walk_root: &Path) -> Option<PathBuf> {
    let common_dir = if walk_root == base_path {
        git2::Repository::discover(base_path)
            .ok()?
            .commondir()
            .to_path_buf()
    } else {
        crate::watch::common_dir_of(&crate::watch::git_dir_of(walk_root)?)
    };
    Some(common_dir.join("info").join("exclude"))
}

/// A walk limited to `subtree` still traverses from `base_path`, so the root's
/// ignore and exclusion policy applies unchanged. Returns the subtree's path
/// relative to the base, or an error when it lies outside.
fn subtree_relative<'a>(base_path: &Path, subtree: &'a Path) -> crate::Result<&'a Path> {
    subtree.strip_prefix(base_path).map_err(|_| {
        crate::Error::WalkFailed(format!(
            "{} is not under the walk root {}",
            subtree.display(),
            base_path.display()
        ))
    })
}

// What every walk of an index keeps besides the repository's own rules.
#[derive(Clone, Copy)]
pub(crate) struct WalkPolicy<'a> {
    pub(crate) follow_symlinks: bool,
    pub(crate) ignore: &'a FilePickerIgnoreOptions,
}

/// Receives the absolute path of every directory the walk reports, before the
/// walker opens it or reads its entries and ignore files.
pub(crate) type DirHook = Arc<dyn Fn(&Path) + Send + Sync>;

// What a walk reports while it runs, and what stops it.
#[derive(Clone, Copy, Default)]
pub(crate) struct WalkHooks<'a> {
    on_dir: Option<&'a DirHook>,
    cancelled: Option<&'a Arc<AtomicBool>>,
}

impl<'a> WalkHooks<'a> {
    pub(crate) fn new(on_dir: Option<&'a DirHook>, cancelled: Option<&'a Arc<AtomicBool>>) -> Self {
        Self { on_dir, cancelled }
    }

    pub(crate) fn cancelled(&self) -> bool {
        (self.cancelled).is_some_and(|cancelled| cancelled.load(Ordering::Relaxed))
    }
}

pub(crate) struct WalkOutput {
    pub(crate) pairs: Vec<(FileItem, String)>,
    /// Every non-ignored directory the walk visited, relative, ending with /
    pub(crate) dirs: Vec<String>,
    pub(crate) ignore_rules: Option<WalkIgnoreRules>,
    // What each ignore file at or below the walked directory held when the walk read it.
    pub(crate) ignore_sources: Vec<(PathBuf, IgnoreSource)>,
}

#[cfg_attr(not(feature = "zlob"), derive(Clone))]
pub(crate) struct WalkIgnoreRules {
    #[cfg(feature = "zlob")]
    inner: ::zlob::walk::WalkerOutcomeRules,
    #[cfg(feature = "zlob")]
    skip_hidden: bool,
    // The walk's root, which the rules' paths are relative to.
    #[cfg(feature = "zlob")]
    root: std::path::PathBuf,
    #[cfg(not(feature = "zlob"))]
    inner: ::ignore::IncrementalIgnore,
}

// SAFETY: the underlying storage is immutable, heap-owned, and thread-safe to
// read from concurrently (mirrors zlob's `IgnoreRules: Send + Sync`).
#[cfg(feature = "zlob")]
unsafe impl Send for WalkIgnoreRules {}
#[cfg(feature = "zlob")]
unsafe impl Sync for WalkIgnoreRules {}

impl std::fmt::Debug for WalkIgnoreRules {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WalkIgnoreRules")
    }
}

impl WalkIgnoreRules {
    #[cfg(feature = "zlob")]
    pub(crate) fn is_ignored(&self, relative_path: &Path, _is_dir: bool) -> bool {
        if self.skip_hidden
            && relative_path
                .iter()
                .any(|name| name.as_encoded_bytes().starts_with(b"."))
        {
            return true;
        }
        let Some(rules) = self.inner.rules() else {
            return false;
        };
        if !rules.is_ignored(relative_path) {
            return false;
        }
        // zlob takes a path that is gone for ignored: such a path is ignored only below a
        // directory that is, the nearest one still there deciding.
        (relative_path.ancestors())
            .filter(|path| !path.as_os_str().is_empty())
            .find(|path| self.root.join(path).symlink_metadata().is_ok())
            .is_some_and(|path| path == relative_path || rules.is_ignored(path))
    }

    #[cfg(not(feature = "zlob"))]
    pub(crate) fn is_ignored(&mut self, relative_path: &Path, is_dir: bool) -> bool {
        self.inner.matched(relative_path, is_dir).is_ignore()
    }
}

#[cfg(test)]
mod tests {
    use super::{WalkHooks, WalkPolicy, walk_collect_files};
    use crate::file_picker::FilePickerIgnoreOptions;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, LazyLock};

    // Backend-agnostic parity check: both the zlob and ripgrep walkers must
    // respect .gitignore, skip hidden files in a git repo, and surface the
    // expected file set with a correct synced count.
    #[test]
    fn collects_files_respecting_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir(root.join("src")).unwrap();
        fs::create_dir(root.join("target")).unwrap();
        fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
        fs::write(root.join("Cargo.toml"), "x").unwrap();
        fs::write(root.join("debug.log"), "").unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("target/out.bin"), "bin").unwrap();

        let counter = Arc::new(AtomicUsize::new(0));
        let out = walk_collect_files(
            root,
            None,
            true,
            policy(false),
            1,
            &counter,
            WalkHooks::default(),
        )
        .unwrap();

        let mut names: Vec<String> = out.pairs.into_iter().map(|(_, rel)| rel).collect();
        names.sort();

        assert!(names.contains(&"Cargo.toml".to_string()));
        assert!(names.iter().any(|n| n.ends_with("main.rs")));
        // target/ and *.log are gitignored; .git/ is skipped.
        assert!(!names.iter().any(|n| n.contains("target")));
        assert!(!names.iter().any(|n| n.ends_with(".log")));
        assert!(!names.iter().any(|n| n.contains(".git/")));
        assert_eq!(counter.load(Ordering::Relaxed), names.len());
    }

    #[test]
    fn parallel_walk_preserves_files_directories_and_count() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        for directory in 0..64 {
            let path = root.join(format!("src{directory}"));
            fs::create_dir(&path).unwrap();
            for file in 0..32 {
                fs::write(path.join(format!("file{file}.rs")), "fn main() {}\n").unwrap();
            }
        }
        let mut results = Vec::new();
        for threads in [1, 4] {
            let counter = Arc::new(AtomicUsize::new(0));
            let out = walk_collect_files(
                root,
                None,
                true,
                policy(false),
                threads,
                &counter,
                WalkHooks::default(),
            )
            .unwrap();
            let mut files: Vec<_> = out
                .pairs
                .into_iter()
                .map(|(item, path)| (path, item.size))
                .collect();
            files.sort();
            let mut directories = out.dirs;
            directories.sort();
            results.push((files, directories, counter.load(Ordering::Relaxed)));
        }
        assert_eq!(results[0], results[1]);
        assert_eq!(results[1].2, 64 * 32);
    }

    #[test]
    fn skips_git_metadata_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("module")).unwrap();
        fs::write(root.join(".git"), "gitdir: ../admin\n").unwrap();
        fs::write(
            root.join("module/.git"),
            "gitdir: ../../admin/modules/lib\n",
        )
        .unwrap();
        fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("module/lib.rs"), "pub fn lib() {}\n").unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let out = walk_collect_files(
            root,
            None,
            true,
            policy(false),
            4,
            &counter,
            WalkHooks::default(),
        )
        .unwrap();
        let mut paths: Vec<_> = out.pairs.into_iter().map(|(_, path)| path).collect();
        paths.sort();
        assert_eq!(
            (paths, counter.load(Ordering::Relaxed)),
            (vec!["main.rs".to_string(), "module/lib.rs".to_string()], 2)
        );
    }

    // Non-git roots prune known non-code directories (node_modules).
    #[test]
    fn prunes_non_code_dirs_for_non_git_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("node_modules")).unwrap();
        fs::write(root.join("node_modules/lib.js"), "x").unwrap();
        fs::write(root.join("index.js"), "x").unwrap();

        let counter = Arc::new(AtomicUsize::new(0));
        let out = walk_collect_files(
            root,
            None,
            false,
            policy(false),
            1,
            &counter,
            WalkHooks::default(),
        )
        .unwrap();
        let names: Vec<String> = out.pairs.into_iter().map(|(_, rel)| rel).collect();

        assert!(names.iter().any(|n| n.ends_with("index.js")));
        assert!(!names.iter().any(|n| n.contains("node_modules")));
    }

    // Subtree walks keep the root's ignore rules and report base-relative paths.
    #[test]
    fn subtree_walk_keeps_root_rules() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("sub/ignored")).unwrap();
        fs::create_dir_all(root.join("sub/kept")).unwrap();
        fs::create_dir_all(root.join("other")).unwrap();
        fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
        fs::write(root.join("sub/kept/a.rs"), "x").unwrap();
        fs::write(root.join("sub/ignored/b.rs"), "x").unwrap();
        fs::write(root.join("other/c.rs"), "x").unwrap();

        let counter = Arc::new(AtomicUsize::new(0));
        let out = walk_collect_files(
            root,
            Some(&root.join("sub")),
            true,
            policy(false),
            1,
            &counter,
            WalkHooks::default(),
        )
        .unwrap();
        let names: Vec<String> = out.pairs.into_iter().map(|(_, rel)| rel).collect();
        assert_eq!(names, vec!["sub/kept/a.rs".to_string()]);
        let mut dirs = out.dirs;
        dirs.sort();
        assert_eq!(dirs, vec!["sub/".to_string(), "sub/kept/".to_string()]);
    }

    #[test]
    fn subtree_outside_the_root_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let result = walk_collect_files(
            dir.path(),
            Some(other.path()),
            false,
            policy(false),
            1,
            &counter,
            WalkHooks::default(),
        );
        assert!(result.is_err());
    }

    // The hook sees every reported directory before the walker reads it: an ignore
    // file and an entry the hook creates both take effect in the same walk.
    #[test]
    fn dir_hook_runs_before_each_directory_is_read() {
        for threads in [1, 4] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            fs::create_dir(root.join(".git")).unwrap();
            fs::create_dir_all(root.join("a/b")).unwrap();
            fs::create_dir_all(root.join("ignored/c")).unwrap();
            fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
            fs::write(root.join("a/hidden_by_hook.rs"), "x").unwrap();
            fs::write(root.join("a/b/hidden_by_hook.rs"), "x").unwrap();

            let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
            let hook: super::DirHook = {
                let seen = Arc::clone(&seen);
                let root = root.to_path_buf();
                Arc::new(move |path: &std::path::Path| {
                    let _ = fs::write(path.join(".gitignore"), "hidden_by_hook.rs\n");
                    let _ = fs::write(path.join("from_hook.rs"), "x");
                    if let Ok(relative) = path.strip_prefix(&root) {
                        seen.lock().push(relative.to_path_buf());
                    }
                })
            };
            let counter = Arc::new(AtomicUsize::new(0));
            let out = walk_collect_files(
                root,
                None,
                true,
                policy(false),
                threads,
                &counter,
                WalkHooks::new(Some(&hook), None),
            )
            .unwrap();
            let mut names: Vec<String> = out.pairs.into_iter().map(|(_, rel)| rel).collect();
            names.sort();
            assert_eq!(
                names,
                [
                    ".gitignore",
                    "a/.gitignore",
                    "a/b/.gitignore",
                    "a/b/from_hook.rs",
                    "a/from_hook.rs"
                ]
            );
            let mut seen_dirs = seen.lock().clone();
            seen_dirs.sort();
            assert_eq!(
                seen_dirs,
                [std::path::Path::new("a"), std::path::Path::new("a/b")]
            );

            seen.lock().clear();
            let subtree = root.join("a/b");
            walk_collect_files(
                root,
                Some(&subtree),
                true,
                policy(false),
                1,
                &counter,
                WalkHooks::new(Some(&hook), None),
            )
            .unwrap();
            assert_eq!(*seen.lock(), [std::path::Path::new("a/b")]);
        }
    }

    // The walker may read an ignore file between its hash and a write restoring the content.
    #[test]
    fn an_ignore_file_the_walk_read_changed_is_recorded_as_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir(root.join("a")).unwrap();
        fs::write(root.join(".gitignore"), "one/\n").unwrap();
        let ignore = root.join(".gitignore");
        let hook: super::DirHook = {
            let ignore = ignore.clone();
            Arc::new(move |_: &std::path::Path| {
                let _ = fs::write(&ignore, "two/\n");
            })
        };
        let counter = Arc::new(AtomicUsize::new(0));
        let out = walk_collect_files(
            root,
            None,
            true,
            policy(false),
            1,
            &counter,
            WalkHooks::new(Some(&hook), None),
        )
        .unwrap();
        let recorded = (out.ignore_sources.into_iter())
            .find(|(path, _)| *path == ignore)
            .map(|(_, source)| source);
        fs::write(&ignore, "one/\n").unwrap();
        let long_ago = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
        let file = fs::File::options().write(true).open(&ignore).unwrap();
        file.set_modified(long_ago).unwrap();
        assert!(super::IgnoreSource::changed(recorded, &ignore));
    }

    // Stow-style file symlinks pointing outside the root must be indexed with
    // follow_symlinks on, with the target's metadata (#912).
    #[cfg(unix)]
    #[test]
    fn follows_file_symlinks_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("dotfiles");
        let root = dir.path().join("config");
        fs::create_dir_all(outside.join("nested")).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(outside.join("sesh.toml"), "0123456789").unwrap();
        fs::write(outside.join("nested/inner.toml"), "x").unwrap();
        fs::write(root.join("projects.toml"), "x").unwrap();
        std::os::unix::fs::symlink("../dotfiles/sesh.toml", root.join("sesh.toml")).unwrap();
        std::os::unix::fs::symlink("../dotfiles/nested", root.join("linked")).unwrap();
        std::os::unix::fs::symlink("../dotfiles/missing", root.join("broken")).unwrap();

        let counter = Arc::new(AtomicUsize::new(0));
        let out = walk_collect_files(
            &root,
            None,
            false,
            policy(true),
            1,
            &counter,
            WalkHooks::default(),
        )
        .unwrap();
        let mut names: Vec<String> = out.pairs.iter().map(|(_, rel)| rel.clone()).collect();
        names.sort();

        assert_eq!(names, ["linked/inner.toml", "projects.toml", "sesh.toml"]);
        assert_eq!(counter.load(Ordering::Relaxed), names.len());

        let (item, rel) = out.pairs.iter().find(|(_, r)| r == "sesh.toml").unwrap();
        assert_eq!(item.size, 10);
        assert_eq!(&rel[item.filename_offset_in_relative_path()..], "sesh.toml");

        let out = walk_collect_files(
            &root,
            None,
            false,
            policy(false),
            1,
            &counter,
            WalkHooks::default(),
        )
        .unwrap();
        let names: Vec<String> = out.pairs.into_iter().map(|(_, rel)| rel).collect();
        assert_eq!(names, ["projects.toml"]);
    }

    #[cfg(feature = "zlob")]
    #[test]
    fn surfaces_reusable_ignore_rules() {
        use std::path::Path;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
        fs::write(root.join("Cargo.toml"), "x").unwrap();

        let counter = Arc::new(AtomicUsize::new(0));
        let out = walk_collect_files(
            root,
            None,
            true,
            policy(false),
            1,
            &counter,
            WalkHooks::default(),
        )
        .unwrap();

        let rules = out.ignore_rules.expect("zlob surfaces ignore rules");
        fs::create_dir(root.join("target")).unwrap();
        fs::write(root.join("debug.log"), "").unwrap();
        assert!(rules.is_ignored(Path::new("target/"), true));
        assert!(rules.is_ignored(Path::new("debug.log"), false));
        assert!(!rules.is_ignored(Path::new("Cargo.toml"), false));
        // A path that is gone is ignored only below a directory that is.
        assert!(!rules.is_ignored(Path::new("gone.log"), false));
        assert!(rules.is_ignored(Path::new("target/gone.o"), false));
    }

    // Both backends must honor `info/exclude`, which linked worktrees read from
    // the common git dir (#982).
    #[test]
    fn honors_git_info_exclude() {
        use std::path::Path;
        use std::process::Command;

        let git = |dir: &Path, args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("excluded")).unwrap();
        fs::write(root.join("src/a.txt"), "a").unwrap();
        fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "init"]);
        fs::write(root.join("excluded/b.txt"), "b").unwrap();
        fs::write(root.join(".git/info/exclude"), "# comment\nexcluded/\n").unwrap();

        let worktree = dir.path().join("wt");
        git(
            &root,
            &["worktree", "add", "-q", worktree.to_str().unwrap()],
        );
        fs::create_dir_all(worktree.join("excluded")).unwrap();
        fs::write(worktree.join("excluded/c.txt"), "c").unwrap();

        for base in [&root, &worktree] {
            let counter = Arc::new(AtomicUsize::new(0));
            let out = walk_collect_files(
                base,
                None,
                true,
                policy(false),
                1,
                &counter,
                WalkHooks::default(),
            )
            .unwrap();
            let names: Vec<String> = out.pairs.iter().map(|(_, rel)| rel.clone()).collect();

            assert!(names.iter().any(|n| n == "src/a.txt"), "{names:?}");
            assert!(
                !names.iter().any(|n| n.starts_with("excluded")),
                "{names:?}"
            );

            // The watcher filters incremental updates with these rules.
            #[cfg(feature = "zlob")]
            {
                let rules = out.ignore_rules.expect("zlob surfaces ignore rules");
                fs::create_dir_all(base.join("excluded/later")).unwrap();
                fs::write(base.join("excluded/later/new.txt"), "x").unwrap();
                fs::write(base.join("debug.log"), "").unwrap();
                assert!(rules.is_ignored(Path::new("excluded/later/new.txt"), false));
                assert!(rules.is_ignored(Path::new("debug.log"), false));
                assert!(!rules.is_ignored(Path::new("src/a.txt"), false));
            }
        }
    }

    // zlob 1.8 reads core.excludesFile below the root .gitignore, as git does (#874).
    #[cfg(feature = "zlob")]
    #[test]
    fn honors_git_excludes_file_with_git_precedence() {
        use std::process::Command;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join(".worktrees/wt1")).unwrap();
        fs::write(root.join("src/hit.ts"), "x").unwrap();
        fs::write(root.join(".worktrees/wt1/hit.ts"), "x").unwrap();
        fs::write(root.join("drop.tmp"), "x").unwrap();
        fs::write(root.join("keep.tmp"), "x").unwrap();
        fs::write(root.join(".gitignore"), "!keep.tmp\n").unwrap();

        let excludes = dir.path().join("excludes");
        fs::write(&excludes, "# comment\n.worktrees/\n*.tmp\nexcludes\n").unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "core.excludesFile", excludes.to_str().unwrap()],
        ] {
            let status = Command::new("git")
                .args(args)
                .current_dir(root)
                .status()
                .unwrap();
            assert!(status.success());
        }

        let counter = Arc::new(AtomicUsize::new(0));
        let out = walk_collect_files(
            root,
            None,
            true,
            policy(false),
            1,
            &counter,
            WalkHooks::default(),
        )
        .unwrap();
        let names: Vec<String> = out.pairs.iter().map(|(_, rel)| rel.clone()).collect();

        assert!(names.iter().any(|n| n == "src/hit.ts"), "{names:?}");
        assert!(names.iter().any(|n| n == "keep.tmp"), "{names:?}");
        assert!(!names.iter().any(|n| n == "drop.tmp"), "{names:?}");
        assert!(!names.iter().any(|n| n.contains(".worktrees")), "{names:?}");

        // The watcher filters incremental updates with these rules.
        let rules = out.ignore_rules.expect("zlob surfaces ignore rules");
        fs::create_dir_all(root.join(".worktrees/wt1")).unwrap();
        fs::write(root.join(".worktrees/wt1/new.ts"), "x").unwrap();
        assert!(rules.is_ignored(std::path::Path::new(".worktrees/wt1/new.ts"), false));
        assert!(!rules.is_ignored(std::path::Path::new("src/hit.ts"), false));
    }

    // disable_git must also turn off the git exclude files, not only .gitignore.
    #[cfg(feature = "zlob")]
    #[test]
    fn disable_git_skips_every_git_ignore_source() {
        use std::process::Command;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        for name in ["from-gitignore", "from-exclude", "from-global", "from-dot"] {
            fs::write(root.join(name), "x").unwrap();
        }
        fs::write(root.join(".gitignore"), "from-gitignore\n").unwrap();
        fs::write(root.join(".ignore"), "from-dot\n").unwrap();
        let excludes = dir.path().join("excludes");
        fs::write(&excludes, "from-global\n").unwrap();

        for args in [
            vec!["init", "-q"],
            vec!["config", "core.excludesFile", excludes.to_str().unwrap()],
        ] {
            let status = Command::new("git")
                .args(args)
                .current_dir(&root)
                .status()
                .unwrap();
            assert!(status.success());
        }
        fs::write(root.join(".git/info/exclude"), "from-exclude\n").unwrap();

        let walk = |disable_git| -> Vec<String> {
            let options = FilePickerIgnoreOptions {
                disable_git,
                ..Default::default()
            };
            let counter = Arc::new(AtomicUsize::new(0));
            walk_collect_files(
                &root,
                None,
                true,
                WalkPolicy {
                    follow_symlinks: false,
                    ignore: &options,
                },
                1,
                &counter,
                WalkHooks::default(),
            )
            .unwrap()
            .pairs
            .into_iter()
            .map(|(_, rel)| rel)
            .collect()
        };

        let enabled = walk(false);
        for name in ["from-gitignore", "from-exclude", "from-global", "from-dot"] {
            assert!(!enabled.iter().any(|n| n == name), "{name}: {enabled:?}");
        }

        let disabled = walk(true);
        for name in ["from-gitignore", "from-exclude", "from-global"] {
            assert!(disabled.iter().any(|n| n == name), "{name}: {disabled:?}");
        }
        assert!(!disabled.iter().any(|n| n == "from-dot"), "{disabled:?}");
        assert!(
            !disabled.iter().any(|n| n.starts_with(".git/")),
            "{disabled:?}"
        );
    }

    fn policy(follow_symlinks: bool) -> WalkPolicy<'static> {
        static NO_RULES: LazyLock<FilePickerIgnoreOptions> = LazyLock::new(Default::default);
        WalkPolicy {
            follow_symlinks,
            ignore: &NO_RULES,
        }
    }
}
