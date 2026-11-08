//! C FFI bindings for fff-core, usable from any language with C FFI
//! (Bun, Node.js, Python, Ruby, etc.).
//!
//! All state is owned by an opaque instance handle: create with
//! `fff_create_instance*`, pass to every call, free with `fff_destroy`.
//! Multiple instances can coexist in one process.
//!
//! Conventions: every returned `*mut FffResult` is freed with
//! `fff_free_result`; optional string params take NULL/empty; numeric 0 means
//! "use default" unless documented otherwise; grep mode `u8` is 0 = plain
//! text, 1 = regex, 2 = fuzzy; multi-grep patterns are `\n`-separated.

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::{Path, PathBuf};
use std::time::Duration;

use fff::shared::SharedQueryTracker;
use fff_service::program::{
    GlobRequest, GrepOptions, GrepRequest, MultiGrepRequest, SearchRequest,
};
use fff_service::protocol::IndexOptions;

mod accessors;
mod ffi_types;
mod packed;
mod service;
mod watch;

use fff::file_picker::{FilePicker, FilePickerIgnoreOptions};
use fff::frecency::FrecencyTracker;
use fff::query_tracker::QueryTracker;
use fff::{DbHealthChecker, FFFMode, FuzzySearchOptions, PaginationArgs, QueryParser};
use fff::{SharedFilePicker, SharedFrecency};
use ffi_types::{
    FFF_CREATE_OPTIONS_VERSION, FFF_ERROR_INVALID_REQUEST, FFF_ERROR_SCOPE_UNAVAILABLE,
    FffCreateOptions, FffDirItem, FffDirSearchResult, FffFileItem, FffGrepMatch, FffGrepResult,
    FffMixedItem, FffMixedSearchResult, FffResult, FffScanProgress, FffScore, FffSearchResult,
};

/// Opaque handle holding all per-instance state; freed by `fff_destroy`.
enum FffInstance {
    Local(LocalInstance),
    Service(Box<service::ServiceInstance>),
}

// An instance owning its index in this process.
struct LocalInstance {
    picker: SharedFilePicker,
    frecency: SharedFrecency,
    query_tracker: SharedQueryTracker,
    // we keep a single callback type
    watch_callback: std::sync::Arc<watch::WatchCallbackSlot>,
}

/// Convert a C string to `&str`; `None` if null or invalid UTF-8.
pub(crate) unsafe fn cstr_to_str<'a>(s: *const c_char) -> Option<&'a str> {
    if s.is_null() {
        None
    } else {
        unsafe { CStr::from_ptr(s).to_str().ok() }
    }
}

/// Optional C string param: `None` if null, empty, or invalid UTF-8.
unsafe fn optional_cstr<'a>(s: *const c_char) -> Option<&'a str> {
    unsafe { cstr_to_str(s) }.filter(|s| !s.is_empty())
}

// An optional string of the service functions: `None` if null or empty, an error if not UTF-8.
unsafe fn optional_arg<'a>(
    s: *const c_char,
    name: &str,
) -> Result<Option<&'a str>, *mut FffResult> {
    if s.is_null() {
        return Ok(None);
    }
    match unsafe { CStr::from_ptr(s) }.to_str() {
        Ok("") => Ok(None),
        Ok(s) => Ok(Some(s)),
        Err(_) => Err(FffResult::err(&format!("{name} is not valid UTF-8"))),
    }
}

// The newline-separated `extra_ignore` rules of the options; none when null or empty.
unsafe fn extra_ignore_rules(opts: &FffCreateOptions) -> Result<Vec<String>, *mut FffResult> {
    let rules = unsafe { optional_arg(opts.extra_ignore, "extra_ignore") }?;
    Ok(rules.map_or_else(Vec::new, |rules| rules.lines().map(str::to_owned).collect()))
}

/// Recover a `&FffInstance` from the opaque pointer; error `FffResult` if null.
pub(crate) unsafe fn instance_ref<'a>(
    fff_handle: *mut c_void,
) -> Result<&'a FffInstance, *mut FffResult> {
    if fff_handle.is_null() {
        Err(FffResult::err(
            "Instance handle is null. Create one with fff_create_instance first.",
        ))
    } else {
        Ok(unsafe { &*(fff_handle as *const FffInstance) })
    }
}

// The in-process instance behind the handle; `call` names the function a
// service-backed instance cannot serve.
pub(crate) unsafe fn local_instance<'a>(
    fff_handle: *mut c_void,
    call: &str,
) -> Result<&'a LocalInstance, *mut FffResult> {
    match unsafe { instance_ref(fff_handle) }? {
        FffInstance::Local(local) => Ok(local),
        FffInstance::Service(_) => Err(service::unavailable(call)),
    }
}

/// Decode a `u8` grep mode into the core enum.
fn grep_mode_from_u8(mode: u8) -> fff::GrepMode {
    match mode {
        1 => fff::GrepMode::Regex,
        2 => fff::GrepMode::Fuzzy,
        _ => fff::GrepMode::PlainText,
    }
}

fn service_definition_mode(mode: fff::DefinitionMode) -> fff_service::program::DefinitionMode {
    use fff_service::program::DefinitionMode;
    match mode {
        fff::DefinitionMode::Off => DefinitionMode::Off,
        fff::DefinitionMode::Classify => DefinitionMode::Classify,
        fff::DefinitionMode::DefinitionOnly => DefinitionMode::DefinitionOnly,
        fff::DefinitionMode::UsageOnly => DefinitionMode::UsageOnly,
    }
}

/// Apply "0 means default" convention.
fn default_u32(val: u32, default: u32) -> u32 {
    if val == 0 { default } else { val }
}

fn default_u64(val: u64, default: u64) -> u64 {
    if val == 0 { default } else { val }
}

fn default_i32(val: i32, default: i32) -> i32 {
    if val == 0 { default } else { val }
}

/// Create a new file finder instance (legacy 8-arg positional signature).
///
/// @deprecated Use [`fff_create_instance_with`]. The `use_unsafe_no_lock` parameter is ignored.
///
/// ## Safety
/// See `fff_create_instance_with`.
#[deprecated(
    since = "0.8.5",
    note = "Use fff_create_instance_with with a pointer to FffCreateOptions instead."
)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_create_instance(
    base_path: *const c_char,
    frecency_db_path: *const c_char,
    history_db_path: *const c_char,
    _use_unsafe_no_lock: bool,
    enable_mmap_cache: bool,
    enable_content_indexing: bool,
    watch: bool,
    ai_mode: bool,
) -> *mut FffResult {
    let mut opts = FffCreateOptions::defaults();
    opts.base_path = base_path;
    opts.frecency_db_path = frecency_db_path;
    opts.history_db_path = history_db_path;
    opts.enable_mmap_cache = enable_mmap_cache;
    opts.enable_content_indexing = enable_content_indexing;
    opts.watch = watch;
    opts.ai_mode = ai_mode;
    unsafe { fff_create_instance_with(&opts as *const FffCreateOptions) }
}

/// Create a new file finder instance (legacy 13-arg positional signature).
///
/// @deprecated Use [`fff_create_instance_with`]. The `use_unsafe_no_lock` parameter is ignored.
///
/// ## Safety
/// See `fff_create_instance_with`.
#[deprecated(
    since = "0.8.5",
    note = "Use fff_create_instance_with with a pointer to FffCreateOptions instead."
)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_create_instance2(
    base_path: *const c_char,
    frecency_db_path: *const c_char,
    history_db_path: *const c_char,
    _use_unsafe_no_lock: bool,
    enable_mmap_cache: bool,
    enable_content_indexing: bool,
    watch: bool,
    ai_mode: bool,
    log_file_path: *const c_char,
    log_level: *const c_char,
    cache_budget_max_files: u64,
    cache_budget_max_bytes: u64,
    cache_budget_max_file_size: u64,
) -> *mut FffResult {
    let mut opts = FffCreateOptions::defaults();
    opts.base_path = base_path;
    opts.frecency_db_path = frecency_db_path;
    opts.history_db_path = history_db_path;
    opts.enable_mmap_cache = enable_mmap_cache;
    opts.enable_content_indexing = enable_content_indexing;
    opts.watch = watch;
    opts.ai_mode = ai_mode;
    opts.log_file_path = log_file_path;
    opts.log_level = log_level;
    opts.cache_budget_max_files = cache_budget_max_files;
    opts.cache_budget_max_bytes = cache_budget_max_bytes;
    opts.cache_budget_max_file_size = cache_budget_max_file_size;
    unsafe { fff_create_instance_with(&opts as *const FffCreateOptions) }
}

