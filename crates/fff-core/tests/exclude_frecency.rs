use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use fff_search::file_picker::FilePicker;
use fff_search::frecency::FrecencyTracker;
use fff_search::{
    DirSearchConfig, FilePickerOptions, FuzzySearchOptions, GrepSearchOptions, MixedItemRef,
    MixedSearchConfig, PaginationArgs, QueryParser, SharedFilePicker, SharedFrecency,
};
use tempfile::TempDir;

const TREE: &[&str] = &[
    "b.rs",
    "a.rs",
    "dir1/c.rs",
    "dir1/sub/d.rs",
    "dir10/e.rs",
    "dir2/zz.rs",
];

struct Fixture {
    _dir: TempDir,
    root: PathBuf,
    tracked: SharedFilePicker,
    plain: FilePicker,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let root = fff_search::path_utils::canonicalize(dir.path()).unwrap();
        let project = root.join("project");
        // Later entries are older, so grep orders the frecent `dir2/zz.rs` last by time.
        let now = SystemTime::now();
        for (age, path) in TREE.iter().enumerate() {
            let path = project.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "fn main() {}\n").unwrap();
            let modified = now - Duration::from_secs(60 * (age as u64 + 1));
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(modified).unwrap();
        }
        let tracker = FrecencyTracker::open(root.join("frecency")).unwrap();
        for _ in 0..5 {
            tracker.track_access(&project.join("dir2/zz.rs")).unwrap();
        }
        let frecency = SharedFrecency::default();
        frecency.init(tracker).unwrap();
        let tracked = SharedFilePicker::default();
        FilePicker::new_with_shared_state(tracked.clone(), frecency, options(&project)).unwrap();
        assert!(tracked.wait_for_scan(Duration::from_secs(30)));
        let mut plain = FilePicker::new(options(&project)).unwrap();
        plain.collect_files().unwrap();
        Self {
            _dir: dir,
            root: project,
            tracked,
            plain,
        }
    }
}

fn options(root: &Path) -> FilePickerOptions {
    FilePickerOptions {
        base_path: root.to_str().unwrap().into(),
        watch: false,
        ..Default::default()
    }
}

fn search_options(
    exclude_frecency: bool,
    offset: usize,
    limit: usize,
) -> FuzzySearchOptions<'static> {
    FuzzySearchOptions {
        pagination: PaginationArgs { offset, limit },
        exclude_frecency,
        ..Default::default()
    }
}

fn files(picker: &FilePicker, query: &str, options: FuzzySearchOptions) -> Vec<(String, i32)> {
    let parsed = QueryParser::default().parse(query);
    let result = picker.fuzzy_search(&parsed, None, options);
    let paths = result.items.iter().map(|file| file.relative_path(picker));
    paths.zip(result.scores.iter().map(|s| s.total)).collect()
}

fn paths(results: &[(String, i32)]) -> Vec<&str> {
    results.iter().map(|(path, _)| path.as_str()).collect()
}

fn sorted(mut paths: Vec<String>) -> Vec<String> {
    paths.sort();
    paths
}

#[test]
fn empty_query_lists_files_in_path_order_without_frecency() {
    let fixture = Fixture::new();
    let guard = fixture.tracked.read().unwrap();
    let picker = guard.as_ref().unwrap();

    let ranked = files(picker, "", search_options(false, 0, 0));
    assert_eq!(ranked[0].0, "dir2/zz.rs", "frecency ranks its file first");

    let listed = files(picker, "", search_options(true, 0, 0));
    let expected = sorted(TREE.iter().map(|path| path.to_string()).collect());
    assert_eq!(paths(&listed), expected);
    assert!(listed.iter().all(|(_, total)| *total == 0));

    let page = files(picker, "", search_options(true, 2, 2));
    assert_eq!(paths(&page), &expected[2..4]);
    assert!(files(picker, "", search_options(true, 10, 2)).is_empty());

    let globbed = picker.glob("**/*.rs", search_options(true, 0, 0));
    let globbed: Vec<_> = globbed
        .items
        .iter()
        .map(|f| f.relative_path(picker))
        .collect();
    assert_eq!(globbed, expected);
}

