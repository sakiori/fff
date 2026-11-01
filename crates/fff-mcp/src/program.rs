use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use fff::file_picker::FilePicker;
use fff::grep::{GrepResult, GrepSearchOptions};
use fff::scope::SearchScope;
use fff::types::{DirItem, FileItem, MixedItemRef, PaginationArgs};
use fff::{FuzzySearchOptions, QueryParser};
use fff_query_parser::{AiGrepConfig, DirSearchConfig, MixedSearchConfig};
use fff_service::program::{
    DefinitionMode, DirEntry, DirResults, FileEntry, FileResults, FrameError, GlobRequest,
    GrepCursor, GrepMatch, GrepMode, GrepOptions, GrepResults, Location, MAX_REQUEST_BYTES,
    MAX_WAIT_READY, MixedEntry, MixedResults, ProgramError, ProgramReply, ProgramRequest,
    ScanProgress, Score, SearchRequest, read_frame, write_frame,
};
use fff_service::protocol::Watcher;
use interprocess::local_socket::tokio::{RecvHalf, SendHalf};
use parking_lot::Mutex;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::time::Instant;

use crate::registry::{Binding, Session, SessionError};

// Serves one program session until the program closes it or its connection ends.
pub(crate) async fn serve(
    session: Arc<Session>,
    mut recv: BufReader<RecvHalf>,
    mut send: SendHalf,
) -> io::Result<()> {
    let calls = Calls {
        session,
        counted: Arc::default(),
    };
    loop {
        let request = match read_frame(&mut recv, MAX_REQUEST_BYTES).await {
            Ok(ProgramRequest::Close) => return write_frame(&mut send, &ProgramReply::Done).await,
            Ok(request) => request,
            Err(FrameError::Closed) => return Ok(()),
            Err(FrameError::Io(error)) => return Err(error),
            // The rest of an oversized frame was not read, so the stream cannot continue.
            Err(error @ FrameError::TooLarge(_)) => {
                let reply = ProgramReply::Error(ProgramError::Invalid(error.to_string()));
                return write_frame(&mut send, &reply).await;
            }
            Err(error) => {
                let reply = ProgramReply::Error(ProgramError::Invalid(error.to_string()));
                write_frame(&mut send, &reply).await?;
                continue;
            }
        };
        // A program sends nothing while it waits for a reply, so readable data is the next
        // request and the end of the stream means the program is gone.
        let abort = Arc::new(AtomicBool::new(false));
        let reply = tokio::select! {
            reply = calls.handle(request, abort.clone()) => reply.unwrap_or_else(ProgramReply::Error),
            () = closed(&mut recv) => {
                abort.store(true, Ordering::Relaxed);
                return Ok(());
            }
        };
        write_frame(&mut send, &reply).await?;
    }
}

// Resolves once the connection ends or fails, without consuming a request.
async fn closed(recv: &mut BufReader<RecvHalf>) {
    match recv.fill_buf().await {
        Ok([]) | Err(_) => {}
        Ok(_) => std::future::pending().await,
    }
}

struct Calls {
    session: Arc<Session>,
    // The session's file count in the index that last gave it without waiting.
    counted: Arc<Mutex<Option<(String, usize)>>>,
}