/// Create a file finder from versioned options. `base_path` is required.
///
/// # Safety
/// Pass a pointer to initialized fields through the version specified in `opts.version`.
/// String pointers must be NUL-terminated UTF-8 or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_create_instance_with(opts: *const FffCreateOptions) -> *mut FffResult {
    if opts.is_null() {
        return FffResult::err("opts is null");
    }
    let version = unsafe { (*opts).version };
    if version == 0 || version > FFF_CREATE_OPTIONS_VERSION {
        return FffResult::err(&format!(
            "Unsupported FffCreateOptions version {} (library understands up to {})",
            version, FFF_CREATE_OPTIONS_VERSION
        ));
    }
    let opts = unsafe { FffCreateOptions::read_versioned(opts, version) };

    let base_path_str = match unsafe { cstr_to_str(opts.base_path) } {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return FffResult::err("opts.base_path is null or empty"),
    };

    let extra_ignore = match unsafe { extra_ignore_rules(&opts) } {
        Ok(rules) => rules,
        Err(error) => return error,
    };
    let ignore_options = FilePickerIgnoreOptions {
        extra_ignore: extra_ignore.into(),
        disable_git: opts.disable_git,
        disable_ignore_files: opts.disable_ignore_files,
    };

    if let Some(log_path) = unsafe { optional_cstr(opts.log_file_path) } {
        let level = unsafe { optional_cstr(opts.log_level) };
        if let Err(e) = fff::log::init_tracing(log_path, level, None) {
            return FffResult::err(&format!("Failed to init tracing: {}", e));
        }
    }

    let frecency_path = unsafe { optional_cstr(opts.frecency_db_path) }.map(|s| s.to_string());
    let history_path = unsafe { optional_cstr(opts.history_db_path) }.map(|s| s.to_string());

    let shared_picker = SharedFilePicker::default();
    let shared_frecency = SharedFrecency::default();
    let query_tracker = SharedQueryTracker::default();

    if let Some(ref frecency_path) = frecency_path {
        if let Some(parent) = PathBuf::from(frecency_path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        match FrecencyTracker::open(frecency_path) {
            Ok(tracker) => {
                if let Err(e) = shared_frecency.init(tracker) {
                    return FffResult::err(&format!("Failed to acquire frecency lock: {}", e));
                }
            }
            Err(e) => return FffResult::err(&format!("Failed to init frecency db: {}", e)),
        }
    }

    if let Some(ref history_path) = history_path {
        if let Some(parent) = PathBuf::from(history_path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        match QueryTracker::open(history_path) {
            Ok(tracker) => {
                if let Err(e) = query_tracker.init(tracker) {
                    return FffResult::err(&format!("Failed to acquire query tracker lock: {}", e));
                }
            }
            Err(e) => return FffResult::err(&format!("Failed to init query tracker db: {}", e)),
        }
    }

    let mode = if opts.ai_mode {
        FFFMode::Ai
    } else {
        FFFMode::Neovim
    };

    let cache_budget = fff::ContentCacheBudget::from_overrides(
        opts.cache_budget_max_files as usize,
        opts.cache_budget_max_bytes,
        opts.cache_budget_max_file_size,
    );

    if let Err(e) = FilePicker::new_with_shared_state_and_ignore_options(
        shared_picker.clone(),
        shared_frecency.clone(),
        fff::FilePickerOptions {
            base_path: base_path_str,
            enable_mmap_cache: opts.enable_mmap_cache,
            enable_content_indexing: opts.enable_content_indexing,
            watch: opts.watch,
            mode,
            cache_budget,
            follow_symlinks: opts.follow_symlinks,
            enable_fs_root_scanning: opts.enable_fs_root_scanning,
            enable_home_dir_scanning: opts.enable_home_dir_scanning,
            git_recency: Default::default(),
            rescan_cooldown: None,
        },
        ignore_options,
    ) {
        return FffResult::err(&format!("Failed to init file picker: {}", e));
    }

    let instance = Box::new(FffInstance::Local(LocalInstance {
        picker: shared_picker,
        frecency: shared_frecency,
        query_tracker,
        watch_callback: std::sync::Arc::new(watch::WatchCallbackSlot::default()),
    }));

    let fff_handle = Box::into_raw(instance) as *mut c_void;
    FffResult::ok_handle(fff_handle)
}

// The exported by-value symbol keeps its original ABI.
#[repr(C)]
struct LegacyCreateOptions {
    version: u32,
    base_path: *const c_char,
    frecency_db_path: *const c_char,
    history_db_path: *const c_char,
    enable_mmap_cache: bool,
    enable_content_indexing: bool,
    watch: bool,
    ai_mode: bool,
    log_file_path: *const c_char,
    log_level: *const c_char,
    cache_budget_max_files: u64,
    cache_budget_max_bytes: u64,
    cache_budget_max_file_size: u64,
    enable_fs_root_scanning: bool,
    enable_home_dir_scanning: bool,
    follow_symlinks: std::mem::MaybeUninit<bool>,
}

#[unsafe(no_mangle)]
unsafe extern "C" fn fff_create_instance_with_value(opts: LegacyCreateOptions) -> *mut FffResult {
    if opts.version > 2 {
        return FffResult::err("The by-value symbol accepts FffCreateOptions versions 1 and 2");
    }
    unsafe { fff_create_instance_with((&raw const opts).cast()) }
}

/// Create an instance whose index lives in the shared fff service of this
/// library's build, starting that service from `executable` when none runs.
///
/// The instance takes the same handle as in-process instances and works with
/// every search, grep, scan, watch and health function. Functions without a
/// service equivalent (query tracking and history, git status refresh,
/// restarting in another directory) return an error naming them as
/// unavailable over the shared service.
///
/// From `opts`: `base_path`, `frecency_db_path`, `enable_mmap_cache` (warmup),
/// `enable_content_indexing`, `watch`, `ai_mode` (grep constraint syntax),
/// `cache_budget_max_files`, `enable_fs_root_scanning`,
/// `enable_home_dir_scanning`, `follow_symlinks`, `extra_ignore`, `disable_git`
/// and `disable_ignore_files` choose the index the session uses; `log_file_path`
/// and `log_level` configure a service this call starts. `history_db_path` is
/// refused; the other cache budgets are ignored. A session with `extra_ignore`
/// rules shares only an index rooted at its own directory, as the rules are
/// relative to it.
///
/// `executable` is the `fff-mcp` executable built with this library.
/// `service_dir` selects the service namespace; NULL/empty takes
/// `FFF_MCP_SERVICE_DIR` or the default namespace. A service this call starts
/// exits after `FFF_MCP_IDLE_TIMEOUT_SECS` without clients, one hour by default.
///
/// `timeout_ms` bounds the wait for a service that does not answer: opening
/// the session, which may start the service, and each call beyond the time the
/// call itself asks for (a readiness wait's timeout, a grep's time budget). 0
/// takes the defaults, 90 seconds to open and 30 seconds per call. A call that
/// times out or loses the service fails, and the instance's next call opens a
/// new session for the same directory within that call's own bound.
///
/// Calls on one service instance may come from any thread; they are
/// serialized. `fff_destroy` must not overlap another call on the instance.
///
/// On such an instance, the `file_offset` of the grep functions continues only
/// its last grep page; the cursors of [`fff_service_call`] name the index they
/// belong to. A string argument that is not valid UTF-8 fails the call.
///
/// ## Safety
/// * `opts` as in [`fff_create_instance_with`].
/// * `executable` and `service_dir` must be null-terminated strings or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_create_service_instance(
    opts: *const FffCreateOptions,
    executable: *const c_char,
    service_dir: *const c_char,
    timeout_ms: u64,
) -> *mut FffResult {
    if opts.is_null() {
        return FffResult::err("opts is null");
    }
    let version = unsafe { (*opts).version };
    if version == 0 || version > FFF_CREATE_OPTIONS_VERSION {
        return FffResult::err(&format!(
            "Unsupported FffCreateOptions version {} (library understands up to {})",
            version, FFF_CREATE_OPTIONS_VERSION
        ));
    }
    let opts = unsafe { FffCreateOptions::read_versioned(opts, version) };
    match unsafe { service_settings(&opts, executable, service_dir, timeout_ms) } {
        Ok(settings) => match service::ServiceInstance::open(settings) {
            Ok(instance) => {
                let instance = Box::new(FffInstance::Service(Box::new(instance)));
                FffResult::ok_handle(Box::into_raw(instance) as *mut c_void)
            }
            Err(error) => service::failure(&error, "Failed to open a shared service session: "),
        },
        Err(error) => error,
    }
}

unsafe fn service_settings(
    opts: &FffCreateOptions,
    executable: *const c_char,
    service_dir: *const c_char,
    timeout_ms: u64,
) -> Result<service::ServiceSettings, *mut FffResult> {
    let arg = |s, name| unsafe { optional_arg(s, name) };
    let Some(base_path) = arg(opts.base_path, "opts.base_path")? else {
        return Err(FffResult::err("opts.base_path is null or empty"));
    };
    let scope_error = |message: String| FffResult::err_with(&message, FFF_ERROR_SCOPE_UNAVAILABLE);
    if !Path::new(base_path).exists() {
        return Err(scope_error(format!("Path does not exist: {base_path}")));
    }
    let base_path = fff::path_utils::canonicalize(base_path)
        .map_err(|e| scope_error(format!("Failed to canonicalize path: {e}")))?;
    if arg(opts.history_db_path, "opts.history_db_path")?.is_some() {
        return Err(service::unavailable("Query history (history_db_path)"));
    }
    let Some(executable) = arg(executable, "executable")? else {
        return Err(FffResult::err("executable is null or empty"));
    };
    let options = IndexOptions {
        warmup: opts.enable_mmap_cache,
        content_indexing: opts.enable_content_indexing,
        watch: opts.watch,
        follow_symlinks: opts.follow_symlinks,
        max_cached_files: (opts.cache_budget_max_files > 0)
            .then_some(opts.cache_budget_max_files as usize),
        frecency_db: arg(opts.frecency_db_path, "opts.frecency_db_path")?.map(str::to_owned),
        enable_home_scan: opts.enable_home_dir_scanning,
        enable_root_scan: opts.enable_fs_root_scanning,
        extra_ignore: unsafe { extra_ignore_rules(opts) }?,
        disable_git: opts.disable_git,
        disable_ignore_files: opts.disable_ignore_files,
    };
    Ok(service::ServiceSettings {
        base_path,
        options,
        ai_syntax: opts.ai_mode,
        executable: PathBuf::from(executable),
        service_dir: arg(service_dir, "service_dir")?.map(PathBuf::from),
        log_file: arg(opts.log_file_path, "opts.log_file_path")?.map(PathBuf::from),
        log_level: arg(opts.log_level, "opts.log_level")?.map(str::to_owned),
        timeout: (timeout_ms > 0).then(|| std::time::Duration::from_millis(timeout_ms)),
    })
}

