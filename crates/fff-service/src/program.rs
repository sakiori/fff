use std::io;
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::protocol::ServiceError;

/// Largest request frame the service reads.
pub const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;
/// Largest reply frame a client reads.
pub const MAX_REPLY_BYTES: usize = 1024 * 1024 * 1024;
/// Longest readiness wait the service grants one request.
pub const MAX_WAIT_READY: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// One call of a program session; the service answers each with one [`ProgramReply`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ProgramRequest {
    SearchFiles(SearchRequest),
    SearchDirectories(SearchRequest),
    SearchMixed(SearchRequest),
    Glob(GlobRequest),
    Grep(GrepRequest),
    MultiGrep(MultiGrepRequest),
    /// Waits until the index is ready, its watcher settled and no scan runs or waits to run,
    /// for at most [`MAX_WAIT_READY`].
    WaitReady {
        timeout_ms: u64,
    },
    IsScanning,
    /// Scan state of the index, with the files of the session's directory once it is ready.
    Progress,
    /// Requests a full rescan of the shared index. During a scan it starts one more rescan
    /// right after that scan, which later requests join.
    Rescan,
    /// The git work tree containing the session's directory.
    Repository,
    /// Applies changes the program made to `paths`, absolute or relative to the session's
    /// directory and inside it, to the index as the watcher would, without a rescan. The
    /// reply follows once the index holds them, or, while a scan runs, once they wait to be
    /// applied when it commits. New files past what the index takes in without a full rescan
    /// wait for that rescan, as the watcher's do.
    Changed {
        paths: Vec<PathBuf>,
    },
    Close,
}

/// A fuzzy search page. `current_file` is relative to the session's directory.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    pub current_file: Option<String>,
    pub offset: usize,
    pub limit: usize,
    pub exclude_frecency: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GlobRequest {
    pub pattern: String,
    pub current_file: Option<String>,
    pub offset: usize,
    pub limit: usize,
    pub exclude_frecency: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum GrepMode {
    #[default]
    PlainText,
    Regex,
    Fuzzy,
}

/// Which matches a grep returns and whether they are classified as definitions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DefinitionMode {
    #[default]
    Off,
    Classify,
    DefinitionOnly,
    UsageOnly,
}

/// Where a grep continues: the core's file offset into the file order of the index `index`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepCursor {
    pub index: String,
    pub offset: usize,
}

/// Content search options as the core takes them; `cursor` continues an earlier page.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GrepOptions {
    pub max_file_size: u64,
    pub max_matches_per_file: usize,
    pub smart_case: bool,
    pub cursor: Option<GrepCursor>,
    pub page_size: usize,
    pub time_budget_ms: u64,
    pub enforce_time_budget: bool,
    pub before_context: usize,
    pub after_context: usize,
    pub classify_definitions: DefinitionMode,
    pub exclude_frecency: bool,
}

/// `ai_syntax` parses constraints as instances in AI mode do.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GrepRequest {
    pub query: String,
    pub mode: GrepMode,
    pub ai_syntax: bool,
    pub options: GrepOptions,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MultiGrepRequest {
    pub patterns: Vec<String>,
    pub constraints: Option<String>,
    pub options: GrepOptions,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ProgramReply {
    Files(FileResults),
    Directories(DirResults),
    Mixed(MixedResults),
    Grep(GrepResults),
    Ready(bool),
    Scanning(bool),
    Progress(ScanProgress),
    Done,
    Repository(Option<PathBuf>),
    Error(ProgramError),
}

/// `files` counts the session's files once the index is ready, and the files scanned so far before.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanProgress {
    pub files: usize,
    pub scanning: bool,
    pub watcher_ready: bool,
    pub warmup_complete: bool,
}

