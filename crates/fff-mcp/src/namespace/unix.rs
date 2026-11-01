use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use interprocess::local_socket::tokio::{Stream, prelude::*};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, Stat};

const PRIVATE_DIR_MODE: u32 = 0o700;
const ROOT_UID: u32 = 0;
const CREATE_ATTEMPTS: usize = 16;

// Walks the canonical path from `/` through directory handles, so no component can be
// swapped for a symlink between validation and use (ssh StrictModes style).
pub(super) fn open_private_dir(path: &Path) -> io::Result<(PathBuf, OwnedFd)> {
    use std::os::unix::fs::DirBuilderExt;
    let absolute = std::path::absolute(path)?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(PRIVATE_DIR_MODE)
        .create(&absolute)?;
    let canonical = absolute.canonicalize()?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let mut dir = rustix::fs::open("/", flags, Mode::empty())?;
    let parts: Vec<_> = canonical
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part),
            _ => None,
        })
        .collect();
    for (position, part) in parts.iter().enumerate() {
        check_ancestor(&rustix::fs::fstat(&dir)?, &canonical)?;
        dir = rustix::fs::openat(&dir, *part, flags | OFlags::NOFOLLOW, Mode::empty())?;
        if position + 1 == parts.len() {
            check_private(&rustix::fs::fstat(&dir)?, FileType::Directory)?;
        }
    }
    if parts.is_empty() {
        return Err(private_error(
            "Service directory cannot be the filesystem root",
        ));
    }
    Ok((canonical, dir))
}

// A directory-relative `O_CREAT` without `O_EXCL` is not atomic on every kernel: a caller
// racing another creator of the same name can fail with ENOENT although the file exists.
// Exclusive creation and a plain open each have one defined outcome; EEXIST from the first
// and ENOENT from the second only mean another process created or removed the file in
// between, so the pair is retried a bounded number of times until one of them settles.
pub(super) fn open_private_file(dir: &OwnedFd, name: &str, create: bool) -> io::Result<File> {
    let flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    if !create {
        return private_file(dir, name, flags);
    }
    let mut unsettled = None;
    for _ in 0..CREATE_ATTEMPTS {
        match private_file(dir, name, flags | OFlags::CREATE | OFlags::EXCL) {
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            // The same race can fail exclusive creation spuriously; a removed directory
            // keeps failing and exhausts the attempts.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                unsettled = Some(error);
                continue;
            }
            result => return result,
        }
        match private_file(dir, name, flags) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => unsettled = Some(error),
            result => return result,
        }
    }
    Err(unsettled.expect("at least one attempt"))
}

pub(super) fn create_new_private_file(dir: &OwnedFd, name: &str) -> io::Result<File> {
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    private_file(dir, name, flags)
}

pub(super) fn rename(dir: &OwnedFd, from: &str, to: &str) -> io::Result<()> {
    Ok(rustix::fs::renameat(dir, from, dir, to)?)
}

pub(super) fn remove_file(dir: &OwnedFd, name: &str) -> io::Result<()> {
    Ok(rustix::fs::unlinkat(dir, name, AtFlags::empty())?)
}