/// Send one request of the shared service's program protocol to a service
/// instance and return the whole reply as one JSON document, a C string in
/// `handle` to free with `fff_free_string`.
///
/// `request_json` is the JSON form of `fff_service::program::ProgramRequest` of
/// this library's build, for example
/// `{"SearchFiles":{"query":"main","current_file":null,"offset":0,"limit":100,"exclude_frecency":false}}`.
/// Replies to searches, globs and greps carry every item with the fields of the
/// result structs in camelCase; a grep page's `nextCursor` is passed back
/// unchanged as the next request's `cursor`. A scan progress reply has the
/// fields of `FffScanProgress` in camelCase; readiness and scanning replies are
/// booleans, a repository reply is a path or null and other replies are null.
/// Grep constraints use the syntax of the instance's `ai_mode`. Close the
/// instance with `fff_destroy`, not with a request.
///
/// A failure carries its kind in `int_value`, one of the `FFF_ERROR_*`
/// constants: [`FFF_ERROR_LOST_SERVICE`] when the instance lost its service,
/// whose next call opens a new session, [`FFF_ERROR_STALE_CURSOR`] for a grep
/// cursor of an index the session no longer uses, and so on.
///
/// ## Safety
/// * `fff_handle` must be a valid instance pointer from `fff_create_service_instance`.
/// * `request_json` must be a null-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_service_call(
    fff_handle: *mut c_void,
    request_json: *const c_char,
) -> *mut FffResult {
    let service = match unsafe { instance_ref(fff_handle) } {
        Ok(FffInstance::Service(service)) => service,
        Ok(FffInstance::Local(_)) => {
            return FffResult::err_with(
                "fff_service_call needs an instance of the shared service",
                FFF_ERROR_INVALID_REQUEST,
            );
        }
        Err(e) => return e,
    };
    match unsafe { optional_arg(request_json, "request_json") } {
        Ok(Some(request)) => service.call_json(request),
        Ok(None) => FffResult::err("request_json is null or empty"),
        Err(error) => error,
    }
}

/// Destroy a file finder instance and free all its resources.
///
/// ## Safety
/// `fff_handle` must be a valid pointer returned by `fff_create_instance`, or null (no-op).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_destroy(fff_handle: *mut c_void) {
    if fff_handle.is_null() {
        return;
    }

    let instance = match *unsafe { Box::from_raw(fff_handle as *mut FffInstance) } {
        FffInstance::Local(local) => local,
        FffInstance::Service(service) => return (*service).close(),
    };

    // The C callback and user_data may be freed as soon as this returns.
    instance.picker.shutdown_watches_and_wait();
    instance.watch_callback.clear();

    if let Ok(mut guard) = instance.picker.write()
        && let Some(picker) = guard.take()
    {
        drop(picker);
    }

    if let Ok(mut guard) = instance.frecency.write() {
        *guard = None;
    }
    if let Ok(mut guard) = instance.query_tracker.write() {
        *guard = None;
    }
}

/// Perform fuzzy search on indexed files.
///
/// `current_file` deprioritizes the currently open file (NULL/empty to skip).
/// Zero picks the default: `max_threads` auto, `page_size` 100,
/// `combo_boost_multiplier` 100, `min_combo_count` 3.
///
/// ## Safety
/// * `fff_handle` must be a valid instance pointer from `fff_create_instance`.
/// * `query` and `current_file` must be valid null-terminated UTF-8 strings or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_search(
    fff_handle: *mut c_void,
    query: *const c_char,
    current_file: *const c_char,
    max_threads: u32,
    page_index: u32,
    page_size: u32,
    combo_boost_multiplier: i32,
    min_combo_count: u32,
) -> *mut FffResult {
    unsafe {
        fff_search_ex(
            fff_handle,
            query,
            current_file,
            max_threads,
            page_index,
            page_size,
            combo_boost_multiplier,
            min_combo_count,
            false,
        )
    }
}

/// [`fff_search`] plus `exclude_frecency`: rank as an index without a frecency
/// database would; a query without fuzzy text then lists files in path order.
///
/// ## Safety
/// Same as [`fff_search`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_search_ex(
    fff_handle: *mut c_void,
    query: *const c_char,
    current_file: *const c_char,
    max_threads: u32,
    page_index: u32,
    page_size: u32,
    combo_boost_multiplier: i32,
    min_combo_count: u32,
    exclude_frecency: bool,
) -> *mut FffResult {
    let inst = match unsafe { instance_ref(fff_handle) } {
        Ok(i) => i,
        Err(e) => return e,
    };

    let query_str = match unsafe { cstr_to_str(query) } {
        Some(s) => s,
        None => return FffResult::err("Query is null or invalid UTF-8"),
    };

    let current_file_str = unsafe { optional_cstr(current_file) };
    let page_size = default_u32(page_size, 100) as usize;
    let min_combo_count = default_u32(min_combo_count, 3);
    let combo_boost_multiplier = default_i32(combo_boost_multiplier, 100);
    let inst = match inst {
        FffInstance::Local(local) => local,
        FffInstance::Service(service) => {
            return service.search(search_request(
                query_str,
                current_file_str,
                page_index,
                page_size,
                exclude_frecency,
            ));
        }
    };

    let picker_guard = match inst.picker.read() {
        Ok(g) => g,
        Err(e) => return FffResult::err(&format!("Failed to acquire file picker lock: {}", e)),
    };

    let picker = match picker_guard.as_ref() {
        Some(p) => p,
        None => {
            return FffResult::err("File picker not initialized. Call fff_create_instance first.");
        }
    };

    // Get query tracker ref for combo matching
    let qt_guard = match inst.query_tracker.read() {
        Ok(q) => q,
        Err(_) => return FffResult::err("Failed to acquire query tracker lock"),
    };
    let query_tracker_ref = qt_guard.as_ref();

    let parser = QueryParser::default();
    let parsed = parser.parse(query_str);

    let results = picker.fuzzy_search(
        &parsed,
        query_tracker_ref,
        FuzzySearchOptions {
            max_threads: max_threads as usize,
            current_file: current_file_str,
            project_path: Some(picker.shared_base_path()),
            combo_boost_score_multiplier: combo_boost_multiplier,
            min_combo_count,
            pagination: PaginationArgs {
                offset: page_index as usize,
                limit: page_size,
            },
            scope: Default::default(),
            exclude_frecency,
        },
    );

    let search_result = FffSearchResult::from_core(&results, picker);
    FffResult::ok_handle(search_result as *mut c_void)
}

/// Glob-only search: filter indexed files by a single glob pattern (passed
/// through verbatim, no query parsing), rank by frecency, and paginate.
///
/// `current_file` deprioritizes the currently open file (NULL/empty to skip).
/// Zero picks the default: `max_threads` auto, `page_size` 100.
///
/// ## Safety
/// * `fff_handle` must be a valid instance pointer from `fff_create_instance`.
/// * `pattern` and `current_file` must be valid null-terminated UTF-8 strings or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_glob(
    fff_handle: *mut c_void,
    pattern: *const c_char,
    current_file: *const c_char,
    max_threads: u32,
    page_index: u32,
    page_size: u32,
) -> *mut FffResult {
    unsafe {
        fff_glob_ex(
            fff_handle,
            pattern,
            current_file,
            max_threads,
            page_index,
            page_size,
            false,
        )
    }
}

