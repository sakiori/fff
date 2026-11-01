use std::path::Path;

use fff::FilePicker;
use fff::scope::SearchScope;
use fff::types::{DirItem, FileItem};

/// The part of an index one session searches. Paths are relative to the session
/// directory, and items outside it are invisible.
#[derive(Clone, Copy)]
pub(crate) struct SessionView<'a> {
    pub picker: &'a FilePicker,
    pub scope: &'a SearchScope,
    root: &'a Path,
}

impl<'a> SessionView<'a> {
    pub fn new(picker: &'a FilePicker, scope: &'a SearchScope, root: &'a Path) -> Self {
        Self {
            picker,
            scope,
            root,
        }
    }

    /// The session directory, absolute.
    pub fn root(&self) -> &'a Path {
        self.root
    }

    /// Session-relative path of an indexed file, or `None` outside the session.
    pub fn file_path(&self, file: &FileItem) -> Option<String> {
        self.within(file.relative_path(self.picker))
    }

    /// Session-relative path of an indexed directory, or `None` outside the session.
    pub fn dir_path(&self, dir: &DirItem) -> Option<String> {
        self.within(dir.relative_path(self.picker))
    }

    /// Path of a search result as reported to the client.
    pub fn display(&self, file: &FileItem) -> String {
        crate::output::display_path(file, self.picker, self.scope)
    }

    pub fn live_file_count(&self) -> usize {
        self.picker.live_file_count_in(self.scope)
    }

    fn within(&self, path: String) -> Option<String> {
        if self.scope.is_root() {
            return Some(path);
        }
        self.scope.relative_path(&path).map(str::to_owned)
    }
}