pub(super) fn list(dir: &OwnedFd) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in rustix::fs::Dir::read_from(dir)? {
        let entry = entry?;
        if let Some(name) = OsStr::from_bytes(entry.file_name().to_bytes()).to_str() {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

pub(super) fn names_dir(dir: &OwnedFd, path: &Path) -> io::Result<bool> {
    let opened = rustix::fs::fstat(dir)?;
    match rustix::fs::stat(path) {
        Ok(current) => Ok(current.st_dev == opened.st_dev && current.st_ino == opened.st_ino),
        Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

// Identity of a namespace entry; it changes when the name is replaced by another file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileId {
    dev: rustix::fs::Dev,
    ino: u64,
}

pub(super) fn file_id(dir: &OwnedFd, name: &str) -> io::Result<Option<FileId>> {
    match rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(Some(FileId {
            dev: stat.st_dev,
            ino: stat.st_ino,
        })),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn verify_peer(stream: &Stream) -> io::Result<()> {
    let euid = rustix::process::geteuid().as_raw();
    match stream.peer_creds()?.euid() {
        Some(peer) if peer == euid => Ok(()),
        _ => Err(private_error("Local service peer belongs to another user")),
    }
}

fn private_file(dir: &OwnedFd, name: &str, flags: OFlags) -> io::Result<File> {
    let file = rustix::fs::openat(dir, name, flags, Mode::RUSR | Mode::WUSR)?;
    check_private(&rustix::fs::fstat(file.as_fd())?, FileType::RegularFile)?;
    Ok(File::from(file))
}

fn check_ancestor(stat: &Stat, path: &Path) -> io::Result<()> {
    let euid = rustix::process::geteuid().as_raw();
    let mode = Mode::from_raw_mode(stat.st_mode);
    let trusted_owner = stat.st_uid == ROOT_UID || stat.st_uid == euid;
    // Root-owned sticky directories such as /tmp prevent others from replacing our entries.
    let shared_sticky = stat.st_uid == ROOT_UID && mode.contains(Mode::SVTX);
    if !trusted_owner || (mode.intersects(Mode::WGRP | Mode::WOTH) && !shared_sticky) {
        return Err(private_error(&format!(
            "Every parent of the service directory {} must be owned by the current user or root and not writable by others",
            path.display()
        )));
    }
    Ok(())
}

fn check_private(stat: &Stat, kind: FileType) -> io::Result<()> {
    let euid = rustix::process::geteuid().as_raw();
    if FileType::from_raw_mode(stat.st_mode) != kind
        || stat.st_uid != euid
        || Mode::from_raw_mode(stat.st_mode).intersects(Mode::RWXG | Mode::RWXO)
    {
        return Err(private_error(
            "Service directory and state files must be private to the current user",
        ));
    }
    Ok(())
}

fn private_error(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn chmod(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn state_files_reject_public_permissions_and_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let (_, dir) = open_private_dir(&temp.path().join("service")).unwrap();
        let path = temp.path().join("service/endpoint.json");
        drop(open_private_file(&dir, "endpoint.json", true).unwrap());
        assert!(open_private_file(&dir, "endpoint.json", false).is_ok());
        chmod(&path, 0o644);
        assert!(open_private_file(&dir, "endpoint.json", false).is_err());
        assert!(open_private_file(&dir, "endpoint.json", true).is_err());
        chmod(&path, 0o600);
        symlink(&path, temp.path().join("service/alias")).unwrap();
        assert!(open_private_file(&dir, "alias", false).is_err());
    }

    // Concurrent clients open the same lock files of a fresh namespace at once.
    #[test]
    fn concurrent_creators_all_open_the_same_file() {
        const CREATORS: usize = 4;
        const ROUNDS: usize = 200;
        let temp = tempfile::tempdir().unwrap();
        let (_, dir) = open_private_dir(&temp.path().join("service")).unwrap();
        let barrier = std::sync::Barrier::new(CREATORS);
        let failures: Vec<String> = std::thread::scope(|scope| {
            let creators: Vec<_> = (0..CREATORS)
                .map(|_| {
                    scope.spawn(|| {
                        (0..ROUNDS)
                            .filter_map(|round| {
                                let name = format!("startup-{round}.lock");
                                barrier.wait();
                                open_private_file(&dir, &name, true)
                                    .err()
                                    .map(|error| format!("round {round}: {error}"))
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            creators
                .into_iter()
                .flat_map(|creator| creator.join().unwrap())
                .collect()
        });
        assert!(failures.is_empty(), "{failures:?}");
    }

    #[test]
    fn creating_in_a_removed_directory_fails_as_not_found() {
        let temp = tempfile::tempdir().unwrap();
        let (_, dir) = open_private_dir(&temp.path().join("service")).unwrap();
        std::fs::remove_dir(temp.path().join("service")).unwrap();
        let error = open_private_file(&dir, "startup.lock", true).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn directories_reject_public_access_and_writable_parents() {
        let temp = tempfile::tempdir().unwrap();
        let service = temp.path().join("service");
        assert!(open_private_dir(&service).is_ok());
        chmod(&service, 0o750);
        assert!(open_private_dir(&service).is_err());
        chmod(&service, 0o700);
        let shared = temp.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        chmod(&shared, 0o777);
        assert!(open_private_dir(&shared.join("service")).is_err());
        chmod(&shared, 0o755);
        assert!(open_private_dir(&shared.join("service")).is_ok());
    }

    #[test]
    fn symlinked_namespace_resolves_to_the_validated_directory() {
        let temp = tempfile::tempdir().unwrap();
        let service = temp.path().join("service");
        let (canonical, _) = open_private_dir(&service).unwrap();
        let alias = temp.path().join("alias");
        symlink(&service, &alias).unwrap();
        assert_eq!(open_private_dir(&alias).unwrap().0, canonical);
    }
}
