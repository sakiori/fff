//! Filesystem traversal backend. Selects one implementation at compile time:
//! - `zlob`: zlob's native parallel walker (requires the Zig toolchain).
//! - `ripgrep`: the `ignore` crate (ripgrep's walker), used by default.
//!
//! Both expose [`walk_collect_files`] with identical semantics so the rest of
//! the crate stays backend-agnostic, including walks limited to a subtree.

use crate::file_picker::FilePickerIgnoreOptions;
use crate::types::FileItem;
use std::path::Path;
use std::sync::Arc;

#[cfg(feature = "zlob")]
mod zlob;
#[cfg(feature = "zlob")]
pub(crate) use zlob::walk_collect_files;

#[cfg(all(not(feature = "zlob"), feature = "ripgrep"))]
mod ripgrep;
#[cfg(all(not(feature = "zlob"), feature = "ripgrep"))]
pub(crate) use ripgrep::walk_collect_files;

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

pub(crate) struct WalkOutput {
    pub(crate) pairs: Vec<(FileItem, String)>,
    /// Every non-ignored directory the walk visited, relative, ending with /
    pub(crate) dirs: Vec<String>,
    pub(crate) ignore_rules: Option<WalkIgnoreRules>,
}

pub(crate) struct WalkIgnoreRules {
    #[cfg(feature = "zlob")]
    inner: ::zlob::walk::WalkerOutcomeRules,
    #[cfg(not(feature = "zlob"))]
    _never: std::convert::Infallible,
}

// SAFETY: the underlying storage is immutable, heap-owned, and thread-safe to
// read from concurrently (mirrors zlob's `IgnoreRules: Send + Sync`).
unsafe impl Send for WalkIgnoreRules {}
unsafe impl Sync for WalkIgnoreRules {}

impl std::fmt::Debug for WalkIgnoreRules {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WalkIgnoreRules")
    }
}

// In ripgrep builds `WalkIgnoreRules` is never constructed (the `_never`
// field is uninhabited), so its methods are legitimately dead there.
#[cfg_attr(not(feature = "zlob"), allow(dead_code))]
impl WalkIgnoreRules {
    /// Returns `true` if the provided path is ignored by the collected rule set
    ///
    /// `relative_path` has to be relative to the walker's provided base path
    pub(crate) fn is_ignored(&self, relative_path: &Path) -> bool {
        #[cfg(feature = "zlob")]
        {
            self.inner
                .rules()
                .is_some_and(|rules| rules.is_ignored(relative_path))
        }
        #[cfg(not(feature = "zlob"))]
        {
            let _ = relative_path;
            match self._never {}
        }
    }

    // The old `is_ignored_untrusted` variant was folded away when zlob's
    // ignore matcher moved to full ancestor enumeration — trailing-slash
    // sniffing on the input is now sufficient for external queries.
}

#[cfg(test)]
mod tests {
    use super::{WalkPolicy, walk_collect_files};
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
        let out = walk_collect_files(root, None, true, policy(false), 1, &counter, None).unwrap();

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

    // Non-git roots prune known non-code directories (node_modules).
    #[test]
    fn prunes_non_code_dirs_for_non_git_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("node_modules")).unwrap();
        fs::write(root.join("node_modules/lib.js"), "x").unwrap();
        fs::write(root.join("index.js"), "x").unwrap();

        let counter = Arc::new(AtomicUsize::new(0));
        let out = walk_collect_files(root, None, false, policy(false), 1, &counter, None).unwrap();
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
            None,
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
            None,
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
                Some(&hook),
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
                Some(&hook),
            )
            .unwrap();
            assert_eq!(*seen.lock(), [std::path::Path::new("a/b")]);
        }
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
        let out = walk_collect_files(&root, None, false, policy(true), 1, &counter, None).unwrap();
        let mut names: Vec<String> = out.pairs.iter().map(|(_, rel)| rel.clone()).collect();
        names.sort();

        assert_eq!(names, ["linked/inner.toml", "projects.toml", "sesh.toml"]);
        assert_eq!(counter.load(Ordering::Relaxed), names.len());

        let (item, rel) = out.pairs.iter().find(|(_, r)| r == "sesh.toml").unwrap();
        assert_eq!(item.size, 10);
        assert_eq!(&rel[item.filename_offset_in_relative_path()..], "sesh.toml");

        let out = walk_collect_files(&root, None, false, policy(false), 1, &counter, None).unwrap();
        let names: Vec<String> = out.pairs.into_iter().map(|(_, rel)| rel).collect();
        assert_eq!(names, ["projects.toml"]);
    }

    // Only the zlob backend surfaces reusable ignore rules; they must match
    // the same tree the walk respected.
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
        let out = walk_collect_files(root, None, true, policy(false), 1, &counter, None).unwrap();

        let rules = out.ignore_rules.expect("zlob surfaces ignore rules");
        assert!(rules.is_ignored(Path::new("target/")));
        assert!(rules.is_ignored(Path::new("debug.log")));
        assert!(!rules.is_ignored(Path::new("Cargo.toml")));
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
            let out =
                walk_collect_files(base, None, true, policy(false), 1, &counter, None).unwrap();
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
                assert!(rules.is_ignored(Path::new("excluded/later/new.txt")));
                assert!(rules.is_ignored(Path::new("debug.log")));
                assert!(!rules.is_ignored(Path::new("src/a.txt")));
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
        let out = walk_collect_files(root, None, true, policy(false), 1, &counter, None).unwrap();
        let names: Vec<String> = out.pairs.iter().map(|(_, rel)| rel.clone()).collect();

        assert!(names.iter().any(|n| n == "src/hit.ts"), "{names:?}");
        assert!(names.iter().any(|n| n == "keep.tmp"), "{names:?}");
        assert!(!names.iter().any(|n| n == "drop.tmp"), "{names:?}");
        assert!(!names.iter().any(|n| n.contains(".worktrees")), "{names:?}");

        // The watcher filters incremental updates with these rules.
        let rules = out.ignore_rules.expect("zlob surfaces ignore rules");
        assert!(rules.is_ignored(std::path::Path::new(".worktrees/wt1/new.ts")));
        assert!(!rules.is_ignored(std::path::Path::new("src/hit.ts")));
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
                None,
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
