use fff_search::file_picker::FilePicker;
use fff_search::{
    DirSearchConfig, FilePickerOptions, FuzzySearchOptions, GrepMode, GrepSearchOptions,
    MixedItemRef, PaginationArgs, QueryParser, parse_grep_query,
};

const THREADS: usize = 4;
const ROUNDS: usize = 2;

// Every search kind runs from several threads at once over one shared index and
// must return exactly what it returns alone.
#[test]
fn concurrent_searches_over_one_index_match_sequential_results() {
    let dir = tempfile::tempdir().unwrap();
    for group in 0..128 {
        let group_dir = dir.path().join(format!("src/group_{group}"));
        std::fs::create_dir_all(&group_dir).unwrap();
        for index in 0..100 {
            let name = ["controller", "model", "service", "utils"][index % 4];
            let ext = ["rs", "ts"][index % 2];
            let body = if (group + index) % 5 == 0 {
                format!("fn f{index}() {{ SharedNeedle::new({group}); }}\nOtherToken\n")
            } else {
                format!("fn f{index}() {{ let value = {group}; }}\n")
            };
            std::fs::write(group_dir.join(format!("{name}_{index}.{ext}")), body).unwrap();
        }
    }
    let mut picker = FilePicker::new(FilePickerOptions {
        base_path: dir.path().to_string_lossy().into_owned(),
        enable_mmap_cache: false,
        watch: false,
        ..Default::default()
    })
    .unwrap();
    picker.collect_files().unwrap();
    assert_eq!(picker.live_file_count(), 12_800);

    let run_all = || -> Vec<Vec<String>> { searches(&picker) };
    let expected = run_all();
    let sizes: Vec<_> = expected.iter().map(Vec::len).collect();
    assert!(sizes.iter().all(|&size| size > 0), "{sizes:?}");

    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..THREADS)
            .map(|_| {
                scope.spawn(|| {
                    for _ in 0..ROUNDS {
                        assert_eq!(run_all(), expected);
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
    });
}

fn searches(picker: &FilePicker) -> Vec<Vec<String>> {
    let grep = |text: &str, mode: GrepMode| {
        let options = GrepSearchOptions {
            page_limit: usize::MAX,
            max_matches_per_file: 0,
            mode,
            ..Default::default()
        };
        let result = picker.grep(&parse_grep_query(text), &options);
        result
            .matches
            .iter()
            .map(|m| {
                let path = result.files[m.file_index].relative_path(picker);
                format!("{path}:{}:{:?}", m.line_number, m.match_byte_offsets)
            })
            .collect::<Vec<_>>()
    };
    let options = FuzzySearchOptions {
        max_threads: 0,
        pagination: PaginationArgs {
            offset: 0,
            limit: 0,
        },
        ..Default::default()
    };
    let parser = QueryParser::default();
    let files = |text: &str| {
        let query = parser.parse(text);
        let result = picker.fuzzy_search(&query, None, options);
        result
            .items
            .iter()
            .zip(&result.scores)
            .map(|(file, score)| format!("{} {}", file.relative_path(picker), score.total))
            .collect::<Vec<_>>()
    };
    let dir_parser = QueryParser::new(DirSearchConfig);
    let dirs = picker
        .fuzzy_search_directories(&dir_parser.parse("group"), options)
        .items
        .iter()
        .map(|dir| dir.relative_path(picker))
        .collect();
    let mixed = picker
        .fuzzy_search_mixed(&dir_parser.parse("group_1"), None, options)
        .items
        .iter()
        .map(|item| match item {
            MixedItemRef::File(file) => file.relative_path(picker),
            MixedItemRef::Dir(dir) => dir.relative_path(picker),
        })
        .collect();
    let multi = {
        let options = GrepSearchOptions {
            page_limit: usize::MAX,
            max_matches_per_file: 0,
            ..Default::default()
        };
        let constraints = parser.parse_constraints("*.rs");
        let result = picker.multi_grep(&["SharedNeedle", "OtherToken"], &constraints, &options);
        result
            .matches
            .iter()
            .map(|m| {
                let path = result.files[m.file_index].relative_path(picker);
                format!("{path}:{}", m.line_number)
            })
            .collect()
    };
    let glob = picker
        .glob("**/*.rs", options)
        .items
        .iter()
        .map(|file| file.relative_path(picker))
        .collect();

    vec![
        grep("SharedNeedle", GrepMode::PlainText),
        grep("Shared[A-Z]eedle", GrepMode::Regex),
        grep("SharedNedle", GrepMode::Fuzzy),
        multi,
        files("controller"),
        files("group_1 serv"),
        files("**/*.rs controller"),
        files("group_1/ controller"),
        files(""),
        glob,
        dirs,
        mixed,
    ]
}
