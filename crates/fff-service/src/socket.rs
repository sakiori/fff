use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::ServiceFiles;
use crate::namespace::Namespace;

const NAMESPACE_HASH_LEN: usize = 16;
// Well under the 10 days after which systemd-tmpfiles removes unused entries of /tmp.
const REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Where the service of one build listens: in its namespace, or, when that path exceeds the
/// platform's socket path limit, in a private per-user runtime directory under a name derived
/// from the namespace path, as tmux and ssh-agent keep their sockets short. The namespace's
/// state stays in the namespace either way.
pub struct SocketPlace {
    runtime: Option<Namespace>,
    name: String,
    refreshed: Mutex<Instant>,
}

impl SocketPlace {
    /// Picks the place in the platform's runtime roots: `$XDG_RUNTIME_DIR/fff` on Linux when
    /// set, then `/tmp/fff-<uid>`.
    pub fn open(namespace: &Namespace, files: &ServiceFiles) -> io::Result<Self> {
        Self::open_with_roots(namespace, files, &runtime_roots())
    }

    /// Like [`SocketPlace::open`], trying `roots` in order instead of the platform's.
    pub fn open_with_roots(
        namespace: &Namespace,
        files: &ServiceFiles,
        roots: &[PathBuf],
    ) -> io::Result<Self> {
        let name = files.socket();
        if fits(&namespace.path().join(&name)) {
            return Ok(Self::new(None, name));
        }
        let name = format!("{}-{name}", namespace_hash(namespace.path()));
        let mut refusals = Vec::new();
        for root in roots {
            match Self::open_root(root, &name) {
                Ok(runtime) => return Ok(Self::new(Some(runtime), name)),
                Err(error) => refusals.push(format!("{}: {error}", root.display())),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "The socket of service directory {} exceeds the platform's socket path limit, \
                 and no runtime directory can hold it: {}",
                namespace.path().display(),
                refusals.join("; ")
            ),
        ))
    }

    fn open_root(root: &Path, name: &str) -> io::Result<Namespace> {
        if !fits(&root.join(name)) {
            return Err(too_long());
        }
        let runtime = Namespace::open_in_parent(root)?;
        // The opened directory is canonical, which can be longer than its spelling.
        if !fits(&runtime.path().join(name)) {
            return Err(too_long());
        }
        Ok(runtime)
    }

    fn new(runtime: Option<Namespace>, name: String) -> Self {
        Self {
            runtime,
            name,
            refreshed: Mutex::new(Instant::now()),
        }
    }

    /// The directory holding the socket: the namespace or the runtime directory.
    pub fn dir<'a>(&'a self, namespace: &'a Namespace) -> &'a Namespace {
        self.runtime.as_ref().unwrap_or(namespace)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn path(&self, namespace: &Namespace) -> PathBuf {
        self.dir(namespace).path().join(&self.name)
    }

    /// Renews the times of a socket in a runtime directory and of the directory once an hour,
    /// so that age-based cleaners of /tmp keep the socket of a long-running service.
    pub fn keep_fresh(&self) -> io::Result<()> {
        let Some(runtime) = &self.runtime else {
            return Ok(());
        };
        let mut refreshed = self
            .refreshed
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if refreshed.elapsed() < REFRESH_INTERVAL {
            return Ok(());
        }
        runtime.touch(&self.name)?;
        runtime.touch(".")?;
        *refreshed = Instant::now();
        Ok(())
    }
}

fn too_long() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "the socket path exceeds the platform's limit",
    )
}

// Whether a socket can be bound at `path`; the limit is that of `sockaddr_un`.
fn fits(path: &Path) -> bool {
    rustix::net::SocketAddrUnix::new(path).is_ok()
}

fn namespace_hash(path: &Path) -> String {
    let hash = blake3::hash(path.as_os_str().as_bytes()).to_hex();
    hash[..NAMESPACE_HASH_LEN].to_owned()
}

