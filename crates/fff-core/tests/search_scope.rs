use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use fff_search::file_picker::FilePicker;
use fff_search::grep::{GrepMode, GrepSearchOptions};
use fff_search::scope::{ScopeError, SearchScope};
use fff_search::{
    AiGrepConfig, Constraint, DirSearchConfig, FFFQuery, FilePickerOptions, FuzzySearchOptions,
    GrepConfig, QueryParser,
};
use tempfile::TempDir;

const TREE: &[(&str, &str)] = &[
    ("inside.rs", "fn ScopeNeedle() {}\n"),
    ("sub/README.md", "ScopeNeedle in the readme\n"),
    (
        "sub/src/inside.rs",
        "fn ScopeNeedle() {}\nlet x = a != b;\n",
    ),
    ("sub/src/other.rs", "fn other() { ScopeNeedle(); }\n"),
    ("sub/src/deep/inside.rs", "// ScopeNeedle deep\n"),
    ("sub/lib/inside_helper.rs", "fn helper() {}\n"),
    ("sub/notes.txt", "foo !bar_baz qux\n"),
    (
        "submarine/src/inside.rs",
        "fn ScopeNeedle() {}\nlet x = a != b;\n",
    ),
    ("submarine/notes.txt", "foo !bar_baz qux\n"),
];

struct Fixture {
    _dir: TempDir,
    root: PathBuf,
    picker: FilePicker,
    scope: SearchScope,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let root = fff_search::path_utils::canonicalize(dir.path()).unwrap();
        for (path, content) in TREE {
            write(&root, path, content);
        }
        let picker = index(&root);
        let scope = SearchScope::new(&root, &root.join("sub")).unwrap();
        Self {
            _dir: dir,
            root,
            picker,
            scope,
        }
    }

    fn standalone(&self) -> FilePicker {
        index(&self.root.join("sub"))
    }

    fn relative(&self, path: String) -> String {
        self.scope
            .relative_path(&path)
            .unwrap_or_else(|| panic!("{path} leaked out of the scope"))
            .to_owned()
    }

    fn fuzzy(&self, query: &str) -> Vec<(String, i32)> {
        let parsed = QueryParser::default().parse(query);
        let result = self
            .picker
            .fuzzy_search(&parsed, None, self.fuzzy_options());
        let paths = result
            .items
            .iter()
            .map(|file| self.relative(file.relative_path(&self.picker)));
        paths.zip(result.scores.iter().map(|s| s.total)).collect()
    }

    fn fuzzy_options(&self) -> FuzzySearchOptions<'_> {
        FuzzySearchOptions {
            scope: &self.scope,
            ..Default::default()
        }
    }

    fn grep_options(&self, mode: GrepMode) -> GrepSearchOptions {
        GrepSearchOptions {
            mode,
            scope: self.scope.clone(),
            ..Default::default()
        }
    }

    fn grep_files(&self, query: &FFFQuery<'_>, options: &GrepSearchOptions) -> BTreeSet<String> {
        let result = self.picker.grep(query, options);
        result
            .files
            .iter()
            .map(|file| self.relative(file.relative_path(&self.picker)))
            .collect()
    }
}

fn write(root: &Path, path: &str, content: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn index(root: &Path) -> FilePicker {
    let mut picker = FilePicker::new(FilePickerOptions {
        base_path: root.to_str().unwrap().into(),
        watch: false,
        ..Default::default()
    })
    .unwrap();
    picker.collect_files().unwrap();
    picker
}

fn standalone_fuzzy(picker: &FilePicker, query: &str) -> Vec<(String, i32)> {
    let parsed = QueryParser::default().parse(query);
    let result = picker.fuzzy_search(&parsed, None, FuzzySearchOptions::default());
    let paths = result.items.iter().map(|file| file.relative_path(picker));
    paths.zip(result.scores.iter().map(|s| s.total)).collect()
}

fn set<'a>(paths: impl IntoIterator<Item = &'a str>) -> BTreeSet<String> {
    paths.into_iter().map(str::to_owned).collect()
}