/// [`fff_glob`] plus `exclude_frecency`, which lists matches in path order.
///
/// ## Safety
/// Same as [`fff_glob`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_glob_ex(
    fff_handle: *mut c_void,
    pattern: *const c_char,
    current_file: *const c_char,
    max_threads: u32,
    page_index: u32,
    page_size: u32,
    exclude_frecency: bool,
) -> *mut FffResult {
    let inst = match unsafe { instance_ref(fff_handle) } {
        Ok(i) => i,
        Err(e) => return e,
    };

    let pattern_str = match unsafe { cstr_to_str(pattern) } {
        Some(s) if !s.is_empty() => s,
        _ => return FffResult::err("Pattern is null, empty, or invalid UTF-8"),
    };

    let current_file_str = unsafe { optional_cstr(current_file) };
    let page_size = default_u32(page_size, 100) as usize;
    let inst = match inst {
        FffInstance::Local(local) => local,
        FffInstance::Service(service) => {
            return service.glob(GlobRequest {
                pattern: pattern_str.to_owned(),
                current_file: current_file_str.map(str::to_owned),
                offset: page_index as usize,
                limit: page_size,
                exclude_frecency,
            });
        }
    };

    let picker_guard = match inst.picker.read() {
        Ok(g) => g,
        Err(e) => return FffResult::err(&format!("Failed to acquire file picker lock: {}", e)),
    };

    let picker = match picker_guard.as_ref() {
        Some(p) => p,
        None => {
            return FffResult::err("File picker not initialized. Call fff_create_instance first.");
        }
    };

    let results = picker.glob(
        pattern_str,
        FuzzySearchOptions {
            max_threads: max_threads as usize,
            current_file: current_file_str,
            project_path: Some(picker.shared_base_path()),
            combo_boost_score_multiplier: 0,
            min_combo_count: 0,
            pagination: PaginationArgs {
                offset: page_index as usize,
                limit: page_size,
            },
            scope: Default::default(),
            exclude_frecency,
        },
    );

    let search_result = FffSearchResult::from_core(&results, picker);
    FffResult::ok_handle(search_result as *mut c_void)
}

/// Perform fuzzy search on indexed directories.
///
/// `current_file` is used for distance scoring (NULL/empty to skip).
/// Zero picks the default: `max_threads` auto, `page_size` 100.
///
/// ## Safety
/// * `fff_handle` must be a valid instance pointer from `fff_create_instance`.
/// * `query` and `current_file` must be valid null-terminated UTF-8 strings or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_search_directories(
    fff_handle: *mut c_void,
    query: *const c_char,
    current_file: *const c_char,
    max_threads: u32,
    page_index: u32,
    page_size: u32,
) -> *mut FffResult {
    unsafe {
        fff_search_directories_ex(
            fff_handle,
            query,
            current_file,
            max_threads,
            page_index,
            page_size,
            false,
        )
    }
}

/// [`fff_search_directories`] plus `exclude_frecency`, as in [`fff_search_ex`].
///
/// ## Safety
/// Same as [`fff_search_directories`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_search_directories_ex(
    fff_handle: *mut c_void,
    query: *const c_char,
    current_file: *const c_char,
    max_threads: u32,
    page_index: u32,
    page_size: u32,
    exclude_frecency: bool,
) -> *mut FffResult {
    let inst = match unsafe { instance_ref(fff_handle) } {
        Ok(i) => i,
        Err(e) => return e,
    };

    let query_str = match unsafe { cstr_to_str(query) } {
        Some(s) => s,
        None => return FffResult::err("Query is null or invalid UTF-8"),
    };

    let current_file_str = unsafe { optional_cstr(current_file) };
    let page_size = default_u32(page_size, 100) as usize;
    let inst = match inst {
        FffInstance::Local(local) => local,
        FffInstance::Service(service) => {
            return service.search_directories(search_request(
                query_str,
                current_file_str,
                page_index,
                page_size,
                exclude_frecency,
            ));
        }
    };

    let picker_guard = match inst.picker.read() {
        Ok(g) => g,
        Err(e) => return FffResult::err(&format!("Failed to acquire file picker lock: {}", e)),
    };

    let picker = match picker_guard.as_ref() {
        Some(p) => p,
        None => {
            return FffResult::err("File picker not initialized. Call fff_create_instance first.");
        }
    };

    let parser = QueryParser::new(fff_query_parser::DirSearchConfig);
    let parsed = parser.parse(query_str);

    let results = picker.fuzzy_search_directories(
        &parsed,
        FuzzySearchOptions {
            max_threads: max_threads as usize,
            current_file: current_file_str,
            project_path: Some(picker.shared_base_path()),
            combo_boost_score_multiplier: 0,
            min_combo_count: 0,
            pagination: PaginationArgs {
                offset: page_index as usize,
                limit: page_size,
            },
            scope: Default::default(),
            exclude_frecency,
        },
    );

    let dir_result = FffDirSearchResult::from_core(&results, picker);
    FffResult::ok_handle(dir_result as *mut c_void)
}

/// Perform a mixed fuzzy search across both files and directories.
///
/// Returns one flat list interleaved by descending total score; each item's
/// `item_type` is 0 = file, 1 = directory. Parameters as in [`fff_search`].
///
/// ## Safety
/// * `fff_handle` must be a valid instance pointer from `fff_create_instance`.
/// * `query` and `current_file` must be valid null-terminated UTF-8 strings or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_search_mixed(
    fff_handle: *mut c_void,
    query: *const c_char,
    current_file: *const c_char,
    max_threads: u32,
    page_index: u32,
    page_size: u32,
    combo_boost_multiplier: i32,
    min_combo_count: u32,
) -> *mut FffResult {
    unsafe {
        fff_search_mixed_ex(
            fff_handle,
            query,
            current_file,
            max_threads,
            page_index,
            page_size,
            combo_boost_multiplier,
            min_combo_count,
            false,
        )
    }
}

/// [`fff_search_mixed`] plus `exclude_frecency`, as in [`fff_search_ex`].
///
/// ## Safety
/// Same as [`fff_search_mixed`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_search_mixed_ex(
    fff_handle: *mut c_void,
    query: *const c_char,
    current_file: *const c_char,
    max_threads: u32,
    page_index: u32,
    page_size: u32,
    combo_boost_multiplier: i32,
    min_combo_count: u32,
    exclude_frecency: bool,
) -> *mut FffResult {
    let inst = match unsafe { instance_ref(fff_handle) } {
        Ok(i) => i,
        Err(e) => return e,
    };

    let query_str = match unsafe { cstr_to_str(query) } {
        Some(s) => s,
        None => return FffResult::err("Query is null or invalid UTF-8"),
    };

    let current_file_str = unsafe { optional_cstr(current_file) };
    let page_size = default_u32(page_size, 100) as usize;
    let min_combo_count = default_u32(min_combo_count, 3);
    let combo_boost_multiplier = default_i32(combo_boost_multiplier, 100);
    let inst = match inst {
        FffInstance::Local(local) => local,
        FffInstance::Service(service) => {
            return service.search_mixed(search_request(
                query_str,
                current_file_str,
                page_index,
                page_size,
                exclude_frecency,
            ));
        }
    };

    let picker_guard = match inst.picker.read() {
        Ok(g) => g,
        Err(e) => return FffResult::err(&format!("Failed to acquire file picker lock: {}", e)),
    };

    let picker = match picker_guard.as_ref() {
        Some(p) => p,
        None => {
            return FffResult::err("File picker not initialized. Call fff_create_instance first.");
        }
    };

    let qt_guard = match inst.query_tracker.read() {
        Ok(q) => q,
        Err(_) => return FffResult::err("Failed to acquire query tracker lock"),
    };
    let query_tracker_ref = qt_guard.as_ref();

    let parser = QueryParser::new(fff_query_parser::MixedSearchConfig);
    let parsed = parser.parse(query_str);

    let results = picker.fuzzy_search_mixed(
        &parsed,
        query_tracker_ref,
        FuzzySearchOptions {
            max_threads: max_threads as usize,
            current_file: current_file_str,
            project_path: Some(picker.shared_base_path()),
            combo_boost_score_multiplier: combo_boost_multiplier,
            min_combo_count,
            pagination: PaginationArgs {
                offset: page_index as usize,
                limit: page_size,
            },
            scope: Default::default(),
            exclude_frecency,
        },
    );

    let mixed_result = FffMixedSearchResult::from_core(&results, picker);
    FffResult::ok_handle(mixed_result as *mut c_void)
}

/// Perform content search (grep) across indexed files.
///
/// `query` supports constraint syntax like `*.rs pattern`; `mode` is
/// 0 = plain text (SIMD), 1 = regex, 2 = fuzzy. Zero picks the default:
/// `max_file_size` 10 MB, `page_limit` 50, `max_matches_per_file` and
/// `time_budget_ms` unlimited. `smart_case` is case-insensitive for
/// all-lowercase queries. `classify_definitions`: 0 = off, 1 = classify,
/// 2 = definition_only, 3 = usage_only.
///
/// ## Safety
/// * `fff_handle` must be a valid instance pointer from `fff_create_instance`.
/// * `query` must be a valid null-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_live_grep(
    fff_handle: *mut c_void,
    query: *const c_char,
    mode: u8,
    max_file_size: u64,
    max_matches_per_file: u32,
    smart_case: bool,
    file_offset: u32,
    page_limit: u32,
    time_budget_ms: u64,
    before_context: u32,
    after_context: u32,
    classify_definitions: u8,
) -> *mut FffResult {
    unsafe {
        fff_live_grep_ex(
            fff_handle,
            query,
            mode,
            max_file_size,
            max_matches_per_file,
            smart_case,
            file_offset,
            page_limit,
            time_budget_ms,
            false,
            before_context,
            after_context,
            classify_definitions,
        )
    }
}