/// A file as the core indexes it; `git_status` holds the raw git status bits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub relative_path: String,
    pub file_name: String,
    pub git_status: Option<u32>,
    pub size: u64,
    pub modified: u64,
    pub access_frecency_score: i32,
    pub modification_frecency_score: i32,
    pub is_binary: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub relative_path: String,
    pub dir_name: String,
    pub max_access_frecency: i32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MixedEntry {
    File(FileEntry),
    Dir(DirEntry),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Score {
    pub total: i32,
    pub base_score: i32,
    pub filename_bonus: i32,
    pub special_filename_bonus: i32,
    pub frecency_boost: i32,
    pub git_status_boost: i32,
    pub git_recency_boost: i32,
    pub distance_penalty: i32,
    pub current_file_penalty: i32,
    pub combo_match_boost: i32,
    pub path_alignment_bonus: i32,
    pub exact_match: bool,
    pub match_type: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Location {
    Line(i32),
    Position { line: i32, col: i32 },
    Range { start: (i32, i32), end: (i32, i32) },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileResults {
    pub items: Vec<FileEntry>,
    pub scores: Vec<Score>,
    pub total_matched: usize,
    pub total_files: usize,
    pub location: Option<Location>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirResults {
    pub items: Vec<DirEntry>,
    pub scores: Vec<Score>,
    pub total_matched: usize,
    pub total_dirs: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MixedResults {
    pub items: Vec<MixedEntry>,
    pub scores: Vec<Score>,
    pub total_matched: usize,
    pub total_files: usize,
    pub total_dirs: usize,
    pub location: Option<Location>,
}

/// A matching line; `file` indexes [`GrepResults::files`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepMatch {
    pub file: usize,
    pub line_number: u64,
    pub col: usize,
    pub byte_offset: u64,
    pub line_content: String,
    pub match_ranges: Vec<(u32, u32)>,
    pub fuzzy_score: Option<u16>,
    pub is_definition: bool,
    pub context_before: Vec<String>,
    pub context_after: Vec<String>,
}

/// One grep page; `next_cursor` is `None` once every file was searched.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepResults {
    pub files: Vec<FileEntry>,
    pub matches: Vec<GrepMatch>,
    pub total_files_searched: usize,
    pub total_files: usize,
    pub filtered_file_count: usize,
    pub next_cursor: Option<GrepCursor>,
    pub regex_fallback_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize)]
pub enum ProgramError {
    #[error("the index is still building; retry shortly")]
    Building,
    #[error("the index coverage is changing; retry shortly")]
    Unstable,
    #[error("the cursor belongs to an index this session no longer uses; search again without it")]
    StaleCursor,
    #[error("{} is outside the session's directory", .0.display())]
    OutsideScope(PathBuf),
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error(transparent)]
    Service(ServiceError),
    #[error("{0}")]
    Failed(String),
    /// An error of a newer build.
    #[error("the service reported an error this client does not know")]
    Unknown,
}

#[derive(Deserialize)]
#[serde(remote = "ProgramError")]
enum KnownProgramError {
    Building,
    Unstable,
    StaleCursor,
    OutsideScope(PathBuf),
    Invalid(String),
    Service(ServiceError),
    Failed(String),
}

impl<'de> Deserialize<'de> for ProgramError {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Known(#[serde(with = "KnownProgramError")] ProgramError),
            Unknown(serde::de::IgnoredAny),
        }
        Ok(match Wire::deserialize(deserializer)? {
            Wire::Known(error) => error,
            Wire::Unknown(_) => ProgramError::Unknown,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("connection closed")]
    Closed,
    #[error("frame of {0} bytes exceeds the limit")]
    TooLarge(usize),
    #[error("malformed frame: {0}")]
    Malformed(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Writes `value` as a big-endian `u32` length followed by its MessagePack encoding.
pub async fn write_frame<T: Serialize, W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &T,
) -> io::Result<()> {
    let body = rmp_serde::to_vec(value).map_err(io::Error::other)?;
    let length = u32::try_from(body.len())
        .map_err(|_| io::Error::other(format!("frame of {} bytes is too large", body.len())))?;
    writer.write_all(&length.to_be_bytes()).await?;
    writer.write_all(&body).await?;
    writer.flush().await
}

/// Reads one frame; a connection that ends between frames is `Closed`.
pub async fn read_frame<T: DeserializeOwned, R: AsyncRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> Result<T, FrameError> {
    let mut header = [0; 4];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
            return Err(FrameError::Closed);
        }
        Err(error) => return Err(error.into()),
    }
    let length = u32::from_be_bytes(header) as usize;
    if length > limit {
        return Err(FrameError::TooLarge(length));
    }
    // A truncated frame must not allocate the length it declares.
    let mut body = Vec::new();
    reader.take(length as u64).read_to_end(&mut body).await?;
    if body.len() < length {
        return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into());
    }
    rmp_serde::from_slice(&body).map_err(|error| FrameError::Malformed(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip_and_reject_oversized_or_truncated_input() {
        let request = ProgramRequest::Grep(GrepRequest {
            query: "needle".into(),
            mode: GrepMode::Regex,
            options: GrepOptions {
                cursor: Some(GrepCursor {
                    index: "index".into(),
                    offset: 7,
                }),
                ..Default::default()
            },
            ..Default::default()
        });
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &request).await.unwrap();
        write_frame(&mut bytes, &ProgramRequest::Close)
            .await
            .unwrap();
        let mut reader = &bytes[..];
        let ProgramRequest::Grep(grep) = read_frame(&mut reader, MAX_REQUEST_BYTES).await.unwrap()
        else {
            panic!("decoded another request");
        };
        assert_eq!(grep.query, "needle");
        assert_eq!(grep.options.cursor.map(|c| c.offset), Some(7));
        assert!(matches!(
            read_frame(&mut reader, MAX_REQUEST_BYTES).await,
            Ok(ProgramRequest::Close)
        ));
        assert!(matches!(
            read_frame::<ProgramRequest, _>(&mut reader, MAX_REQUEST_BYTES).await,
            Err(FrameError::Closed)
        ));
        assert!(matches!(
            read_frame::<ProgramRequest, _>(&mut &bytes[..], 2).await,
            Err(FrameError::TooLarge(_))
        ));
        assert!(matches!(
            read_frame::<ProgramRequest, _>(&mut &bytes[..6], MAX_REQUEST_BYTES).await,
            Err(FrameError::Io(_))
        ));
    }

    #[test]
    fn errors_round_trip() {
        let reply = ProgramReply::Error(ProgramError::Service(ServiceError::Busy { sessions: 2 }));
        let bytes = rmp_serde::to_vec(&reply).unwrap();
        let ProgramReply::Error(error) = rmp_serde::from_slice(&bytes).unwrap() else {
            panic!("decoded another reply");
        };
        assert_eq!(
            error,
            ProgramError::Service(ServiceError::Busy { sessions: 2 })
        );
    }

    #[test]
    fn error_kinds_of_a_newer_build_decode_as_unknown() {
        #[derive(Serialize)]
        enum Newer {
            Overloaded { retry_ms: u64 },
        }
        let bytes = rmp_serde::to_vec(&Newer::Overloaded { retry_ms: 5 }).unwrap();
        let error: ProgramError = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(error, ProgramError::Unknown);
        let bytes = rmp_serde::to_vec(&ProgramError::StaleCursor).unwrap();
        let error: ProgramError = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(error, ProgramError::StaleCursor);
        let text = r#"{"kind":"overloaded","retry_ms":5}"#;
        let error: ServiceError = serde_json::from_str(text).unwrap();
        assert_eq!(error, ServiceError::Unknown);
        let text = r#"{"reply":"error","error":{"kind":"overloaded"}}"#;
        let reply: crate::protocol::Reply = serde_json::from_str(text).unwrap();
        assert!(matches!(
            reply,
            crate::protocol::Reply::Error {
                error: ServiceError::Unknown
            }
        ));
    }
}