#[test]
fn files_added_after_the_scan_take_their_place_in_path_order() {
    let fixture = Fixture::new();
    let added = fixture.root.join("dir1/aa.rs");
    std::fs::write(&added, "").unwrap();
    let mut guard = fixture.tracked.write().unwrap();
    let picker = guard.as_mut().unwrap();
    assert!(picker.handle_create_or_modify(&added).is_some());

    let listed = files(picker, "", search_options(true, 0, 0));
    let mut expected: Vec<String> = TREE.iter().map(|path| path.to_string()).collect();
    expected.push("dir1/aa.rs".into());
    assert_eq!(paths(&listed), sorted(expected));
}

#[test]
fn fuzzy_scores_match_an_index_without_frecency() {
    let fixture = Fixture::new();
    let guard = fixture.tracked.read().unwrap();
    let picker = guard.as_ref().unwrap();
    let by_path = |results: Vec<(String, i32)>| results.into_iter().collect::<BTreeMap<_, _>>();

    let excluded = by_path(files(picker, "rs", search_options(true, 0, 0)));
    let plain = by_path(files(&fixture.plain, "rs", search_options(false, 0, 0)));
    assert_eq!(excluded, plain);
    let ranked = by_path(files(picker, "rs", search_options(false, 0, 0)));
    assert!(ranked["dir2/zz.rs"] > plain["dir2/zz.rs"]);
}

#[test]
fn directories_and_mixed_results_follow_path_order_without_frecency() {
    let fixture = Fixture::new();
    let guard = fixture.tracked.read().unwrap();
    let picker = guard.as_ref().unwrap();

    let dir_query = QueryParser::new(DirSearchConfig).parse("");
    let ranked = picker.fuzzy_search_directories(&dir_query, search_options(false, 0, 0));
    assert_eq!(ranked.items[0].relative_path(picker), "dir2/");
    let listed = picker.fuzzy_search_directories(&dir_query, search_options(true, 0, 0));
    let dirs: Vec<_> = listed
        .items
        .iter()
        .map(|d| d.relative_path(picker))
        .collect();
    assert_eq!(dirs, sorted(dirs.clone()));
    assert!(dirs.contains(&"dir2/".to_string()) && dirs.contains(&"dir1/sub/".to_string()));

    let mixed_query = QueryParser::new(MixedSearchConfig).parse("");
    let path = |item: &MixedItemRef| match item {
        MixedItemRef::File(file) => file.relative_path(picker),
        MixedItemRef::Dir(dir) => dir.relative_path(picker),
    };
    let listed = picker.fuzzy_search_mixed(&mixed_query, None, search_options(true, 0, 0));
    let all: Vec<_> = listed.items.iter().map(path).collect();
    assert_eq!(all, sorted(all.clone()));
    assert!(all.contains(&"dir1/c.rs".to_string()) && all.contains(&"dir1/".to_string()));
    let page = picker.fuzzy_search_mixed(&mixed_query, None, search_options(true, 3, 4));
    let page: Vec<_> = page.items.iter().map(path).collect();
    assert_eq!(page, &all[3..7]);
}

fn grep_order(picker: &FilePicker, exclude_frecency: bool, multi: bool) -> Vec<String> {
    let options = GrepSearchOptions {
        exclude_frecency,
        ..Default::default()
    };
    let result = if multi {
        picker.multi_grep(&["fn main"], &[], &options)
    } else {
        picker.grep(&fff_search::grep::parse_grep_query("fn main"), &options)
    };
    result
        .files
        .iter()
        .map(|f| f.relative_path(picker))
        .collect()
}

#[test]
fn grep_orders_files_as_an_index_without_frecency() {
    let fixture = Fixture::new();
    let guard = fixture.tracked.read().unwrap();
    let picker = guard.as_ref().unwrap();

    for multi in [false, true] {
        let ranked = grep_order(picker, false, multi);
        assert_eq!(ranked[0], "dir2/zz.rs", "frecency orders its file first");
        let excluded = grep_order(picker, true, multi);
        assert_eq!(excluded, grep_order(&fixture.plain, false, multi));
        assert_eq!(excluded.len(), TREE.len());
        assert_eq!(excluded.last().unwrap(), "dir2/zz.rs");
    }
}