impl Calls {
    async fn handle(
        &self,
        request: ProgramRequest,
        abort: Arc<AtomicBool>,
    ) -> Result<ProgramReply, ProgramError> {
        match request {
            ProgramRequest::WaitReady { timeout_ms } => {
                let timeout = Duration::from_millis(timeout_ms).min(MAX_WAIT_READY);
                let deadline = Instant::now() + timeout;
                match self.session.prepare_until(deadline).await {
                    Ok(binding) => Ok(ProgramReply::Ready(
                        binding.index.wait_scanned(deadline).await,
                    )),
                    Err(SessionError::Building) => Ok(ProgramReply::Ready(false)),
                    Err(error) => Err(session_error(error)),
                }
            }
            ProgramRequest::IsScanning => {
                let index = self.session.snapshot().index.clone();
                Ok(ProgramReply::Scanning(!index.is_ready() || index.is_busy()))
            }
            ProgramRequest::Progress => {
                let binding = self.session.snapshot();
                let counted = self.counted.clone();
                let progress = tokio::task::spawn_blocking(move || progress(&binding, &counted))
                    .await
                    .map_err(|error| ProgramError::Failed(error.to_string()))?;
                Ok(ProgramReply::Progress(progress))
            }
            ProgramRequest::Rescan => {
                let index = self.session.snapshot().index.clone();
                tokio::task::spawn_blocking(move || index.rescan())
                    .await
                    .map_err(|error| ProgramError::Failed(error.to_string()))?
                    .map_err(ProgramError::Service)?;
                Ok(ProgramReply::Done)
            }
            ProgramRequest::Repository => {
                let session = self.session.clone();
                let root = tokio::task::spawn_blocking(move || session.repository())
                    .await
                    .map_err(|error| ProgramError::Failed(error.to_string()))?;
                Ok(ProgramReply::Repository(root))
            }
            ProgramRequest::Changed { paths } => {
                let session = self.session.clone();
                tokio::task::spawn_blocking(move || changed(&session, paths))
                    .await
                    .map_err(|error| ProgramError::Failed(error.to_string()))?
            }
            ProgramRequest::Close => Ok(ProgramReply::Done),
            search => self.search(search, abort).await,
        }
    }

    async fn search(
        &self,
        request: ProgramRequest,
        abort: Arc<AtomicBool>,
    ) -> Result<ProgramReply, ProgramError> {
        let binding = self.session.current().await.map_err(session_error)?;
        let cursor = match &request {
            ProgramRequest::Grep(grep) => grep.options.cursor.as_ref(),
            ProgramRequest::MultiGrep(grep) => grep.options.cursor.as_ref(),
            _ => None,
        };
        // Offsets index one index's file order; another index would skip or repeat files.
        if cursor.is_some_and(|cursor| cursor.index != binding.index.id) {
            return Err(ProgramError::StaleCursor);
        }
        tokio::task::spawn_blocking(move || run(&binding, request, abort))
            .await
            .map_err(|error| ProgramError::Failed(error.to_string()))?
    }
}