/// [`fff_live_grep`] plus `enforce_time_budget`: when true the budget also bounds
/// zero-match searches and `next_file_offset` resumes at the first unsearched file.
///
/// ## Safety
/// Same as [`fff_live_grep`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_live_grep_ex(
    fff_handle: *mut c_void,
    query: *const c_char,
    mode: u8,
    max_file_size: u64,
    max_matches_per_file: u32,
    smart_case: bool,
    file_offset: u32,
    page_limit: u32,
    time_budget_ms: u64,
    enforce_time_budget: bool,
    before_context: u32,
    after_context: u32,
    classify_definitions: u8,
) -> *mut FffResult {
    unsafe {
        fff_live_grep_ex2(
            fff_handle,
            query,
            mode,
            max_file_size,
            max_matches_per_file,
            smart_case,
            file_offset,
            page_limit,
            time_budget_ms,
            enforce_time_budget,
            before_context,
            after_context,
            classify_definitions,
            false,
        )
    }
}

/// [`fff_live_grep_ex`] plus `exclude_frecency`: files are searched in the
/// order an index without a frecency database gives, newest first.
///
/// ## Safety
/// Same as [`fff_live_grep`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_live_grep_ex2(
    fff_handle: *mut c_void,
    query: *const c_char,
    mode: u8,
    max_file_size: u64,
    max_matches_per_file: u32,
    smart_case: bool,
    file_offset: u32,
    page_limit: u32,
    time_budget_ms: u64,
    enforce_time_budget: bool,
    before_context: u32,
    after_context: u32,
    classify_definitions: u8,
    exclude_frecency: bool,
) -> *mut FffResult {
    let Some(classify_definitions) = fff::DefinitionMode::from_u8(classify_definitions) else {
        return FffResult::err(
            "classify_definitions must be 0 (off), 1 (classify), 2 (definition_only), or 3 (usage_only)",
        );
    };
    let inst = match unsafe { instance_ref(fff_handle) } {
        Ok(i) => i,
        Err(e) => return e,
    };

    let query_str = match unsafe { cstr_to_str(query) } {
        Some(s) => s,
        None => return FffResult::err("Query is null or invalid UTF-8"),
    };
    let inst = match inst {
        FffInstance::Local(local) => local,
        FffInstance::Service(service) => {
            return service.grep(GrepRequest {
                query: query_str.to_owned(),
                mode: match grep_mode_from_u8(mode) {
                    fff::GrepMode::PlainText => fff_service::program::GrepMode::PlainText,
                    fff::GrepMode::Regex => fff_service::program::GrepMode::Regex,
                    fff::GrepMode::Fuzzy => fff_service::program::GrepMode::Fuzzy,
                },
                ai_syntax: service.ai_syntax(),
                options: GrepOptions {
                    max_file_size: default_u64(max_file_size, 10 * 1024 * 1024),
                    max_matches_per_file: max_matches_per_file as usize,
                    smart_case,
                    cursor: match service.cursor(file_offset) {
                        Ok(cursor) => cursor,
                        Err(error) => return error,
                    },
                    page_size: default_u32(page_limit, 50) as usize,
                    time_budget_ms,
                    enforce_time_budget,
                    before_context: before_context as usize,
                    after_context: after_context as usize,
                    classify_definitions: service_definition_mode(classify_definitions),
                    exclude_frecency,
                },
            });
        }
    };

    let picker_guard = match inst.picker.read() {
        Ok(g) => g,
        Err(e) => return FffResult::err(&format!("Failed to acquire file picker lock: {}", e)),
    };

    let picker = match picker_guard.as_ref() {
        Some(p) => p,
        None => {
            return FffResult::err("File picker not initialized. Call fff_create_instance first.");
        }
    };

    let is_ai = picker.mode().is_ai();
    let parsed = if is_ai {
        fff::QueryParser::new(fff_query_parser::AiGrepConfig).parse(query_str)
    } else {
        fff::grep::parse_grep_query(query_str)
    };

    let options = fff::GrepSearchOptions {
        max_file_size: default_u64(max_file_size, 10 * 1024 * 1024),
        max_matches_per_file: max_matches_per_file as usize,
        smart_case,
        casing: None,
        file_offset: file_offset as usize,
        page_limit: default_u32(page_limit, 50) as usize,
        mode: grep_mode_from_u8(mode),
        time_budget_ms,
        enforce_time_budget,
        before_context: before_context as usize,
        after_context: after_context as usize,
        classify_definitions,
        trim_whitespace: false,
        abort_signal: None,
        scope: Default::default(),
        exclude_frecency,
    };

    let result = picker.grep(&parsed, &options);
    let grep_result = FffGrepResult::from_core(&result, picker);
    FffResult::ok_handle(grep_result as *mut c_void)
}

/// Multi-pattern OR search (SIMD Aho-Corasick): lines matching ANY pattern.
///
/// `patterns_joined` is `\n`-separated (e.g. `"foo\nbar"`); `constraints` is an
/// optional file filter like `"*.rs"` or `"/src/"` (NULL/empty to skip).
/// Remaining parameters as in [`fff_live_grep`].
///
/// ## Safety
/// * `fff_handle` must be a valid instance pointer from `fff_create_instance`.
/// * `patterns_joined` and `constraints` must be valid null-terminated UTF-8 or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_multi_grep(
    fff_handle: *mut c_void,
    patterns_joined: *const c_char,
    constraints: *const c_char,
    max_file_size: u64,
    max_matches_per_file: u32,
    smart_case: bool,
    file_offset: u32,
    page_limit: u32,
    time_budget_ms: u64,
    before_context: u32,
    after_context: u32,
    classify_definitions: u8,
) -> *mut FffResult {
    unsafe {
        fff_multi_grep_ex(
            fff_handle,
            patterns_joined,
            constraints,
            max_file_size,
            max_matches_per_file,
            smart_case,
            file_offset,
            page_limit,
            time_budget_ms,
            false,
            before_context,
            after_context,
            classify_definitions,
        )
    }
}

/// [`fff_multi_grep`] plus `enforce_time_budget`, as in [`fff_live_grep_ex`].
///
/// ## Safety
/// Same as [`fff_multi_grep`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_multi_grep_ex(
    fff_handle: *mut c_void,
    patterns_joined: *const c_char,
    constraints: *const c_char,
    max_file_size: u64,
    max_matches_per_file: u32,
    smart_case: bool,
    file_offset: u32,
    page_limit: u32,
    time_budget_ms: u64,
    enforce_time_budget: bool,
    before_context: u32,
    after_context: u32,
    classify_definitions: u8,
) -> *mut FffResult {
    unsafe {
        fff_multi_grep_ex2(
            fff_handle,
            patterns_joined,
            constraints,
            max_file_size,
            max_matches_per_file,
            smart_case,
            file_offset,
            page_limit,
            time_budget_ms,
            enforce_time_budget,
            before_context,
            after_context,
            classify_definitions,
            false,
        )
    }
}

/// [`fff_multi_grep_ex`] plus `exclude_frecency`, as in [`fff_live_grep_ex2`].
///
/// ## Safety
/// Same as [`fff_multi_grep`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_multi_grep_ex2(
    fff_handle: *mut c_void,
    patterns_joined: *const c_char,
    constraints: *const c_char,
    max_file_size: u64,
    max_matches_per_file: u32,
    smart_case: bool,
    file_offset: u32,
    page_limit: u32,
    time_budget_ms: u64,
    enforce_time_budget: bool,
    before_context: u32,
    after_context: u32,
    classify_definitions: u8,
    exclude_frecency: bool,
) -> *mut FffResult {
    let Some(classify_definitions) = fff::DefinitionMode::from_u8(classify_definitions) else {
        return FffResult::err(
            "classify_definitions must be 0 (off), 1 (classify), 2 (definition_only), or 3 (usage_only)",
        );
    };
    let inst = match unsafe { instance_ref(fff_handle) } {
        Ok(i) => i,
        Err(e) => return e,
    };

    let patterns_str = match unsafe { cstr_to_str(patterns_joined) } {
        Some(s) if !s.is_empty() => s,
        _ => return FffResult::err("patterns_joined is null or empty"),
    };

    let patterns: Vec<&str> = patterns_str.split('\n').collect();
    if patterns.is_empty() || patterns.iter().all(|p| p.is_empty()) {
        return FffResult::err("patterns must not be empty");
    }

    let constraints_str = unsafe { optional_cstr(constraints) };
    let inst = match inst {
        FffInstance::Local(local) => local,
        FffInstance::Service(service) => {
            return service.multi_grep(MultiGrepRequest {
                patterns: patterns.iter().map(|p| (*p).to_owned()).collect(),
                constraints: constraints_str.map(str::to_owned),
                options: GrepOptions {
                    max_file_size: default_u64(max_file_size, 10 * 1024 * 1024),
                    max_matches_per_file: max_matches_per_file as usize,
                    smart_case,
                    cursor: match service.cursor(file_offset) {
                        Ok(cursor) => cursor,
                        Err(error) => return error,
                    },
                    page_size: default_u32(page_limit, 50) as usize,
                    time_budget_ms,
                    enforce_time_budget,
                    before_context: before_context as usize,
                    after_context: after_context as usize,
                    classify_definitions: service_definition_mode(classify_definitions),
                    exclude_frecency,
                },
            });
        }
    };

    let picker_guard = match inst.picker.read() {
        Ok(g) => g,
        Err(e) => return FffResult::err(&format!("Failed to acquire file picker lock: {}", e)),
    };

    let picker = match picker_guard.as_ref() {
        Some(p) => p,
        None => {
            return FffResult::err("File picker not initialized. Call fff_create_instance first.");
        }
    };

    // Parse constraints from the optional string (e.g. "*.rs /src/")
    let parsed_constraints = constraints_str
        .map(|c| fff::QueryParser::new(fff_query_parser::AiGrepConfig).parse_constraints(c));

    let constraint_refs: &[fff::Constraint<'_>] = match &parsed_constraints {
        Some(constraints) => constraints,
        None => &[],
    };

    let options = fff::GrepSearchOptions {
        max_file_size: default_u64(max_file_size, 10 * 1024 * 1024),
        max_matches_per_file: max_matches_per_file as usize,
        smart_case,
        casing: None,
        file_offset: file_offset as usize,
        page_limit: default_u32(page_limit, 50) as usize,
        mode: fff::GrepMode::PlainText, // ignored by multi_grep_search
        time_budget_ms,
        enforce_time_budget,
        before_context: before_context as usize,
        after_context: after_context as usize,
        classify_definitions,
        trim_whitespace: false,
        abort_signal: None,
        scope: Default::default(),
        exclude_frecency,
    };

    let result = picker.multi_grep(&patterns, constraint_refs, &options);
    let grep_result = FffGrepResult::from_core(&result, picker);
    FffResult::ok_handle(grep_result as *mut c_void)
}

