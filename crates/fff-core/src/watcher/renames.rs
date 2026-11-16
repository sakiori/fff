use std::path::{Path, PathBuf};

use ahash::{AHashMap, AHashSet};

use crate::stat::Stat;

// What a rename keeps of a file: its inode where the platform has one, its size and its
// modification time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct FileIdentity {
    inode: u32,
    size: u64,
    mtime: u64,
}

impl FileIdentity {
    pub(super) fn of(stat: &Stat) -> Self {
        Self {
            inode: stat.inode,
            size: stat.size,
            mtime: stat.modified,
        }
    }
}

// Destination -> source over a delta's removed and created files: pairs the backend reported, whose
// source went and destination was written, win; the rest pair by identity, inode included.
pub(super) fn detect_renames(
    removed: &[(PathBuf, FileIdentity)],
    created: &[(PathBuf, FileIdentity)],
    written: &AHashSet<&Path>,
    explicit: &[(PathBuf, PathBuf)],
) -> AHashMap<PathBuf, PathBuf> {
    if removed.is_empty() || written.is_empty() {
        return AHashMap::new();
    }
    let removed_paths: AHashSet<&Path> = removed.iter().map(|(path, _)| path.as_path()).collect();
    let mut renames: AHashMap<PathBuf, PathBuf> = (explicit.iter())
        .filter(|(from, to)| {
            from != to && removed_paths.contains(from.as_path()) && written.contains(to.as_path())
        })
        .map(|(from, to)| (to.clone(), from.clone()))
        .collect();
    let sources: AHashSet<&Path> = renames.values().map(PathBuf::as_path).collect();
    let removed: Vec<(PathBuf, FileIdentity)> = (removed.iter())
        .filter(|(path, _)| !sources.contains(path.as_path()))
        .cloned()
        .collect();
    let created: Vec<(PathBuf, FileIdentity)> = (created.iter())
        .filter(|(path, _)| !renames.contains_key(path))
        .cloned()
        .collect();
    for (from, to) in pair_renames_by_identity(&removed, &created) {
        renames.insert(to, from);
    }
    renames
}

// Pairs a removed and an added path that alone in the delta share an identity, as a rename keeps
// it; uniqueness on both sides keeps bulk operations from cross-wiring histories.
fn pair_renames_by_identity(
    removed: &[(PathBuf, FileIdentity)],
    added: &[(PathBuf, FileIdentity)],
) -> Vec<(PathBuf, PathBuf)> {
    if removed.is_empty() || added.is_empty() {
        return Vec::new();
    }

    // `None` marks an identity claimed by more than one distinct path. The OS
    // repeats a path within a batch (FSEvents does routinely), and a repeat is
    // still one candidate, not a collision.
    fn index_by_identity(
        entries: &[(PathBuf, FileIdentity)],
    ) -> ahash::AHashMap<FileIdentity, Option<&Path>> {
        let mut by_identity: ahash::AHashMap<FileIdentity, Option<&Path>> =
            ahash::AHashMap::default();
        for (path, identity) in entries {
            by_identity
                .entry(*identity)
                .and_modify(|slot| {
                    if *slot != Some(path.as_path()) {
                        *slot = None;
                    }
                })
                .or_insert(Some(path.as_path()));
        }
        by_identity
    }

    let sources = index_by_identity(removed);
    let targets = index_by_identity(added);

    targets
        .into_iter()
        .filter_map(|(identity, target)| {
            let target = target?;
            let source = (*sources.get(&identity)?)?;
            Some((source.to_path_buf(), target.to_path_buf()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(size: u64, mtime: u64) -> FileIdentity {
        FileIdentity {
            inode: 0,
            size,
            mtime,
        }
    }

    fn entry(path: &str, size: u64, mtime: u64) -> (PathBuf, FileIdentity) {
        (PathBuf::from(path), identity(size, mtime))
    }

    #[test]
    fn unique_size_and_mtime_pairs_as_a_rename() {
        let pairs = pair_renames_by_identity(
            &[entry("/w/old.rs", 120, 1_700_000_000)],
            &[entry("/w/new.rs", 120, 1_700_000_000)],
        );

        assert_eq!(
            pairs,
            vec![(PathBuf::from("/w/old.rs"), PathBuf::from("/w/new.rs"))]
        );
    }

    #[test]
    fn ambiguous_identities_are_never_paired() {
        let two_removed = pair_renames_by_identity(
            &[entry("/w/a.rs", 10, 5), entry("/w/b.rs", 10, 5)],
            &[entry("/w/c.rs", 10, 5)],
        );
        assert!(
            two_removed.is_empty(),
            "two removed files sharing an identity are ambiguous: {two_removed:?}"
        );

        let two_added = pair_renames_by_identity(
            &[entry("/w/a.rs", 10, 5)],
            &[entry("/w/b.rs", 10, 5), entry("/w/c.rs", 10, 5)],
        );
        assert!(
            two_added.is_empty(),
            "two added files sharing an identity are ambiguous: {two_added:?}"
        );
    }

    #[test]
    fn a_repeated_path_is_one_candidate_not_a_collision() {
        // FSEvents routinely reports the same path several times per batch.
        let pairs = pair_renames_by_identity(
            &[entry("/w/old.rs", 15, 99), entry("/w/old.rs", 15, 99)],
            &[entry("/w/new.rs", 15, 99), entry("/w/new.rs", 15, 99)],
        );

        assert_eq!(
            pairs,
            vec![(PathBuf::from("/w/old.rs"), PathBuf::from("/w/new.rs"))]
        );
    }

    #[test]
    fn empty_files_are_paired_only_when_unambiguous() {
        let unique = pair_renames_by_identity(&[entry("/w/a", 0, 7)], &[entry("/w/b", 0, 7)]);
        assert_eq!(
            unique,
            vec![(PathBuf::from("/w/a"), PathBuf::from("/w/b"))],
            "a lone empty file still has a usable identity"
        );

        let colliding = pair_renames_by_identity(
            &[entry("/w/a", 0, 7), entry("/w/b", 0, 7)],
            &[entry("/w/c", 0, 7), entry("/w/d", 0, 7)],
        );
        assert!(colliding.is_empty(), "{colliding:?}");
    }

    #[test]
    fn differing_metadata_is_not_a_rename() {
        // git checkout rewrites mtime, so branch switches must never pair.
        let fresh_mtime = pair_renames_by_identity(
            &[entry("/w/old.rs", 120, 1_700_000_000)],
            &[entry("/w/new.rs", 120, 1_700_000_900)],
        );
        assert!(fresh_mtime.is_empty(), "{fresh_mtime:?}");

        let other_size = pair_renames_by_identity(
            &[entry("/w/old.rs", 120, 1_700_000_000)],
            &[entry("/w/new.rs", 121, 1_700_000_000)],
        );
        assert!(other_size.is_empty(), "{other_size:?}");
    }

    #[test]
    fn independent_identities_pair_side_by_side() {
        let pairs = pair_renames_by_identity(
            &[entry("/w/a.rs", 10, 1), entry("/w/b.rs", 20, 2)],
            &[entry("/w/y.rs", 20, 2), entry("/w/x.rs", 10, 1)],
        );

        let mut pairs = pairs;
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                (PathBuf::from("/w/a.rs"), PathBuf::from("/w/x.rs")),
                (PathBuf::from("/w/b.rs"), PathBuf::from("/w/y.rs")),
            ]
        );
    }
}