// Applies reported changes to the session's current index, once every path is known to
// lie inside the session's directory.
fn changed(session: &Session, paths: Vec<PathBuf>) -> Result<ProgramReply, ProgramError> {
    let binding = session.snapshot();
    let resolved = paths
        .into_iter()
        .map(|path| {
            let inside = resolve_reported(&session.scope, &path)
                .map(|reported| match reported {
                    Reported::OnDisk(path) => path,
                    Reported::Missing { path, fold_case } if fold_case => {
                        indexed_spelling(&binding, &path).unwrap_or(path)
                    }
                    Reported::Missing { path, .. } => path,
                })
                .filter(|resolved| resolved.starts_with(&session.scope));
            inside.ok_or(ProgramError::OutsideScope(path))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if !resolved.is_empty() {
        binding.index.apply_changes(resolved);
    }
    Ok(ProgramReply::Done)
}

// A reported path as the index spells it, or, for one that no longer exists, as far as the
// filesystem can tell.
enum Reported {
    OnDisk(PathBuf),
    // `fold_case` when the nearest existing directory ignores case, so the indexed entry may
    // be spelled otherwise.
    Missing { path: PathBuf, fold_case: bool },
}

// Spells a reported path as the filesystem does: lexically normalised, then resolved like the
// index's paths, with every component in its on-disk case. A symlink stays the link the
// index holds; only its name's case is taken from its directory. The caller refuses a result
// outside the session's directory, also one reached through a symlink.
fn resolve_reported(scope: &Path, path: &Path) -> Option<Reported> {
    let mut normal = PathBuf::new();
    for component in scope.join(path).components() {
        match component {
            Component::ParentDir => {
                if !normal.pop() {
                    return None;
                }
            }
            Component::CurDir => {}
            component => normal.push(component),
        }
    }
    match std::fs::symlink_metadata(&normal) {
        Ok(metadata) if !metadata.file_type().is_symlink() => {
            fff::path_utils::canonicalize(&normal)
                .ok()
                .map(Reported::OnDisk)
        }
        Ok(_) => {
            let parent = fff::path_utils::canonicalize(normal.parent()?).ok()?;
            let name = on_disk_name(&parent, normal.file_name()?);
            Some(Reported::OnDisk(parent.join(name)))
        }
        Err(_) => {
            let mut existing = normal.as_path();
            let mut rest = Vec::new();
            let base = loop {
                rest.push(existing.file_name()?);
                existing = existing.parent()?;
                if let Ok(resolved) = fff::path_utils::canonicalize(existing) {
                    break resolved;
                }
            };
            let fold_case = ignores_case(&base);
            let mut path = base;
            path.extend(rest.into_iter().rev());
            Some(Reported::Missing { path, fold_case })
        }
    }
}

// The name of `name`'s entry in `dir` as the directory spells it, which differs only where
// the filesystem ignores case.
fn on_disk_name(dir: &Path, name: &std::ffi::OsStr) -> std::ffi::OsString {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return name.to_owned();
    };
    let names: Vec<_> = entries.flatten().map(|entry| entry.file_name()).collect();
    if names.iter().any(|entry| entry == name) {
        return name.to_owned();
    }
    let wanted = name.to_string_lossy();
    names
        .into_iter()
        .find(|entry| entry.to_string_lossy().eq_ignore_ascii_case(&wanted))
        .unwrap_or_else(|| name.to_owned())
}

// Whether names in `dir` match regardless of case.
#[cfg(target_os = "macos")]
fn ignores_case(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(dir) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: pathconf reads a NUL-terminated path.
    unsafe { libc::pathconf(dir.as_ptr(), libc::_PC_CASE_SENSITIVE) == 0 }
}

#[cfg(windows)]
fn ignores_case(_dir: &Path) -> bool {
    true
}

// Linux directories are case-sensitive unless marked casefold, which the index does not
// distinguish either.
#[cfg(not(any(target_os = "macos", windows)))]
fn ignores_case(_dir: &Path) -> bool {
    false
}

// The indexed spelling of a deleted file, or of a deleted directory's prefix, that matches
// `path` regardless of ASCII case.
fn indexed_spelling(binding: &Binding, path: &Path) -> Option<PathBuf> {
    let guard = binding.index.picker.read().ok()?;
    let picker = guard.as_ref()?;
    let base = picker.base_path();
    let wanted = fff::path_utils::to_canonical_slashes(path.strip_prefix(base).ok()?.to_str()?);
    let length = wanted.len();
    picker
        .get_files()
        .iter()
        .filter(|file| !file.is_deleted())
        .find_map(|file| {
            let indexed = file.relative_path(picker);
            let prefix = indexed.get(..length)?;
            let whole = indexed.len() == length;
            let below = indexed.as_bytes().get(length) == Some(&b'/');
            ((whole || below) && prefix.eq_ignore_ascii_case(&wanted)).then(|| base.join(prefix))
        })
}

fn run(
    binding: &Binding,
    request: ProgramRequest,
    abort: Arc<AtomicBool>,
) -> Result<ProgramReply, ProgramError> {
    #[cfg(debug_assertions)]
    hold_search(binding);
    let guard = binding
        .index
        .picker
        .read()
        .map_err(|error| ProgramError::Failed(error.to_string()))?;
    let Some(picker) = guard.as_ref() else {
        return empty(&request);
    };
    let scope = &binding.scope;
    let reply = match request {
        ProgramRequest::SearchFiles(request) => {
            let query = QueryParser::default().parse(&request.query);
            let current_file = index_path(scope, request.current_file.as_deref());
            let options = fuzzy_options(picker, scope, &request, current_file.as_deref());
            let result = picker.fuzzy_search(&query, None, options);
            ProgramReply::Files(FileResults {
                items: files(&result.items, picker, scope),
                scores: result.scores.iter().map(score).collect(),
                total_matched: result.total_matched,
                total_files: result.total_files,
                location: result.location.map(location),
            })
        }
        ProgramRequest::SearchDirectories(request) => {
            let query = QueryParser::new(DirSearchConfig).parse(&request.query);
            let current_file = index_path(scope, request.current_file.as_deref());
            let options = fuzzy_options(picker, scope, &request, current_file.as_deref());
            let result = picker.fuzzy_search_directories(&query, options);
            ProgramReply::Directories(DirResults {
                items: result
                    .items
                    .iter()
                    .map(|dir| dir_entry(dir, picker, scope))
                    .collect(),
                scores: result.scores.iter().map(score).collect(),
                total_matched: result.total_matched,
                total_dirs: result.total_dirs,
            })
        }
        ProgramRequest::SearchMixed(request) => {
            let query = QueryParser::new(MixedSearchConfig).parse(&request.query);
            let current_file = index_path(scope, request.current_file.as_deref());
            let options = fuzzy_options(picker, scope, &request, current_file.as_deref());
            let result = picker.fuzzy_search_mixed(&query, None, options);
            let entry = |item: &MixedItemRef| match item {
                MixedItemRef::File(file) => MixedEntry::File(file_entry(file, picker, scope)),
                MixedItemRef::Dir(dir) => MixedEntry::Dir(dir_entry(dir, picker, scope)),
            };
            ProgramReply::Mixed(MixedResults {
                items: result.items.iter().map(entry).collect(),
                scores: result.scores.iter().map(score).collect(),
                total_matched: result.total_matched,
                total_files: result.total_files,
                total_dirs: result.total_dirs,
                location: result.location.map(location),
            })
        }
        ProgramRequest::Glob(request) => glob(picker, scope, &request)?,
        ProgramRequest::Grep(request) => {
            let query = if request.ai_syntax {
                QueryParser::new(AiGrepConfig).parse(&request.query)
            } else {
                fff::grep::parse_grep_query(&request.query)
            };
            let options = grep_options(&request.options, request.mode, scope, abort);
            let result = picker.grep(&query, &options);
            ProgramReply::Grep(grep_results(&result, picker, scope, &binding.index.id))
        }
        ProgramRequest::MultiGrep(request) => {
            let patterns: Vec<&str> = request.patterns.iter().map(String::as_str).collect();
            if patterns.iter().all(|pattern| pattern.is_empty()) {
                return Err(ProgramError::Invalid("patterns must not be empty".into()));
            }
            let constraints = QueryParser::new(AiGrepConfig)
                .parse_constraints(request.constraints.as_deref().unwrap_or(""));
            let options = grep_options(&request.options, GrepMode::PlainText, scope, abort);
            let result = picker.multi_grep(&patterns, &constraints, &options);
            ProgramReply::Grep(grep_results(&result, picker, scope, &binding.index.id))
        }
        _ => return Err(ProgramError::Invalid("not a search request".into())),
    };
    Ok(reply)
}

// Reads no lock a search or an index update holds: scan state and counts come from atomics,
// and a session's file count the index cannot give at once is the last one it gave.
fn progress(binding: &Binding, counted: &Mutex<Option<(String, usize)>>) -> ScanProgress {
    let index = &binding.index;
    let scanning = !index.is_ready() || index.is_busy();
    let files = if scanning {
        index.scanned_files()
    } else {
        let mut counted = counted.lock();
        let fresh = index.picker.try_read().and_then(|guard| {
            guard
                .as_ref()
                .map(|picker| picker.live_file_count_in(&binding.scope))
        });
        match (fresh, counted.as_ref()) {
            (Some(files), _) => {
                *counted = Some((index.id.clone(), files));
                files
            }
            (None, Some((id, files))) if *id == index.id => *files,
            (None, _) => index.scanned_files(),
        }
    };
    ScanProgress {
        files,
        scanning,
        watcher_ready: index.watcher() == Watcher::Ready,
        // The content index is built before the scan job ends.
        warmup_complete: !index.options.content_indexing || !scanning,
    }
}

// An empty reply of the request's kind.
fn empty(request: &ProgramRequest) -> Result<ProgramReply, ProgramError> {
    Ok(match request {
        ProgramRequest::SearchFiles(_) | ProgramRequest::Glob(_) => {
            ProgramReply::Files(FileResults::default())
        }
        ProgramRequest::SearchDirectories(_) => ProgramReply::Directories(DirResults::default()),
        ProgramRequest::SearchMixed(_) => ProgramReply::Mixed(MixedResults::default()),
        ProgramRequest::Grep(_) | ProgramRequest::MultiGrep(_) => {
            ProgramReply::Grep(GrepResults::default())
        }
        _ => return Err(ProgramError::Invalid("not a search request".into())),
    })
}

fn glob(
    picker: &FilePicker,
    scope: &SearchScope,
    request: &GlobRequest,
) -> Result<ProgramReply, ProgramError> {
    if request.pattern.is_empty() {
        return Err(ProgramError::Invalid("the glob pattern is empty".into()));
    }
    let current_file = index_path(scope, request.current_file.as_deref());
    let options = FuzzySearchOptions {
        current_file: current_file.as_deref(),
        project_path: Some(picker.shared_base_path()),
        pagination: PaginationArgs {
            offset: request.offset,
            limit: request.limit,
        },
        scope,
        exclude_frecency: request.exclude_frecency,
        ..Default::default()
    };
    let result = picker.glob(&request.pattern, options);
    Ok(ProgramReply::Files(FileResults {
        items: files(&result.items, picker, scope),
        scores: result.scores.iter().map(score).collect(),
        total_matched: result.total_matched,
        total_files: result.total_files,
        location: result.location.map(location),
    }))
}

fn fuzzy_options<'a>(
    picker: &'a FilePicker,
    scope: &'a SearchScope,
    request: &SearchRequest,
    current_file: Option<&'a str>,
) -> FuzzySearchOptions<'a> {
    FuzzySearchOptions {
        current_file,
        project_path: Some(picker.shared_base_path()),
        pagination: PaginationArgs {
            offset: request.offset,
            limit: request.limit,
        },
        scope,
        exclude_frecency: request.exclude_frecency,
        ..Default::default()
    }
}

