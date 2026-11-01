use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use interprocess::local_socket::ListenerOptions;
use interprocess::local_socket::tokio::Stream;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub(crate) use unix::FileId;

// A private per-user directory holding service discovery, lock and log files.
pub(crate) struct Namespace {
    path: PathBuf,
    #[cfg(unix)]
    dir: std::os::fd::OwnedFd,
}

impl Namespace {
    // Creates the directory if needed and verifies that only the current user controls it.
    pub fn open(path: &Path) -> io::Result<Self> {
        let annotate = |error: io::Error| {
            io::Error::new(
                error.kind(),
                format!("Cannot open service directory {}: {error}", path.display()),
            )
        };
        #[cfg(unix)]
        {
            let (path, dir) = unix::open_private_dir(path).map_err(annotate)?;
            Ok(Self { path, dir })
        }
        #[cfg(windows)]
        {
            let path = std::path::absolute(path).map_err(annotate)?;
            windows::prepare(&path).map_err(annotate)?;
            Ok(Self { path })
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    // Whether the namespace path still leads to the directory opened here.
    pub fn names_opened_directory(&self) -> io::Result<bool> {
        #[cfg(unix)]
        {
            unix::names_dir(&self.dir, &self.path)
        }
        #[cfg(windows)]
        {
            match std::fs::metadata(&self.path) {
                Ok(metadata) => Ok(metadata.is_dir()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(error),
            }
        }
    }

    // Identity of the entry named `name`, or None when the name is absent.
    #[cfg(unix)]
    pub fn file_id(&self, name: &str) -> io::Result<Option<FileId>> {
        unix::file_id(&self.dir, name).map_err(|error| self.error("inspect", name, error))
    }

    // Removes `name` only while it is still the entry identified by `id`. Only the holder of
    // the owner lock creates such entries, so the name cannot change in between.
    #[cfg(unix)]
    pub fn remove_file_if(&self, name: &str, id: FileId) -> io::Result<bool> {
        if self.file_id(name)? != Some(id) {
            return Ok(false);
        }
        self.remove_file(name)?;
        Ok(true)
    }

    // Opens a private regular file in the namespace, never following symlinks.
    pub fn open_file(&self, name: &str, create: bool) -> io::Result<File> {
        #[cfg(unix)]
        let result = unix::open_private_file(&self.dir, name, create);
        #[cfg(windows)]
        let result = windows::open_private_file(&self.path.join(name), create);
        result.map_err(|error| self.error("open", name, error))
    }

    // Names the namespace file and the operation that failed, keeping the error kind.
    pub fn error(&self, operation: &str, name: &str, error: io::Error) -> io::Error {
        io::Error::new(
            error.kind(),
            format!(
                "Cannot {operation} {}: {error}",
                self.path.join(name).display()
            ),
        )
    }

    // Replaces a file so that readers observe either the old or the new content.
    pub fn write_atomic(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let temporary = format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple());
        let result = (|| {
            let mut file = self.create_new(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            self.rename(&temporary, name)
        })();
        if result.is_err() {
            let _ = self.remove_file(&temporary);
        }
        result.map_err(|error| self.error("write", name, error))
    }

    pub fn remove_file(&self, name: &str) -> io::Result<()> {
        #[cfg(unix)]
        let result = unix::remove_file(&self.dir, name);
        #[cfg(windows)]
        let result = std::fs::remove_file(self.path.join(name));
        match result {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result.map_err(|error| self.error("remove", name, error)),
        }
    }

    // Lists file names in the namespace that start with `prefix` and end with `suffix`.
    pub fn list(&self, prefix: &str, suffix: &str) -> io::Result<Vec<String>> {
        #[cfg(unix)]
        let names = unix::list(&self.dir);
        #[cfg(windows)]
        let names = std::fs::read_dir(&self.path).map(|entries| {
            entries
                .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
                .collect::<Vec<_>>()
        });
        let names = names.map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("Cannot list {}: {error}", self.path.display()),
            )
        })?;
        Ok(names
            .into_iter()
            .filter(|name| name.starts_with(prefix) && name.ends_with(suffix))
            .collect())
    }

    fn create_new(&self, name: &str) -> io::Result<File> {
        #[cfg(unix)]
        {
            unix::create_new_private_file(&self.dir, name)
        }
        #[cfg(windows)]
        {
            windows::create_new_private_file(&self.path.join(name))
        }
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        #[cfg(unix)]
        {
            unix::rename(&self.dir, from, to)
        }
        #[cfg(windows)]
        {
            std::fs::rename(self.path.join(from), self.path.join(to))
        }
    }
}

pub(crate) fn listener_options(options: ListenerOptions<'_>) -> io::Result<ListenerOptions<'_>> {
    #[cfg(unix)]
    {
        Ok(options)
    }
    #[cfg(windows)]
    {
        use interprocess::os::windows::local_socket::ListenerOptionsExt;
        Ok(options.security_descriptor(windows::private_descriptor()?))
    }
}

// Rejects a local socket peer running as another user.
pub(crate) fn verify_peer(stream: &Stream) -> io::Result<()> {
    #[cfg(unix)]
    {
        unix::verify_peer(stream)
    }
    #[cfg(windows)]
    {
        // The pipe's explicit ACL admits only the user and privileged OS principals.
        let _ = stream;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_name_the_operation_and_file_and_keep_their_kind() {
        let temp = tempfile::tempdir().unwrap();
        let namespace = Namespace::open(&temp.path().join("service")).unwrap();
        let error = namespace
            .open_file("endpoint-missing.json", false)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        let message = error.to_string();
        assert!(
            message.starts_with("Cannot open ") && message.contains("endpoint-missing.json"),
            "{message}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn identity_guarded_removal_spares_a_replaced_entry() {
        let temp = tempfile::tempdir().unwrap();
        let namespace = Namespace::open(&temp.path().join("service")).unwrap();
        assert_eq!(namespace.file_id("entry").unwrap(), None);
        drop(namespace.open_file("entry", true).unwrap());
        let original = namespace.file_id("entry").unwrap().unwrap();
        // Keeps the original inode alive so the replacement cannot reuse its number.
        let _held = namespace.open_file("entry", false).unwrap();
        namespace.remove_file("entry").unwrap();
        drop(namespace.open_file("entry", true).unwrap());
        let replacement = namespace.file_id("entry").unwrap().unwrap();
        assert_ne!(original, replacement);
        assert!(!namespace.remove_file_if("entry", original).unwrap());
        assert_eq!(namespace.file_id("entry").unwrap(), Some(replacement));
        assert!(namespace.remove_file_if("entry", replacement).unwrap());
        assert_eq!(namespace.file_id("entry").unwrap(), None);
    }
}