#[test]
fn scope_construction_uses_the_index_form() {
    let root = Path::new("/index");
    assert!(SearchScope::new(root, root).unwrap().is_root());
    assert_eq!(
        SearchScope::new(root, &root.join("a/b")).unwrap().dir(),
        "a/b/"
    );
    assert!(matches!(
        SearchScope::new(root, Path::new("/other")),
        Err(ScopeError::OutsideIndex { .. })
    ));
    assert!(matches!(
        SearchScope::new(root, &root.join("a/../b")),
        Err(ScopeError::NotNormalized(_))
    ));

    let f = Fixture::new();
    assert!(f.picker.contains_scope(&f.scope));
    assert!(f.picker.contains_scope(SearchScope::root()));
    let missing = SearchScope::new(&f.root, &f.root.join("missing")).unwrap();
    assert!(!f.picker.contains_scope(&missing));
}

#[test]
fn fuzzy_ranking_matches_a_standalone_index() {
    let f = Fixture::new();
    let standalone = f.standalone();
    for query in [
        "inside",
        "ins",
        "src/",
        "sub",
        "rs",
        "",
        "deep inside",
        "src/inside.rs",
    ] {
        assert_eq!(
            f.fuzzy(query),
            standalone_fuzzy(&standalone, query),
            "query: {query}"
        );
    }

    let parsed = QueryParser::default().parse("inside");
    let scoped = f.picker.fuzzy_search(&parsed, None, f.fuzzy_options());
    let independent = standalone.fuzzy_search(&parsed, None, FuzzySearchOptions::default());
    assert_eq!(scoped.total_files, independent.total_files);
    assert_eq!(scoped.total_matched, independent.total_matched);
    // Highlights index the scope-relative path, like a standalone index.
    assert_eq!(scoped.match_byte_offsets, independent.match_byte_offsets);
    let top = f.relative(scoped.items[0].relative_path(&f.picker));
    let (start, end) = scoped.match_byte_offsets[0][0];
    assert_eq!(&top[start as usize..end as usize], "inside");
}

#[test]
fn sibling_prefix_stays_outside() {
    let f = Fixture::new();
    let everything = f.fuzzy("");
    assert!(
        everything
            .iter()
            .all(|(path, _)| !path.starts_with("marine"))
    );
    assert_eq!(everything.len(), 6);

    let parser = QueryParser::new(GrepConfig);
    for mode in [GrepMode::PlainText, GrepMode::Regex, GrepMode::Fuzzy] {
        let files = f.grep_files(&parser.parse("ScopeNeedle"), &f.grep_options(mode));
        assert!(files.contains("src/inside.rs"), "{mode:?}: {files:?}");
    }

    let dirs = f.picker.fuzzy_search_directories(
        &QueryParser::new(DirSearchConfig).parse("src"),
        f.fuzzy_options(),
    );
    for dir in &dirs.items {
        f.relative(dir.relative_path(&f.picker));
    }
    let standalone = f.standalone();
    let independent = standalone.fuzzy_search_directories(
        &QueryParser::new(DirSearchConfig).parse("src"),
        FuzzySearchOptions::default(),
    );
    assert_eq!(dirs.total_dirs, independent.total_dirs);
    assert_eq!(dirs.total_matched, independent.total_matched);

    let mixed = f.picker.fuzzy_search_mixed(
        &QueryParser::new(DirSearchConfig).parse("src/"),
        None,
        f.fuzzy_options(),
    );
    assert_eq!(mixed.total_files, 6);
}