/// Trigger a rescan of the file index.
///
/// ## Safety
/// `fff_handle` must be a valid instance pointer from `fff_create_instance`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_scan_files(fff_handle: *mut c_void) -> *mut FffResult {
    let inst = match unsafe { instance_ref(fff_handle) } {
        Ok(FffInstance::Local(local)) => local,
        Ok(FffInstance::Service(service)) => return service.rescan(),
        Err(e) => return e,
    };

    // Async: rescan runs on a BG thread, caller returns immediately.
    // Use `fff_is_scanning` / `fff_wait_for_scan` to observe progress.
    match inst.picker.trigger_full_rescan_async(&inst.frecency) {
        Ok(()) => FffResult::ok_empty(),
        Err(e) => FffResult::err(&format!("Failed to trigger rescan: {}", e)),
    }
}

/// Check if a scan is currently in progress.
///
/// ## Safety
/// `fff_handle` must be a valid instance pointer from `fff_create_instance`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_is_scanning(fff_handle: *mut c_void) -> bool {
    let inst = match unsafe { instance_ref(fff_handle) } {
        Ok(FffInstance::Local(local)) => local,
        Ok(FffInstance::Service(service)) => return service.is_scanning(),
        Err(_) => return false,
    };

    inst.picker
        .read()
        .ok()
        .and_then(|guard| guard.as_ref().map(|p| p.is_scan_active()))
        .unwrap_or(false)
}

/// Get the picker's base path as a heap C string in `handle`;
/// free it with `fff_free_string`.
///
/// ## Safety
/// `fff_handle` must be a valid instance pointer from `fff_create_instance`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_get_base_path(fff_handle: *mut c_void) -> *mut FffResult {
    let inst = match unsafe { instance_ref(fff_handle) } {
        Ok(FffInstance::Local(local)) => local,
        Ok(FffInstance::Service(service)) => {
            return FffResult::ok_string(&service.base_path().to_string_lossy());
        }
        Err(e) => return e,
    };

    let guard = match inst.picker.read() {
        Ok(g) => g,
        Err(e) => return FffResult::err(&format!("Failed to acquire file picker lock: {}", e)),
    };

    let picker = match guard.as_ref() {
        Some(p) => p,
        None => return FffResult::err("File picker not initialized"),
    };

    FffResult::ok_string(&picker.base_path().to_string_lossy())
}

/// Get scan progress information.
///
/// ## Safety
/// `fff_handle` must be a valid instance pointer from `fff_create_instance`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_get_scan_progress(fff_handle: *mut c_void) -> *mut FffResult {
    let inst = match unsafe { instance_ref(fff_handle) } {
        Ok(FffInstance::Local(local)) => local,
        Ok(FffInstance::Service(service)) => return service.scan_progress(),
        Err(e) => return e,
    };

    let guard = match inst.picker.read() {
        Ok(g) => g,
        Err(e) => return FffResult::err(&format!("Failed to acquire file picker lock: {}", e)),
    };

    let picker = match guard.as_ref() {
        Some(p) => p,
        None => return FffResult::err("File picker not initialized"),
    };

    let result = Box::into_raw(Box::new(FffScanProgress::from(picker.get_scan_progress())));
    FffResult::ok_handle(result as *mut c_void)
}

/// Wait for initial scan to complete.
///
/// ## Safety
/// `fff_handle` must be a valid instance pointer from `fff_create_instance`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_wait_for_scan(
    fff_handle: *mut c_void,
    timeout_ms: u64,
) -> *mut FffResult {
    let picker = match unsafe { instance_ref(fff_handle) } {
        Ok(FffInstance::Local(local)) => &local.picker,
        Ok(FffInstance::Service(service)) => {
            return service.wait_ready(Duration::from_millis(timeout_ms));
        }
        Err(e) => return e,
    };

    let completed = picker.wait_for_scan(Duration::from_millis(timeout_ms));
    FffResult::ok_int(completed as i64)
}

/// Wait for the background file watcher to be ready.
///
/// ## Safety
/// `fff_handle` must be a valid instance pointer from `fff_create_instance`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_wait_for_watcher(
    fff_handle: *mut c_void,
    timeout_ms: u64,
) -> *mut FffResult {
    // A service session is ready once its watcher settled.
    let inst = match unsafe { instance_ref(fff_handle) } {
        Ok(FffInstance::Local(local)) => local,
        Ok(FffInstance::Service(service)) => {
            return service.wait_ready(Duration::from_millis(timeout_ms));
        }
        Err(e) => return e,
    };

    let completed = inst
        .picker
        .wait_for_watcher(Duration::from_millis(timeout_ms));
    FffResult::ok_int(completed as i64)
}

/// Restart indexing in a new directory.
///
/// ## Safety
/// * `fff_handle` must be a valid instance pointer from `fff_create_instance`.
/// * `new_path` must be a valid null-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_restart_index(
    fff_handle: *mut c_void,
    new_path: *const c_char,
) -> *mut FffResult {
    let inst = match unsafe { instance_ref(fff_handle) } {
        Ok(i) => i,
        Err(e) => return e,
    };

    let path_str = match unsafe { cstr_to_str(new_path) } {
        Some(s) => s,
        None => return FffResult::err("Path is null or invalid UTF-8"),
    };

    let path = PathBuf::from(&path_str);
    if !path.exists() {
        return FffResult::err(&format!("Path does not exist: {}", path_str));
    }

    let canonical_path = match fff::path_utils::canonicalize(&path) {
        Ok(p) => p,
        Err(e) => return FffResult::err(&format!("Failed to canonicalize path: {}", e)),
    };
    let inst = match inst {
        FffInstance::Local(local) => local,
        FffInstance::Service(service) => return service.restart(&canonical_path),
    };

    let guard = match inst.picker.write() {
        Ok(g) => g,
        Err(e) => return FffResult::err(&format!("Failed to acquire file picker lock: {}", e)),
    };

    let (warmup_caches, content_indexing, watch, mode, fs_root, home_dir, follow_symlinks) =
        if let Some(ref picker) = *guard {
            (
                picker.has_mmap_cache(),
                picker.has_content_indexing(),
                picker.has_watcher(),
                picker.mode(),
                picker.fs_root_scanning_enabled(),
                picker.home_dir_scanning_enabled(),
                picker.follows_symlinks(),
            )
        } else {
            (false, true, true, FFFMode::default(), false, false, false)
        };
    let git_recency = guard
        .as_ref()
        .map(|p| p.git_recency_config())
        .unwrap_or_default();
    let ignore_options = guard
        .as_ref()
        .map(|picker| picker.ignore_options().clone())
        .unwrap_or_default();

    drop(guard);

    match FilePicker::new_with_shared_state_and_ignore_options(
        inst.picker.clone(),
        inst.frecency.clone(),
        fff::FilePickerOptions {
            base_path: canonical_path.to_string_lossy().to_string(),
            enable_mmap_cache: warmup_caches,
            enable_content_indexing: content_indexing,
            watch,
            mode,
            cache_budget: None,
            follow_symlinks,
            enable_fs_root_scanning: fs_root,
            enable_home_dir_scanning: home_dir,
            git_recency,
            rescan_cooldown: None,
        },
        ignore_options,
    ) {
        Ok(()) => FffResult::ok_empty(),
        Err(e) => FffResult::err(&format!("Failed to init file picker: {}", e)),
    }
}

/// Refresh git status cache.
///
/// ## Safety
/// `fff_handle` must be a valid instance pointer from `fff_create_instance`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_refresh_git_status(fff_handle: *mut c_void) -> *mut FffResult {
    let inst = match unsafe { local_instance(fff_handle, "fff_refresh_git_status") } {
        Ok(i) => i,
        Err(e) => return e,
    };

    match inst.picker.refresh_git_status(&inst.frecency) {
        Ok(count) => FffResult::ok_int(count as i64),
        Err(e) => FffResult::err(&format!("Failed to refresh git status: {}", e)),
    }
}