fn grep_options(
    options: &GrepOptions,
    mode: GrepMode,
    scope: &SearchScope,
    abort: Arc<AtomicBool>,
) -> GrepSearchOptions {
    GrepSearchOptions {
        max_file_size: options.max_file_size,
        max_matches_per_file: options.max_matches_per_file,
        smart_case: options.smart_case,
        file_offset: options.cursor.as_ref().map_or(0, |cursor| cursor.offset),
        page_limit: options.page_size,
        mode: match mode {
            GrepMode::PlainText => fff::grep::GrepMode::PlainText,
            GrepMode::Regex => fff::grep::GrepMode::Regex,
            GrepMode::Fuzzy => fff::grep::GrepMode::Fuzzy,
        },
        time_budget_ms: options.time_budget_ms,
        enforce_time_budget: options.enforce_time_budget,
        before_context: options.before_context,
        after_context: options.after_context,
        classify_definitions: match options.classify_definitions {
            DefinitionMode::Off => fff::DefinitionMode::Off,
            DefinitionMode::Classify => fff::DefinitionMode::Classify,
            DefinitionMode::DefinitionOnly => fff::DefinitionMode::DefinitionOnly,
            DefinitionMode::UsageOnly => fff::DefinitionMode::UsageOnly,
        },
        exclude_frecency: options.exclude_frecency,
        scope: scope.clone(),
        abort_signal: Some(abort),
        ..Default::default()
    }
}

