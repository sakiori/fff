use crate::view::SessionView;
use rmcp::{model::ErrorData, schemars};
use std::cmp::Reverse;
use std::collections::BinaryHeap;

#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FilePatternMode {
    Fuzzy,
    Glob,
    Regex,
}

// Session-relative paths of one page of matching files, and the number of matches.
pub(crate) fn search(
    view: &SessionView<'_>,
    pattern: &str,
    mode: FilePatternMode,
    constraints: &[(String, bool)],
    filters: &[String],
    offset: usize,
    limit: usize,
) -> Result<(Vec<String>, usize), ErrorData> {
    let filters = filters
        .iter()
        .map(|pattern| {
            regex::Regex::new(pattern)
                .map_err(|e| ErrorData::invalid_params(format!("Invalid path regex: {e}"), None))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let regex = if matches!(mode, FilePatternMode::Glob) {
        None
    } else {
        let pattern = if matches!(mode, FilePatternMode::Fuzzy) {
            fragment_pattern(pattern)
        } else {
            pattern.to_string()
        };
        Some(
            regex::RegexBuilder::new(&pattern)
                .case_insensitive(matches!(mode, FilePatternMode::Fuzzy))
                .build()
                .map_err(|e| ErrorData::invalid_params(format!("Invalid path regex: {e}"), None))?,
        )
    };
    let mut globs = constraints
        .iter()
        .map(|(p, exclude)| crate::scope::compile_glob(p).map(|g| (g.compile_matcher(), *exclude)))
        .collect::<Result<Vec<_>, _>>()?;
    if matches!(mode, FilePatternMode::Glob) && !pattern.is_empty() {
        let pattern = if pattern.contains('/') {
            pattern.to_string()
        } else {
            format!("**/{pattern}")
        };
        globs.push((
            crate::scope::compile_glob(&pattern)?.compile_matcher(),
            false,
        ));
    }
    let files = view.picker.get_files();
    let capacity = offset.saturating_add(limit);
    let fuzzy = matches!(mode, FilePatternMode::Fuzzy);
    // Each range keeps its best `capacity` paths; the merge keeps the global best.
    let (count, matches) = fff::map_search_ranges(
        files.len(),
        || (),
        |_, range, out| {
            let mut count = 0usize;
            let mut best = BinaryHeap::new();
            for file in &files[range] {
                if file.is_deleted() {
                    continue;
                }
                let Some(path) = view.file_path(file) else {
                    continue;
                };
                if regex.as_ref().is_some_and(|regex| !regex.is_match(&path))
                    || filters.iter().any(|regex| !regex.is_match(&path))
                    || globs
                        .iter()
                        .any(|(glob, exclude)| glob.is_match(&path) == *exclude)
                {
                    continue;
                }
                count += 1;
                let rank = if fuzzy {
                    literal_rank(&path, pattern)
                } else {
                    (0, false, false)
                };
                let depth = path.bytes().filter(|&b| b == b'/').count();
                let frecency = Reverse(file.total_frecency_score());
                keep_best(&mut best, (rank, depth, frecency, path), capacity);
            }
            out.push((count, best));
        },
    )
    .into_iter()
    .fold(
        (0, BinaryHeap::new()),
        |(total, mut left), (count, right)| {
            for key in right {
                keep_best(&mut left, key, capacity);
            }
            (total + count, left)
        },
    );
    Ok((
        matches
            .into_sorted_vec()
            .into_iter()
            .skip(offset)
            .map(|(.., path)| path)
            .collect(),
        count,
    ))
}

// Tier, then ties: an uppercase pattern prefers its exact case; exact names in `lsp/lsp.ts` beat `config/lsp.ts`.
fn literal_rank(path: &str, pattern: &str) -> (u8, bool, bool) {
    let mut parts = path.rsplit('/');
    let name = parts.next().unwrap_or(path);
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    let tier = if equal_fragment(name, pattern) || equal_fragment(stem, pattern) {
        0
    } else if path.split('/').any(|part| equal_fragment(part, pattern)) {
        1
    } else if name
        .get(..pattern.len())
        .is_some_and(|prefix| equal_fragment(prefix, pattern))
    {
        2
    } else {
        3
    };
    let case_miss = pattern.bytes().any(|b| b.is_ascii_uppercase()) && !path.contains(pattern);
    let parent_miss = tier == 0 && !parts.next().is_some_and(|dir| equal_fragment(dir, stem));
    (tier, case_miss, parent_miss)
}

fn fragment_pattern(pattern: &str) -> String {
    let mut escaped = String::with_capacity(pattern.len());
    for (i, part) in pattern.split(['-', '_']).enumerate() {
        if i > 0 {
            escaped.push_str("[-_]");
        }
        escaped.push_str(&regex::escape(part));
    }
    escaped
}

fn equal_fragment(left: &str, right: &str) -> bool {
    left.len() == right.len()
        && left.bytes().zip(right.bytes()).all(|(a, b)| {
            a.eq_ignore_ascii_case(&b) || (matches!(a, b'-' | b'_') && matches!(b, b'-' | b'_'))
        })
}

fn keep_best<T: Ord>(best: &mut BinaryHeap<T>, key: T, capacity: usize) {
    if best.len() < capacity {
        best.push(key);
    } else if best.peek().is_some_and(|worst| key < *worst) {
        *best.peek_mut().unwrap() = key;
    }
}
