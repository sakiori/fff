use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::types::FileItem;

// What recording a file needs to know of the index, read under its lock: the entry it has, if
// any, whether that entry is binary already, and whether its content feeds the bigram overlay.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RecordSlot {
    pub(crate) held: bool,
    pub(crate) binary: bool,
    pub(crate) overlay: bool,
}

// What a file's entry is recorded from, read with no lock held: its stat, binary verdict and
// bigrams, or a new file's whole entry. The write section only commits it.
pub(crate) struct PreparedFile {
    pub(crate) path: PathBuf,
    pub(crate) recorded_at: SystemTime,
    pub(crate) metadata: Option<Metadata>,
    pub(crate) kind: Prepared,
}

pub(crate) enum Prepared {
    Held {
        binary: bool,
        bigrams: Option<Vec<u16>>,
    },
    New(Option<(FileItem, String)>),
}

impl PreparedFile {
    pub(crate) fn read(path: &Path, base_path: &Path, slot: RecordSlot) -> Self {
        // Taken before the stat: Git's racy rule needs a time no later than the stat's.
        let recorded_at = SystemTime::now();
        if slot.held {
            let metadata = std::fs::metadata(path).ok();
            let size = metadata.as_ref().map_or(0, Metadata::len);
            let binary = !slot.binary && size > 0 && crate::types::file_is_binary(path);
            let bigrams = (slot.overlay && metadata.is_some())
                .then(|| crate::stat::read_regular(path))
                .flatten()
                .map(|content| crate::index::bigram_filter::extract_bigrams(&content));
            return Self {
                path: path.to_path_buf(),
                recorded_at,
                metadata,
                kind: Prepared::Held { binary, bigrams },
            };
        }
        let path_for_index = path_for_index(path, base_path);
        let metadata = (path_for_index.as_ref()).and_then(|path| std::fs::metadata(path).ok());
        let path_for_index = path_for_index.unwrap_or_else(|| path.to_path_buf());
        let item = metadata.as_ref().map(|metadata| {
            let (mut item, rel) = FileItem::new_with_metadata(
                path_for_index.clone(),
                base_path,
                None,
                Some(metadata),
            );
            item.smudge_if_racy(recorded_at);
            // Classified here, as a scan classifies in batches.
            item.detect_binary_per_byte(
                &path_for_index,
                &mut [0u8; crate::types::BINARY_CLASSIFICATION_CHUNK_SIZE],
            );
            (item, rel)
        });
        Self {
            path: path.to_path_buf(),
            recorded_at,
            metadata,
            kind: Prepared::New(item),
        }
    }
}

// Windows' `pathdiff::diff_paths` is byte-wise: a short-name input shares no prefix with the
// canonical base and would turn absolute, so it is canonicalized first, `None` if that fails.
fn path_for_index(path: &Path, _base_path: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    if !path.starts_with(_base_path) {
        return crate::path_utils::canonicalize(path).ok();
    }
    Some(path.to_path_buf())
}