fn grep_results(
    result: &GrepResult,
    picker: &FilePicker,
    scope: &SearchScope,
    index: &str,
) -> GrepResults {
    GrepResults {
        files: files(&result.files, picker, scope),
        matches: result
            .matches
            .iter()
            .map(|found| GrepMatch {
                file: found.file_index,
                line_number: found.line_number,
                col: found.col,
                byte_offset: found.byte_offset,
                line_content: found.line_content.clone(),
                match_ranges: found.match_byte_offsets.to_vec(),
                fuzzy_score: found.fuzzy_score,
                is_definition: found.is_definition,
                context_before: found.context_before.clone(),
                context_after: found.context_after.clone(),
            })
            .collect(),
        total_files_searched: result.total_files_searched,
        total_files: result.total_files,
        filtered_file_count: result.filtered_file_count,
        next_cursor: (result.next_file_offset > 0).then(|| GrepCursor {
            index: index.to_owned(),
            offset: result.next_file_offset,
        }),
        regex_fallback_error: result.regex_fallback_error.clone(),
    }
}

fn files(items: &[&FileItem], picker: &FilePicker, scope: &SearchScope) -> Vec<FileEntry> {
    items
        .iter()
        .map(|file| file_entry(file, picker, scope))
        .collect()
}

fn file_entry(file: &FileItem, picker: &FilePicker, scope: &SearchScope) -> FileEntry {
    FileEntry {
        relative_path: scope_relative(scope, file.relative_path(picker)),
        file_name: file.file_name(picker),
        git_status: file.git_status.map(|status| status.bits()),
        size: file.size,
        modified: file.modified,
        access_frecency_score: file.access_frecency_score.into(),
        modification_frecency_score: file.modification_frecency_score.into(),
        is_binary: file.is_binary(),
    }
}