// `$XDG_RUNTIME_DIR/fff` on Linux when it is set, then `/tmp/fff-<uid>`. In debug builds,
// `FFF_MCP_TEST_RUNTIME_DIR` replaces both, so tests never write to the shared ones.
fn runtime_roots() -> Vec<PathBuf> {
    if cfg!(debug_assertions)
        && let Some(root) = std::env::var_os("FFF_MCP_TEST_RUNTIME_DIR")
    {
        return vec![PathBuf::from(root)];
    }
    let mut roots = Vec::new();
    if cfg!(target_os = "linux")
        && let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)
        && runtime.is_absolute()
    {
        roots.push(runtime.join("fff"));
    }
    let uid = rustix::process::geteuid().as_raw();
    roots.push(PathBuf::from("/tmp").join(format!("fff-{uid}")));
    roots
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    use crate::Build;

    // A namespace path long enough that no socket fits in it on any platform.
    fn long_namespace(root: &Path) -> Namespace {
        let segment = "n".repeat(60);
        Namespace::open(&root.join(&segment).join(&segment).join("service")).unwrap()
    }

    // A short private runtime root inside a test directory, standing in for /tmp.
    fn short_root(name: &str) -> (tempfile::TempDir, PathBuf) {
        let parent = tempfile::Builder::new()
            .prefix("fs")
            .tempdir_in("/tmp")
            .unwrap();
        let root = parent.path().canonicalize().unwrap().join(name);
        (parent, root)
    }

    #[test]
    fn a_short_namespace_keeps_its_socket() {
        let (_parent, root) = short_root("ns");
        let namespace = Namespace::open(&root).unwrap();
        let files = Build::current().files();
        let place = SocketPlace::open_with_roots(&namespace, &files, &[]).unwrap();
        assert_eq!(
            place.path(&namespace),
            namespace.path().join(files.socket())
        );
    }

    #[test]
    fn a_long_namespace_gets_a_short_socket_in_a_private_runtime_directory() {
        let temp = tempfile::tempdir().unwrap();
        let namespace = long_namespace(temp.path());
        let files = Build::current().files();
        assert!(!fits(&namespace.path().join(files.socket())));
        let (_parent, root) = short_root("rt");
        let place =
            SocketPlace::open_with_roots(&namespace, &files, std::slice::from_ref(&root)).unwrap();
        let path = place.path(&namespace);
        assert!(fits(&path), "{}", path.display());
        assert_eq!(path.parent().unwrap(), root);
        let mode = std::fs::metadata(&root).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        // Another opening of the same namespace, as the client or the service makes it,
        // derives the same socket; another namespace derives another one.
        let again = Namespace::open(namespace.path()).unwrap();
        let same =
            SocketPlace::open_with_roots(&again, &files, std::slice::from_ref(&root)).unwrap();
        assert_eq!(same.path(&again), path);
        let other = Namespace::open(&namespace.path().with_file_name("other")).unwrap();
        let different =
            SocketPlace::open_with_roots(&other, &files, std::slice::from_ref(&root)).unwrap();
        assert_ne!(different.path(&other), path);
    }

    #[test]
    fn a_runtime_directory_open_to_others_or_a_symlink_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let namespace = long_namespace(temp.path());
        let files = Build::current().files();
        let (_parent, root) = short_root("rt");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let error = SocketPlace::open_with_roots(&namespace, &files, std::slice::from_ref(&root))
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");

        let (_target_parent, target) = short_root("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (_link_parent, link) = short_root("link");
        symlink(&target, &link).unwrap();
        let error = SocketPlace::open_with_roots(&namespace, &files, std::slice::from_ref(&link))
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
        assert!(error.to_string().contains("symlink"), "{error}");
    }

    #[test]
    fn unusable_runtime_roots_are_skipped_and_all_their_refusals_reported() {
        let temp = tempfile::tempdir().unwrap();
        let namespace = long_namespace(temp.path());
        let files = Build::current().files();
        let too_long = temp.path().join("r".repeat(120));
        let missing_parent = PathBuf::from("/nonexistent-fff-parent/fff");
        let (_open_parent, open_to_others) = short_root("open");
        std::fs::create_dir(&open_to_others).unwrap();
        std::fs::set_permissions(&open_to_others, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (_parent, root) = short_root("rt");
        let roots = [
            too_long.clone(),
            missing_parent.clone(),
            open_to_others.clone(),
            root.clone(),
        ];
        let place = SocketPlace::open_with_roots(&namespace, &files, &roots).unwrap();
        assert_eq!(place.path(&namespace).parent().unwrap(), root);
        assert!(!too_long.exists());

        let error = SocketPlace::open_with_roots(&namespace, &files, &roots[..3])
            .err()
            .unwrap();
        let message = error.to_string();
        for root in &roots[..3] {
            assert!(message.contains(&root.display().to_string()), "{message}");
        }
        assert!(message.contains("private"), "{message}");
    }

    #[test]
    fn a_regular_file_in_place_of_the_runtime_directory_is_named_as_such() {
        let temp = tempfile::tempdir().unwrap();
        let namespace = long_namespace(temp.path());
        let files = Build::current().files();
        let (_parent, root) = short_root("file");
        std::fs::write(&root, "").unwrap();
        let error = SocketPlace::open_with_roots(&namespace, &files, std::slice::from_ref(&root))
            .err()
            .unwrap();
        assert!(error.to_string().contains("is not a directory"), "{error}");
    }

    #[test]
    fn a_runtime_socket_is_kept_fresh() {
        let temp = tempfile::tempdir().unwrap();
        let namespace = long_namespace(temp.path());
        let files = Build::current().files();
        let (_parent, root) = short_root("rt");
        let place =
            SocketPlace::open_with_roots(&namespace, &files, std::slice::from_ref(&root)).unwrap();
        let socket = place.path(&namespace);
        std::fs::write(&socket, "").unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(5 * 24 * 60 * 60);
        let file = std::fs::File::options().write(true).open(&socket).unwrap();
        file.set_modified(old).unwrap();
        // Within the interval nothing is touched; past it the socket is renewed.
        place.keep_fresh().unwrap();
        assert_eq!(std::fs::metadata(&socket).unwrap().modified().unwrap(), old);
        *place.refreshed.lock().unwrap() = Instant::now() - REFRESH_INTERVAL;
        place.keep_fresh().unwrap();
        assert!(std::fs::metadata(&socket).unwrap().modified().unwrap() > old);
    }
}