#[test]
fn constraints_and_globs_are_scope_relative() {
    let f = Fixture::new();
    let standalone = f.standalone();
    let options = f.grep_options(GrepMode::PlainText);
    let unscoped = GrepSearchOptions::default();
    let compare = |constraints: &[Constraint<'_>]| -> BTreeSet<String> {
        let result = f.picker.multi_grep(&["ScopeNeedle"], constraints, &options);
        let scoped: BTreeSet<String> = result
            .files
            .iter()
            .map(|file| f.relative(file.relative_path(&f.picker)))
            .collect();
        let result = standalone.multi_grep(&["ScopeNeedle"], constraints, &unscoped);
        let independent = result
            .files
            .iter()
            .map(|file| file.relative_path(&standalone));
        assert_eq!(scoped, independent.collect(), "{constraints:?}");
        scoped
    };

    // Pure globs take the batched pre-pass; mixed ones match inline.
    assert!(compare(&[Constraint::Glob("src/*.rs")]).contains("src/inside.rs"));
    assert!(compare(&[Constraint::Glob("sub/src/*.rs")]).is_empty());
    let mixed = [Constraint::Extension("rs"), Constraint::Glob("src/*.rs")];
    assert!(compare(&mixed).contains("src/other.rs"));
    let excluded = [Constraint::Not(Box::new(Constraint::Glob("src/**")))];
    assert_eq!(compare(&excluded), set(["README.md"]));
    assert_eq!(
        compare(&[Constraint::PathSegment("src")]),
        set(["src/inside.rs", "src/other.rs", "src/deep/inside.rs"])
    );
    assert!(compare(&[Constraint::PathSegment("sub")]).is_empty());

    let globbed = f.picker.glob("src/**/*.rs", f.fuzzy_options());
    let independent = standalone.glob("src/**/*.rs", FuzzySearchOptions::default());
    assert_eq!(globbed.total_matched, independent.total_matched);
    assert_eq!(globbed.total_files, independent.total_files);
    assert!(f.picker.glob("sub/**", f.fuzzy_options()).items.is_empty());

    let parser = QueryParser::new(GrepConfig);
    assert_eq!(
        f.grep_files(&parser.parse("src/ ScopeNeedle"), &options),
        set(["src/inside.rs", "src/other.rs", "src/deep/inside.rs"])
    );
}

#[test]
fn grep_fallbacks_stay_inside_the_scope() {
    let f = Fixture::new();
    let options = f.grep_options(GrepMode::PlainText);

    // `!bar_baz` becomes an exclusion; the literal retry must keep the scope.
    let literal = QueryParser::new(GrepConfig).parse("foo !bar_baz qux");
    let result = f.picker.grep(&literal, &options);
    assert!(result.literal_fallback);
    assert_eq!(result.matches.len(), 1);
    assert_eq!(
        f.relative(result.files[0].relative_path(&f.picker)),
        "notes.txt"
    );

    // A path token relative to the scope narrows the search...
    let pinned = FFFQuery::parse("src/inside.rs ScopeNeedle", AiGrepConfig);
    assert_eq!(f.grep_files(&pinned, &options), set(["src/inside.rs"]));
    // ...one relative to the index root matches nothing, and the FilePath
    // retry falls back to the whole scope, never outside it.
    let unpinned = FFFQuery::parse("sub/src/inside.rs ScopeNeedle", AiGrepConfig);
    assert_eq!(
        f.grep_files(&unpinned, &options),
        set([
            "README.md",
            "src/inside.rs",
            "src/other.rs",
            "src/deep/inside.rs"
        ])
    );
}

#[test]
fn grep_pages_and_counts_only_cover_the_scope() {
    let f = Fixture::new();
    let standalone = f.standalone();
    let query = QueryParser::new(GrepConfig).parse("ScopeNeedle");
    let mut options = f.grep_options(GrepMode::PlainText);
    options.page_limit = 1;
    options.max_matches_per_file = 1;

    let mut seen = BTreeSet::new();
    loop {
        let result = f.picker.grep(&query, &options);
        let independent = standalone.grep(
            &query,
            &GrepSearchOptions {
                scope: SearchScope::default(),
                ..options.clone()
            },
        );
        assert_eq!(result.total_files, independent.total_files);
        assert_eq!(result.filtered_file_count, independent.filtered_file_count);
        assert_eq!(result.next_file_offset, independent.next_file_offset);
        for file in &result.files {
            assert!(seen.insert(f.relative(file.relative_path(&f.picker))));
        }
        if result.next_file_offset == 0 {
            break;
        }
        options.file_offset = result.next_file_offset;
    }
    assert_eq!(
        seen,
        set([
            "README.md",
            "src/inside.rs",
            "src/other.rs",
            "src/deep/inside.rs"
        ])
    );
}