/// Track query completion for smart suggestions.
///
/// ## Safety
/// * `fff_handle` must be a valid instance pointer from `fff_create_instance`.
/// * `query` and `file_path` must be valid null-terminated UTF-8 strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_track_query(
    fff_handle: *mut c_void,
    query: *const c_char,
    file_path: *const c_char,
) -> *mut FffResult {
    let inst = match unsafe { local_instance(fff_handle, "fff_track_query") } {
        Ok(i) => i,
        Err(e) => return e,
    };

    let query_str = match unsafe { cstr_to_str(query) } {
        Some(s) => s,
        None => return FffResult::err("Query is null or invalid UTF-8"),
    };

    let path_str = match unsafe { cstr_to_str(file_path) } {
        Some(s) => s,
        None => return FffResult::err("File path is null or invalid UTF-8"),
    };

    let file_path = match fff::path_utils::canonicalize(path_str) {
        Ok(p) => p,
        Err(e) => return FffResult::err(&format!("Failed to canonicalize path: {}", e)),
    };

    let (project_path, file_path) = {
        let guard = match inst.picker.read() {
            Ok(g) => g,
            Err(_) => return FffResult::ok_int(0),
        };
        match guard.as_ref() {
            Some(p) => (
                p.shared_base_path().to_path_buf(),
                p.shared_path_for(&file_path).unwrap_or(file_path),
            ),
            None => return FffResult::ok_int(0),
        }
    };

    let mut qt_guard = match inst.query_tracker.write() {
        Ok(q) => q,
        Err(_) => return FffResult::ok_int(0),
    };

    if let Some(ref mut tracker) = *qt_guard
        && let Err(e) = tracker.track_query_completion(query_str, &project_path, &file_path)
    {
        return FffResult::err(&format!("Failed to track query: {}", e));
    }

    FffResult::ok_int(1)
}

/// Get historical query by offset (0 = most recent).
///
/// ## Safety
/// `fff_handle` must be a valid instance pointer from `fff_create_instance`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_get_historical_query(
    fff_handle: *mut c_void,
    offset: u64,
) -> *mut FffResult {
    let inst = match unsafe { local_instance(fff_handle, "fff_get_historical_query") } {
        Ok(i) => i,
        Err(e) => return e,
    };

    let project_path = {
        let guard = match inst.picker.read() {
            Ok(g) => g,
            Err(_) => return FffResult::ok_empty(),
        };
        match guard.as_ref() {
            Some(p) => p.shared_base_path().to_path_buf(),
            None => return FffResult::ok_empty(),
        }
    };

    let qt_guard = match inst.query_tracker.read() {
        Ok(q) => q,
        Err(_) => return FffResult::ok_empty(),
    };

    let tracker = match qt_guard.as_ref() {
        Some(t) => t,
        None => return FffResult::ok_empty(),
    };

    match tracker.get_historical_query(&project_path, offset as usize) {
        Ok(Some(query)) => FffResult::ok_string(&query),
        Ok(None) => FffResult::ok_empty(),
        Err(e) => FffResult::err(&format!("Failed to get historical query: {}", e)),
    }
}

/// Get health check information.
///
/// On a service-backed instance the check asks the service for the index's
/// state and repository; when the instance lost its service, the check fails
/// with [`FFF_ERROR_LOST_SERVICE`] as [`fff_service_call`] does.
///
/// ## Safety
/// * `fff_handle` must be a valid instance pointer from `fff_create_instance`, or null for
///   a limited health check (version + git only).
/// * `test_path` can be null or a valid null-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_health_check(
    fff_handle: *mut c_void,
    test_path: *const c_char,
) -> *mut FffResult {
    let test_path = unsafe { optional_cstr(test_path) }.map(PathBuf::from);

    let mut health = serde_json::Map::new();
    health.insert(
        "version".to_string(),
        serde_json::Value::String(env!("CARGO_PKG_VERSION").to_string()),
    );

    let inst: Option<&LocalInstance> = if fff_handle.is_null() {
        None
    } else {
        match unsafe { &*(fff_handle as *const FffInstance) } {
            FffInstance::Local(local) => Some(local),
            FffInstance::Service(service) => {
                return match service.health(test_path.as_deref(), &mut health) {
                    Ok(()) => health_result(&health),
                    Err(failure) => failure,
                };
            }
        }
    };

    let test_path = test_path.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    health.insert("git".to_string(), git_health(discover_workdir(&test_path)));

    // File picker info
    let mut picker_info = serde_json::Map::new();
    if let Some(inst) = inst {
        match inst.picker.read() {
            Ok(guard) => {
                if let Some(ref picker) = *guard {
                    picker_info.insert("initialized".to_string(), serde_json::Value::Bool(true));
                    picker_info.insert(
                        "base_path".to_string(),
                        serde_json::Value::String(picker.base_path().to_string_lossy().to_string()),
                    );
                    picker_info.insert(
                        "is_scanning".to_string(),
                        serde_json::Value::Bool(picker.is_scan_active()),
                    );
                    let progress = picker.get_scan_progress();
                    picker_info.insert(
                        "indexed_files".to_string(),
                        serde_json::Value::Number(progress.scanned_files_count.into()),
                    );
                } else {
                    picker_info.insert("initialized".to_string(), serde_json::Value::Bool(false));
                }
            }
            Err(_) => {
                picker_info.insert("initialized".to_string(), serde_json::Value::Bool(false));
                picker_info.insert(
                    "error".to_string(),
                    serde_json::Value::String("Failed to acquire lock".to_string()),
                );
            }
        }
    } else {
        picker_info.insert("initialized".to_string(), serde_json::Value::Bool(false));
    }
    health.insert(
        "file_picker".to_string(),
        serde_json::Value::Object(picker_info),
    );

    // Frecency info
    let mut frecency_info = serde_json::Map::new();
    if let Some(inst) = inst {
        match inst.frecency.read() {
            Ok(guard) => {
                frecency_info.insert(
                    "initialized".to_string(),
                    serde_json::Value::Bool(guard.is_some()),
                );
                if let Some(ref frecency) = *guard
                    && let Ok(health_data) = frecency.get_health()
                {
                    let mut db_health = serde_json::Map::new();
                    db_health.insert(
                        "path".to_string(),
                        serde_json::Value::String(health_data.path),
                    );
                    db_health.insert(
                        "disk_size".to_string(),
                        serde_json::Value::Number(health_data.disk_size.into()),
                    );
                    frecency_info.insert(
                        "db_healthcheck".to_string(),
                        serde_json::Value::Object(db_health),
                    );
                }
            }
            Err(_) => {
                frecency_info.insert("initialized".to_string(), serde_json::Value::Bool(false));
            }
        }
    } else {
        frecency_info.insert("initialized".to_string(), serde_json::Value::Bool(false));
    }
    health.insert(
        "frecency".to_string(),
        serde_json::Value::Object(frecency_info),
    );

    // Query tracker info
    let mut query_info = serde_json::Map::new();
    if let Some(inst) = inst {
        match inst.query_tracker.read() {
            Ok(guard) => {
                query_info.insert(
                    "initialized".to_string(),
                    serde_json::Value::Bool(guard.is_some()),
                );
                if let Some(ref tracker) = *guard
                    && let Ok(health_data) = tracker.get_health()
                {
                    let mut db_health = serde_json::Map::new();
                    db_health.insert(
                        "path".to_string(),
                        serde_json::Value::String(health_data.path),
                    );
                    db_health.insert(
                        "disk_size".to_string(),
                        serde_json::Value::Number(health_data.disk_size.into()),
                    );
                    query_info.insert(
                        "db_healthcheck".to_string(),
                        serde_json::Value::Object(db_health),
                    );
                }
            }
            Err(_) => {
                query_info.insert("initialized".to_string(), serde_json::Value::Bool(false));
            }
        }
    } else {
        query_info.insert("initialized".to_string(), serde_json::Value::Bool(false));
    }
    health.insert(
        "query_tracker".to_string(),
        serde_json::Value::Object(query_info),
    );

    health_result(&health)
}

/// Free a search result returned by `fff_search`: the struct, its `items`
/// and `scores` arrays, and every string within.
///
/// ## Safety
/// `result` must be a valid pointer previously returned via `FffResult.handle`
/// from `fff_search`, or null (no-op).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_free_search_result(result: *mut FffSearchResult) {
    if result.is_null() {
        return;
    }

    unsafe {
        let result = Box::from_raw(result);
        let count = result.count as usize;

        if !result.items.is_null() {
            let mut items = Vec::from_raw_parts(result.items, count, count);
            for item in &mut items {
                item.free_strings();
            }
        }
        if !result.scores.is_null() {
            let mut scores = Vec::from_raw_parts(result.scores, count, count);
            for score in &mut scores {
                score.free_strings();
            }
        }
    }
}

/// Pointer to the `index`-th `FffFileItem`; null if `result` is null or
/// `index >= count`. Valid until the search result is freed.
///
/// ## Safety
/// `result` must be a valid `FffSearchResult` pointer from `fff_search`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_search_result_get_item(
    result: *const FffSearchResult,
    index: u32,
) -> *const FffFileItem {
    if result.is_null() {
        return std::ptr::null();
    }
    let result = unsafe { &*result };
    if index >= result.count || result.items.is_null() {
        return std::ptr::null();
    }
    unsafe { result.items.add(index as usize) }
}

