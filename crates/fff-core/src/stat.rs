use std::fs::{File, Metadata};
use std::path::Path;
use std::time::{Duration, SystemTime};

// A file system's timestamp tick: kernels stamp files from a clock that may lag the precise one.
const TICK: Duration = Duration::from_millis(50);
// Times without a sub-second part come from a file system of seconds, FAT's of two.
const COARSE_TICK: Duration = Duration::from_secs(2);
// No sub-second part reaches it: an entry recorded racily, as Git smudges one by its size.
const SMUDGED: u32 = u32::MAX;

// The stat fields Git's `core.checkStat` compares: size, mtime with nanoseconds, and inode and
// ctime folded into `stamp`; `inode` alone follows a rename. Both are 0 on platforms without.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stat {
    pub(crate) size: u64,
    pub(crate) modified: u64,
    pub(crate) nanos: u32,
    pub(crate) stamp: u32,
    pub(crate) inode: u32,
}

impl Stat {
    pub(crate) fn of(metadata: &Metadata) -> Self {
        let since = (metadata.modified().ok())
            .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
            .unwrap_or_default();
        #[cfg(unix)]
        let (stamp, inode) = {
            use std::os::unix::fs::MetadataExt;
            let changed = metadata.ctime() as i128 * 1_000_000_000 + metadata.ctime_nsec() as i128;
            (
                stamp_of(metadata.ino(), changed as i64),
                inode_of(metadata.ino()),
            )
        };
        #[cfg(not(unix))]
        let (stamp, inode) = (0, 0);
        Self {
            size: metadata.len(),
            modified: since.as_secs(),
            nanos: since.subsec_nanos(),
            stamp,
            inode,
        }
    }

    // Whether the file changed since an entry recorded `recorded`; a smudged entry never vouches
    // for its file.
    pub(crate) fn differs_from(&self, recorded: &Stat) -> bool {
        recorded.is_smudged() || self != recorded
    }

    // Whether anything but the sub-second time a smudged entry lost tells of a change.
    pub(crate) fn visibly_differs_from(&self, recorded: &Stat) -> bool {
        match recorded.is_smudged() {
            true => {
                Self { nanos: 0, ..*self }
                    != Self {
                        nanos: 0,
                        ..*recorded
                    }
            }
            false => self != recorded,
        }
    }

    pub(crate) fn is_smudged(&self) -> bool {
        self.nanos == SMUDGED
    }

    // Git's racy-git rule: a file written within a timestamp tick of the moment its entry was
    // recorded may change again unseen by its stat, so its entry is checked again next time.
    pub(crate) fn smudged_if_racy(self, recorded_at: SystemTime) -> Self {
        let modified = SystemTime::UNIX_EPOCH + Duration::new(self.modified, self.nanos);
        let tick = if self.nanos == 0 { COARSE_TICK } else { TICK };
        match modified + tick > recorded_at {
            true => Self {
                nanos: SMUDGED,
                ..self
            },
            false => self,
        }
    }
}

pub(crate) fn stamp_of(inode: u64, changed_ns: i64) -> u32 {
    (crate::types::fixed_hasher().hash_one((inode, changed_ns)) as u32).max(1)
}

pub(crate) fn inode_of(inode: u64) -> u32 {
    (crate::types::fixed_hasher().hash_one(inode) as u32).max(1)
}

// Opens `path` for reading only when it is a regular file, checked on the open handle so a FIFO
// or device swapped in never blocks the reader.
pub(crate) fn open_regular(path: &Path) -> Option<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path).ok()?;
    file.metadata().ok()?.is_file().then_some(file)
}

// The content of the regular file at `path`.
pub(crate) fn read_regular(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut file = open_regular(path)?;
    let mut content = Vec::new();
    file.read_to_end(&mut content).ok()?;
    Some(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(modified: SystemTime) -> Stat {
        let since = modified.duration_since(SystemTime::UNIX_EPOCH).unwrap();
        Stat {
            size: 3,
            modified: since.as_secs(),
            nanos: since.subsec_nanos(),
            stamp: 1,
            inode: 1,
        }
    }

    #[test]
    fn an_entry_recorded_within_a_tick_of_its_write_is_smudged() {
        // Mid-second, so the whole seconds of the times compared here agree.
        let now = SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_750);
        let older = stat(now - Duration::from_millis(500));
        assert_eq!(older.smudged_if_racy(now), older);
        let racy = stat(now - Duration::from_millis(10)).smudged_if_racy(now);
        assert!(racy.is_smudged());
        assert!(
            stat(now).differs_from(&racy),
            "a smudged entry vouches for nothing"
        );
        assert!(!stat(now).visibly_differs_from(&racy));
        let whole = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert!(
            stat(whole)
                .smudged_if_racy(whole + Duration::from_secs(1))
                .is_smudged()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_is_never_read() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        assert!(made.unwrap().success());
        assert!(read_regular(&fifo).is_none());
        std::fs::write(dir.path().join("a"), "alpha").unwrap();
        assert_eq!(read_regular(&dir.path().join("a")).unwrap(), b"alpha");
    }
}