fn dir_entry(dir: &DirItem, picker: &FilePicker, scope: &SearchScope) -> DirEntry {
    DirEntry {
        relative_path: scope_relative(scope, dir.relative_path(picker)),
        dir_name: dir.dir_name(picker),
        max_access_frecency: dir.max_access_frecency(),
    }
}

fn score(score: &fff::types::Score) -> Score {
    Score {
        total: score.total,
        base_score: score.base_score,
        filename_bonus: score.filename_bonus,
        special_filename_bonus: score.special_filename_bonus,
        frecency_boost: score.frecency_boost,
        git_status_boost: score.git_status_boost,
        git_recency_boost: score.git_recency_boost,
        distance_penalty: score.distance_penalty,
        current_file_penalty: score.current_file_penalty,
        combo_match_boost: score.combo_match_boost,
        path_alignment_bonus: score.path_alignment_bonus,
        exact_match: score.exact_match,
        match_type: score.match_type.to_owned(),
    }
}

fn location(location: fff_query_parser::Location) -> Location {
    match location {
        fff_query_parser::Location::Line(line) => Location::Line(line),
        fff_query_parser::Location::Position { line, col } => Location::Position { line, col },
        fff_query_parser::Location::Range { start, end } => Location::Range { start, end },
    }
}

fn session_error(error: SessionError) -> ProgramError {
    match error {
        SessionError::Building => ProgramError::Building,
        SessionError::Unstable => ProgramError::Unstable,
        SessionError::Service(error) => ProgramError::Service(error),
        SessionError::Task(error) => ProgramError::Failed(error.to_string()),
    }
}

// Results outside the scope would be a search bug; they keep their index path.
fn scope_relative(scope: &SearchScope, path: String) -> String {
    match scope.relative_path(&path) {
        Some(relative) => relative.to_owned(),
        None => {
            tracing::error!(path, "Search result outside the session scope");
            path
        }
    }
}

// A path relative to the session's directory as the index stores it.
fn index_path(scope: &SearchScope, path: Option<&str>) -> Option<String> {
    path.map(|path| format!("{}{path}", scope.dir()))
}

// In debug builds, a search holds its index locked for writing while the file named by
// `FFF_MCP_TEST_HOLD_SEARCH` exists, as a long search does with an index update queued.
#[cfg(debug_assertions)]
fn hold_search(binding: &Binding) {
    let Some(file) = std::env::var_os("FFF_MCP_TEST_HOLD_SEARCH") else {
        return;
    };
    let file = std::path::Path::new(&file);
    if !file.exists() {
        return;
    }
    let _guard = binding.index.picker.write();
    let mut held = file.as_os_str().to_owned();
    held.push(".held");
    let _ = std::fs::write(&held, "");
    while file.exists() {
        std::thread::sleep(Duration::from_millis(10));
    }
}