#[test]
fn watcher_added_and_deleted_files_follow_the_scope() {
    let mut f = Fixture::new();
    for path in ["sub/new/added.rs", "submarine/new/added.rs"] {
        write(&f.root, path, "fn ScopeNeedle() {}\n");
        let file = f.picker.handle_create_or_modify(f.root.join(path)).unwrap();
        assert!(file.is_overflow());
    }
    assert!(
        f.picker
            .remove_file_by_path(f.root.join("sub/src/other.rs"))
    );

    let paths: BTreeSet<String> = f.fuzzy("").into_iter().map(|(path, _)| path).collect();
    assert!(paths.contains("new/added.rs"));
    assert!(!paths.contains("src/other.rs"));
    assert_eq!(paths.len(), 6);

    let added = f.fuzzy("added");
    assert_eq!(added[0].0, "new/added.rs");
    let parsed = QueryParser::default().parse("added");
    let result = f.picker.fuzzy_search(&parsed, None, f.fuzzy_options());
    assert_eq!(result.total_files, 6);
    let (start, end) = result.match_byte_offsets[0][0];
    assert_eq!(&"new/added.rs"[start as usize..end as usize], "added");

    let grep = f.grep_files(
        &QueryParser::new(GrepConfig).parse("ScopeNeedle"),
        &f.grep_options(GrepMode::PlainText),
    );
    assert!(grep.contains("new/added.rs"));
    assert!(!grep.contains("src/other.rs"));

    let dirs = f.picker.fuzzy_search_directories(
        &QueryParser::new(DirSearchConfig).parse("new"),
        f.fuzzy_options(),
    );
    let dir_paths: Vec<String> = dirs
        .items
        .iter()
        .map(|dir| f.relative(dir.relative_path(&f.picker)))
        .collect();
    assert_eq!(dir_paths.first().map(String::as_str), Some("new/"));

    // A rescan replaces the index layout the scope was resolved against.
    write(&f.root, "sub/src/rescanned.rs", "fn ScopeNeedle() {}\n");
    f.picker.collect_files().unwrap();
    let paths: BTreeSet<String> = f.fuzzy("").into_iter().map(|(path, _)| path).collect();
    assert!(paths.contains("src/rescanned.rs"));
    assert!(paths.contains("new/added.rs"));
}

#[test]
fn shared_parent_matches_a_standalone_index_exactly_when_exclusions_agree() {
    let dir = TempDir::new().unwrap();
    let root = fff_search::path_utils::canonicalize(dir.path()).unwrap();
    for path in [
        "proj/src/lib.rs",
        "proj/target/debug/out.rs",
        "proj/target/target/debug/nested.rs",
        "proj/src/target/debug/generated.rs",
    ] {
        write(&root, path, "fn main() {}\n");
    }
    let parent = index(&root);
    for subtree in ["proj", "proj/src", "proj/target"] {
        let scope = SearchScope::new(&root, &root.join(subtree)).unwrap();
        let shared: BTreeSet<String> = parent
            .get_files()
            .iter()
            .filter_map(|file| {
                let path = file.relative_path(&parent);
                scope.relative_path(&path).map(str::to_owned)
            })
            .collect();
        let standalone = index(&root.join(subtree));
        let independent: BTreeSet<String> = standalone
            .get_files()
            .iter()
            .map(|file| file.relative_path(&standalone))
            .collect();
        assert_eq!(
            fff_search::ignore::subtree_keeps_ignored_dirs(Path::new(subtree)),
            shared == independent,
            "{subtree}: shared {shared:?}, standalone {independent:?}"
        );
    }
}
