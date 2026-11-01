use fff::git::format_git_status;
use fff_service::program::{
    DirEntry, FileEntry, GrepCursor, GrepResults, Location, MixedEntry, ProgramReply, Score,
};
use serde::Serialize;

// A whole reply as one JSON document with the field names and values of the result structs,
// so a binding decodes it in one pass instead of reading every item through the FFI.
pub(crate) fn reply_json(reply: &ProgramReply) -> serde_json::Result<String> {
    match reply {
        ProgramReply::Files(results) => serde_json::to_string(&Files {
            items: results.items.iter().map(File::from).collect(),
            scores: results.scores.iter().map(Scored::from).collect(),
            total_matched: results.total_matched,
            total_files: results.total_files,
            location: results.location.map(Place::from),
        }),
        ProgramReply::Directories(results) => serde_json::to_string(&Dirs {
            items: results.items.iter().map(Dir::from).collect(),
            scores: results.scores.iter().map(Scored::from).collect(),
            total_matched: results.total_matched,
            total_dirs: results.total_dirs,
        }),
        ProgramReply::Mixed(results) => serde_json::to_string(&Mixed {
            items: results.items.iter().map(MixedItem::from).collect(),
            scores: results.scores.iter().map(Scored::from).collect(),
            total_matched: results.total_matched,
            total_files: results.total_files,
            total_dirs: results.total_dirs,
            location: results.location.map(Place::from),
        }),
        ProgramReply::Grep(results) => serde_json::to_string(&Grep::from(results)),
        ProgramReply::Ready(value) | ProgramReply::Scanning(value) => serde_json::to_string(value),
        ProgramReply::Progress(progress) => serde_json::to_string(&Progress {
            scanned_files_count: progress.files,
            is_scanning: progress.scanning,
            is_watcher_ready: progress.watcher_ready,
            is_warmup_complete: progress.warmup_complete,
        }),
        ProgramReply::Repository(root) => serde_json::to_string(root),
        ProgramReply::Done | ProgramReply::Error(_) => Ok("null".into()),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Files<'a> {
    items: Vec<File<'a>>,
    scores: Vec<Scored<'a>>,
    total_matched: usize,
    total_files: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    location: Option<Place>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Dirs<'a> {
    items: Vec<Dir<'a>>,
    scores: Vec<Scored<'a>>,
    total_matched: usize,
    total_dirs: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Mixed<'a> {
    items: Vec<MixedItem<'a>>,
    scores: Vec<Scored<'a>>,
    total_matched: usize,
    total_files: usize,
    total_dirs: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    location: Option<Place>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct File<'a> {
    relative_path: &'a str,
    file_name: &'a str,
    git_status: &'static str,
    size: u64,
    modified: u64,
    access_frecency_score: i32,
    modification_frecency_score: i32,
    total_frecency_score: i64,
}

impl<'a> From<&'a FileEntry> for File<'a> {
    fn from(file: &'a FileEntry) -> Self {
        Self {
            relative_path: &file.relative_path,
            file_name: &file.file_name,
            git_status: git_status(file.git_status),
            size: file.size,
            modified: file.modified,
            access_frecency_score: file.access_frecency_score,
            modification_frecency_score: file.modification_frecency_score,
            total_frecency_score: total_frecency(file),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Dir<'a> {
    relative_path: &'a str,
    dir_name: &'a str,
    max_access_frecency: i32,
}

impl<'a> From<&'a DirEntry> for Dir<'a> {
    fn from(dir: &'a DirEntry) -> Self {
        Self {
            relative_path: &dir.relative_path,
            dir_name: &dir.dir_name,
            max_access_frecency: dir.max_access_frecency,
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", content = "item", rename_all = "lowercase")]
enum MixedItem<'a> {
    File(File<'a>),
    Directory(Dir<'a>),
}

impl<'a> From<&'a MixedEntry> for MixedItem<'a> {
    fn from(entry: &'a MixedEntry) -> Self {
        match entry {
            MixedEntry::File(file) => Self::File(file.into()),
            MixedEntry::Dir(dir) => Self::Directory(dir.into()),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Scored<'a> {
    total: i32,
    base_score: i32,
    filename_bonus: i32,
    special_filename_bonus: i32,
    frecency_boost: i32,
    distance_penalty: i32,
    current_file_penalty: i32,
    combo_match_boost: i32,
    path_alignment_bonus: i32,
    exact_match: bool,
    match_type: &'a str,
}

impl<'a> From<&'a Score> for Scored<'a> {
    fn from(score: &'a Score) -> Self {
        Self {
            total: score.total,
            base_score: score.base_score,
            filename_bonus: score.filename_bonus,
            special_filename_bonus: score.special_filename_bonus,
            frecency_boost: score.frecency_boost,
            distance_penalty: score.distance_penalty,
            current_file_penalty: score.current_file_penalty,
            combo_match_boost: score.combo_match_boost,
            path_alignment_bonus: score.path_alignment_bonus,
            exact_match: score.exact_match,
            match_type: &score.match_type,
        }
    }
}

#[derive(Serialize)]
struct Position {
    line: i32,
    col: i32,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Place {
    Line { line: i32 },
    Position { line: i32, col: i32 },
    Range { start: Position, end: Position },
}

impl From<Location> for Place {
    fn from(location: Location) -> Self {
        match location {
            Location::Line(line) => Self::Line { line },
            Location::Position { line, col } => Self::Position { line, col },
            Location::Range { start, end } => Self::Range {
                start: Position {
                    line: start.0,
                    col: start.1,
                },
                end: Position {
                    line: end.0,
                    col: end.1,
                },
            },
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Grep<'a> {
    items: Vec<Match<'a>>,
    total_matched: usize,
    total_files_searched: usize,
    total_files: usize,
    filtered_file_count: usize,
    next_cursor: Option<&'a GrepCursor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    regex_fallback_error: Option<&'a str>,
}

impl<'a> From<&'a GrepResults> for Grep<'a> {
    fn from(results: &'a GrepResults) -> Self {
        let items: Vec<_> = results
            .matches
            .iter()
            .filter_map(|found| {
                let file = results.files.get(found.file)?;
                Some(Match {
                    file: file.into(),
                    is_binary: file.is_binary,
                    line_number: found.line_number,
                    col: found.col,
                    byte_offset: found.byte_offset,
                    line_content: &found.line_content,
                    match_ranges: &found.match_ranges,
                    fuzzy_score: found.fuzzy_score,
                    context_before: &found.context_before,
                    context_after: &found.context_after,
                    is_definition: found.is_definition,
                })
            })
            .collect();
        Self {
            total_matched: items.len(),
            items,
            total_files_searched: results.total_files_searched,
            total_files: results.total_files,
            filtered_file_count: results.filtered_file_count,
            next_cursor: results.next_cursor.as_ref(),
            regex_fallback_error: results.regex_fallback_error.as_deref(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Match<'a> {
    #[serde(flatten)]
    file: File<'a>,
    is_binary: bool,
    line_number: u64,
    col: usize,
    byte_offset: u64,
    line_content: &'a str,
    match_ranges: &'a [(u32, u32)],
    #[serde(skip_serializing_if = "Option::is_none")]
    fuzzy_score: Option<u16>,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    context_before: &'a [String],
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    context_after: &'a [String],
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    is_definition: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Progress {
    scanned_files_count: usize,
    is_scanning: bool,
    is_watcher_ready: bool,
    is_warmup_complete: bool,
}

pub(crate) fn git_status(bits: Option<u32>) -> &'static str {
    format_git_status(bits.map(git2::Status::from_bits_truncate))
}

pub(crate) fn total_frecency(file: &FileEntry) -> i64 {
    i64::from(file.access_frecency_score) + i64::from(file.modification_frecency_score)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fff_service::program::GrepMatch;
    use serde_json::{Value, json};

    fn file(path: &str, git_status: Option<u32>) -> FileEntry {
        FileEntry {
            relative_path: path.into(),
            file_name: path.rsplit('/').next().unwrap().into(),
            git_status,
            size: 3,
            modified: 4,
            access_frecency_score: 5,
            modification_frecency_score: 6,
            is_binary: false,
        }
    }

    fn parsed(reply: &ProgramReply) -> Value {
        serde_json::from_str(&reply_json(reply).unwrap()).unwrap()
    }

    #[test]
    fn grep_pages_carry_file_fields_per_match_and_their_cursor() {
        let results = GrepResults {
            files: vec![file("src/a.rs", Some(git2::Status::WT_NEW.bits()))],
            matches: vec![GrepMatch {
                file: 0,
                line_number: 7,
                col: 1,
                byte_offset: 40,
                line_content: "let needle".into(),
                match_ranges: vec![(4, 10)],
                fuzzy_score: None,
                is_definition: false,
                context_before: vec![],
                context_after: vec!["}".into()],
            }],
            total_files_searched: 1,
            total_files: 2,
            filtered_file_count: 2,
            next_cursor: Some(GrepCursor {
                index: "index".into(),
                offset: 1,
            }),
            regex_fallback_error: None,
        };
        let value = parsed(&ProgramReply::Grep(results));
        assert_eq!(value["nextCursor"], json!({"index": "index", "offset": 1}));
        assert_eq!(value["totalMatched"], 1);
        let found = &value["items"][0];
        assert_eq!(found["relativePath"], "src/a.rs");
        assert_eq!(found["gitStatus"], "untracked");
        assert_eq!(found["totalFrecencyScore"], 11);
        assert_eq!(found["matchRanges"], json!([[4, 10]]));
        assert_eq!(found["contextAfter"], json!(["}"]));
        assert!(found.get("contextBefore").is_none());
        assert!(found.get("fuzzyScore").is_none());
        assert!(found.get("isDefinition").is_none());
        assert!(value.get("regexFallbackError").is_none());
    }

    #[test]
    fn mixed_items_and_locations_take_the_bindings_shapes() {
        let results = fff_service::program::MixedResults {
            items: vec![
                MixedEntry::File(file("a.rs", None)),
                MixedEntry::Dir(DirEntry {
                    relative_path: "src/".into(),
                    dir_name: "src".into(),
                    max_access_frecency: 2,
                }),
            ],
            location: Some(Location::Range {
                start: (1, 2),
                end: (3, 4),
            }),
            ..Default::default()
        };
        let value = parsed(&ProgramReply::Mixed(results));
        assert_eq!(value["items"][0]["type"], "file");
        assert_eq!(value["items"][0]["item"]["gitStatus"], "clean");
        assert_eq!(value["items"][1]["type"], "directory");
        assert_eq!(value["items"][1]["item"]["dirName"], "src");
        assert_eq!(
            value["location"],
            json!({"type": "range", "start": {"line": 1, "col": 2}, "end": {"line": 3, "col": 4}})
        );
    }
}
