pub mod client;
mod identity;
pub mod namespace;
pub mod program;
pub mod protocol;
pub mod session;
mod sigpipe;
#[cfg(unix)]
pub mod socket;

use std::io;
use std::path::{Path, PathBuf};

use interprocess::local_socket::{Name, prelude::*};

pub use identity::{Build, ServiceFiles};

/// The namespace used when none is configured: `fff/service` in the user state directory.
pub fn default_directory() -> PathBuf {
    state_dir().join("fff").join("service")
}

/// Per-user state root shared by logs and the default service namespace.
#[cfg(not(windows))]
pub fn state_dir() -> PathBuf {
    absolute_env("XDG_STATE_HOME").unwrap_or_else(|| dirs_home().join(".local").join("state"))
}

/// Per-user state root shared by logs and the default service namespace.
#[cfg(windows)]
pub fn state_dir() -> PathBuf {
    absolute_env("LOCALAPPDATA").unwrap_or_else(|| dirs_home().join("AppData").join("Local"))
}

/// The spelling of a frecency database path that every client sends, so equal databases
/// share an index. The database may not exist yet, so only its directory is resolved.
pub fn stable_database_path(path: impl AsRef<Path>) -> io::Result<String> {
    let path = std::path::absolute(path)?;
    let resolved = match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => canonicalize(parent).map(|parent| parent.join(name)).ok(),
        _ => None,
    };
    Ok(resolved.unwrap_or(path).to_string_lossy().into_owned())
}

/// Local socket name of a service endpoint.
pub fn socket_name(name: &str) -> io::Result<Name<'_>> {
    #[cfg(unix)]
    {
        name.to_fs_name::<interprocess::local_socket::GenericFilePath>()
    }
    #[cfg(windows)]
    {
        name.to_ns_name::<interprocess::local_socket::GenericNamespaced>()
    }
}

#[cfg(windows)]
fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    dunce::canonicalize(path)
}

#[cfg(not(windows))]
fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    std::fs::canonicalize(path)
}

// XDG spec: a non-absolute value must be ignored as if unset.
fn absolute_env(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|value| value.is_absolute())
}

// Relative homes would break the absolute-path contract of the default above,
// and TMPDIR can be relative too, so every fallback is filtered for absoluteness.
fn dirs_home() -> PathBuf {
    absolute_env("HOME")
        .or_else(|| absolute_env("USERPROFILE"))
        .or_else(|| {
            let tmp = std::env::temp_dir();
            tmp.is_absolute().then_some(tmp)
        })
        .unwrap_or_else(|| PathBuf::from(FALLBACK_ROOT))
}

#[cfg(not(windows))]
const FALLBACK_ROOT: &str = "/tmp";
#[cfg(windows)]
const FALLBACK_ROOT: &str = "C:\\Windows\\Temp";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_paths_resolve_their_directory_only() {
        let temp = tempfile::tempdir().unwrap();
        let dir = canonicalize(temp.path()).unwrap();
        std::fs::create_dir(dir.join("nested")).unwrap();
        let expected = dir.join("frecency").to_string_lossy().into_owned();
        let spelled = temp.path().join("nested").join("..").join("frecency");
        assert_eq!(stable_database_path(&spelled).unwrap(), expected);
        let missing = dir.join("missing").join("db");
        assert_eq!(
            stable_database_path(&missing).unwrap(),
            missing.to_string_lossy()
        );
    }
}
