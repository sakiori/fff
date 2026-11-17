use super::ref_watches::{OwnRepository, common_dir_of};
use git2::Repository;
use std::path::{Path, PathBuf};

// Whether the common dir's config keeps the refs in reftable.
pub(super) fn names_reftable(common_dir: &Path) -> bool {
    let format = git2::Config::open(&common_dir.join("config"))
        .and_then(|config| config.get_string("extensions.refstorage"));
    format.is_ok_and(|format| format.eq_ignore_ascii_case("reftable"))
}

// libgit2 refuses a reftable repository for its format only after git's discovery found it,
// with its ceilings and filesystem boundary, so that discovery decides, and the ownership
// check libgit2 would run after the format's is made here.
pub(super) fn reftable_repository(base_path: &Path) -> Option<OwnRepository> {
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
    if !crate::watch::is_git_dir(&git_dir) || !names_reftable(&common_dir) {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