/// Pointer to the `index`-th `FffScore`; null if `result` is null or
/// `index >= count`. Valid until the search result is freed.
///
/// ## Safety
/// `result` must be a valid `FffSearchResult` pointer from `fff_search`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_search_result_get_score(
    result: *const FffSearchResult,
    index: u32,
) -> *const FffScore {
    if result.is_null() {
        return std::ptr::null();
    }
    let result = unsafe { &*result };
    if index >= result.count || result.scores.is_null() {
        return std::ptr::null();
    }
    unsafe { result.scores.add(index as usize) }
}

/// Free a grep result returned by `fff_live_grep` or `fff_multi_grep`:
/// the struct, its `items` array, and all strings/ranges/context within.
///
/// ## Safety
/// `result` must be a valid pointer previously returned via `FffResult.handle`
/// from `fff_live_grep` or `fff_multi_grep`, or null (no-op).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_free_grep_result(result: *mut FffGrepResult) {
    if result.is_null() {
        return;
    }

    unsafe {
        let result = Box::from_raw(result);
        let count = result.count as usize;

        if !result.items.is_null() {
            let mut items = Vec::from_raw_parts(result.items, count, count);
            for item in &mut items {
                item.free_fields();
            }
        }
        if !result.regex_fallback_error.is_null() {
            drop(CString::from_raw(result.regex_fallback_error));
        }
    }
}

/// Pointer to the `index`-th `FffGrepMatch`; null if `result` is null or
/// `index >= count`. Valid until the grep result is freed.
///
/// ## Safety
/// `result` must be a valid `FffGrepResult` pointer from `fff_live_grep` or `fff_multi_grep`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_grep_result_get_match(
    result: *const FffGrepResult,
    index: u32,
) -> *const FffGrepMatch {
    if result.is_null() {
        return std::ptr::null();
    }
    let result = unsafe { &*result };
    if index >= result.count || result.items.is_null() {
        return std::ptr::null();
    }
    unsafe { result.items.add(index as usize) }
}

/// Free a scan progress result returned by `fff_get_scan_progress`.
///
/// ## Safety
/// `result` must be a valid pointer previously returned via `FffResult.handle`
/// from `fff_get_scan_progress`, or null (no-op).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_free_scan_progress(result: *mut FffScanProgress) {
    if !result.is_null() {
        unsafe { drop(Box::from_raw(result)) };
    }
}

/// Offset a pointer by `byte_offset` bytes (FFI array iteration helper).
/// Returns null if `base` is null.
///
/// ## Safety
/// The resulting pointer must be within the bounds of the original allocation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_ptr_offset(base: *const c_void, byte_offset: usize) -> *const c_void {
    if base.is_null() {
        return std::ptr::null();
    }
    unsafe { (base as *const u8).add(byte_offset) as *const c_void }
}

/// Free a result envelope returned by any `fff_*` function.
/// **IMPORTANT:** the `handle` payload is NOT freed release it separately
/// using handle specific cleaning methods (`fff_destroy`, `fff_free_search_result`, etc.).
///
/// ## Safety
/// `result_ptr` must be a valid pointer returned by a `fff_*` function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_free_result(result_ptr: *mut FffResult) {
    if result_ptr.is_null() {
        return;
    }

    unsafe {
        let result = Box::from_raw(result_ptr);
        if !result.error.is_null() {
            drop(CString::from_raw(result.error));
        }

        // note: handle is not freed by design
    }
}

/// Free a string returned by `fff_*` functions.
///
/// ## Safety
/// `s` must be a valid C string allocated by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_free_string(s: *mut c_char) {
    unsafe {
        if !s.is_null() {
            drop(CString::from_raw(s));
        }
    }
}

// ---------------------------------------------------------------------------
// Directory search: free and accessor functions
// ---------------------------------------------------------------------------

/// Free a directory search result returned by `fff_search_directories`.
///
/// ## Safety
/// `result` must be a valid pointer previously returned via `FffResult.handle`
/// from `fff_search_directories`, or null (no-op).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_free_dir_search_result(result: *mut FffDirSearchResult) {
    if result.is_null() {
        return;
    }

    unsafe {
        let result = Box::from_raw(result);
        let count = result.count as usize;

        if !result.items.is_null() {
            let mut items = Vec::from_raw_parts(result.items, count, count);
            for item in &mut items {
                item.free_strings();
            }
        }
        if !result.scores.is_null() {
            let mut scores = Vec::from_raw_parts(result.scores, count, count);
            for score in &mut scores {
                score.free_strings();
            }
        }
    }
}

/// Get a pointer to the `index`-th `FffDirItem` in a directory search result.
///
/// ## Safety
/// `result` must be a valid `FffDirSearchResult` pointer from `fff_search_directories`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_dir_search_result_get_item(
    result: *const FffDirSearchResult,
    index: u32,
) -> *const FffDirItem {
    if result.is_null() {
        return std::ptr::null();
    }
    let result = unsafe { &*result };
    if index >= result.count || result.items.is_null() {
        return std::ptr::null();
    }
    unsafe { result.items.add(index as usize) }
}

/// Get a pointer to the `index`-th `FffScore` in a directory search result.
///
/// ## Safety
/// `result` must be a valid `FffDirSearchResult` pointer from `fff_search_directories`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_dir_search_result_get_score(
    result: *const FffDirSearchResult,
    index: u32,
) -> *const FffScore {
    if result.is_null() {
        return std::ptr::null();
    }
    let result = unsafe { &*result };
    if index >= result.count || result.scores.is_null() {
        return std::ptr::null();
    }
    unsafe { result.scores.add(index as usize) }
}

// ---------------------------------------------------------------------------
// Mixed search: free and accessor functions
// ---------------------------------------------------------------------------

/// Free a mixed search result returned by `fff_search_mixed`.
///
/// ## Safety
/// `result` must be a valid pointer previously returned via `FffResult.handle`
/// from `fff_search_mixed`, or null (no-op).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_free_mixed_search_result(result: *mut FffMixedSearchResult) {
    if result.is_null() {
        return;
    }

    unsafe {
        let result = Box::from_raw(result);
        let count = result.count as usize;

        if !result.items.is_null() {
            let mut items = Vec::from_raw_parts(result.items, count, count);
            for item in &mut items {
                item.free_strings();
            }
        }
        if !result.scores.is_null() {
            let mut scores = Vec::from_raw_parts(result.scores, count, count);
            for score in &mut scores {
                score.free_strings();
            }
        }
    }
}

/// Get a pointer to the `index`-th `FffMixedItem` in a mixed search result.
///
/// ## Safety
/// `result` must be a valid `FffMixedSearchResult` pointer from `fff_search_mixed`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_mixed_search_result_get_item(
    result: *const FffMixedSearchResult,
    index: u32,
) -> *const FffMixedItem {
    if result.is_null() {
        return std::ptr::null();
    }
    let result = unsafe { &*result };
    if index >= result.count || result.items.is_null() {
        return std::ptr::null();
    }
    unsafe { result.items.add(index as usize) }
}

/// Get a pointer to the `index`-th `FffScore` in a mixed search result.
///
/// ## Safety
/// `result` must be a valid `FffMixedSearchResult` pointer from `fff_search_mixed`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fff_mixed_search_result_get_score(
    result: *const FffMixedSearchResult,
    index: u32,
) -> *const FffScore {
    if result.is_null() {
        return std::ptr::null();
    }
    let result = unsafe { &*result };
    if index >= result.count || result.scores.is_null() {
        return std::ptr::null();
    }
    unsafe { result.scores.add(index as usize) }
}

fn search_request(
    query: &str,
    current_file: Option<&str>,
    page_index: u32,
    page_size: usize,
    exclude_frecency: bool,
) -> SearchRequest {
    SearchRequest {
        query: query.to_owned(),
        current_file: current_file.map(str::to_owned),
        offset: page_index as usize,
        limit: page_size,
        exclude_frecency,
    }
}

// The work tree of the repository containing `path`, or git's reason for none.
pub(crate) fn discover_workdir(path: &Path) -> Result<Option<PathBuf>, String> {
    git2::Repository::discover(path)
        .map(|repo| repo.workdir().map(Path::to_path_buf))
        .map_err(|e| e.message().to_string())
}

pub(crate) fn git_health(workdir: Result<Option<PathBuf>, String>) -> serde_json::Value {
    let (major, minor, rev) = git2::Version::get().libgit2_version();
    let mut git_info = serde_json::Map::new();
    git_info.insert(
        "libgit2_version".to_string(),
        serde_json::Value::String(format!("{}.{}.{}", major, minor, rev)),
    );
    git_info.insert("available".to_string(), serde_json::Value::Bool(true));
    git_info.insert(
        "repository_found".to_string(),
        serde_json::Value::Bool(workdir.is_ok()),
    );
    match workdir {
        Ok(Some(workdir)) => {
            git_info.insert(
                "workdir".to_string(),
                serde_json::Value::String(workdir.to_string_lossy().to_string()),
            );
        }
        Ok(None) => {}
        Err(error) => {
            git_info.insert("error".to_string(), serde_json::Value::String(error));
        }
    }
    serde_json::Value::Object(git_info)
}

fn health_result(health: &serde_json::Map<String, serde_json::Value>) -> *mut FffResult {
    match serde_json::to_string(health) {
        Ok(json) => FffResult::ok_string(&json),
        Err(e) => FffResult::err(&format!("Failed to serialize health check: {}", e)),
    }
}
