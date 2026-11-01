use super::types::GrepSearchOptions;
use crate::index::BigramFilter;
use crate::index::constraints::{ConstraintPlan, ConstraintsBuffers};
use crate::scope::ResolvedScope;
use crate::simd_path::Arenas;
use crate::sort_buffer::sort_with_buffer;
use crate::types::FileItem;
use fff_query_parser::Constraint;
use smallvec::{SmallVec, smallvec};
use std::cmp::Ordering;
use std::ops::Range;

const FIRST_RANKED: usize = 8192;

pub(super) struct RankedFiles<'a> {
    files: Vec<&'a FileItem>,
    ranked: usize,
    offset: usize,
}

fn rank(a: &&FileItem, b: &&FileItem) -> Ordering {
    b.total_frecency_score()
        .cmp(&a.total_frecency_score())
        .then_with(|| b.modified.cmp(&a.modified))
}

fn rank_with_ties(a: &&FileItem, b: &&FileItem) -> Ordering {
    // All references point into one input slice, whose order breaks ties.
    rank(a, b).then_with(|| std::ptr::from_ref(*a).cmp(&std::ptr::from_ref(*b)))
}

impl<'a> RankedFiles<'a> {
    fn new(mut files: Vec<&'a FileItem>, options: &GrepSearchOptions) -> Self {
        let offset = options.file_offset.min(files.len());
        let prefix = offset.saturating_add(FIRST_RANKED);
        let ranked = if offset == files.len() {
            files.len()
        } else if prefix <= files.len() / 8 && options.page_limit <= FIRST_RANKED {
            files.select_nth_unstable_by(prefix, rank_with_ties);
            files[..prefix].sort_unstable_by(rank_with_ties);
            prefix
        } else {
            sort_with_buffer(&mut files, rank);
            files.len()
        };
        Self {
            files,
            ranked,
            offset,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.files.len() - self.offset
    }

    pub(super) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(super) fn batch(&mut self, range: Range<usize>) -> &[&'a FileItem] {
        let range = self.offset + range.start..self.offset + range.end;
        if range.end > self.ranked {
            self.files[self.ranked..].sort_unstable_by(rank_with_ties);
            self.ranked = self.files.len();
        }
        &self.files[range]
    }
}

/// Prefilter with a FilePath-constraint fallback: when constraints yield 0
/// files and the query had FilePath constraints, retry without them (the path
/// token was likely part of the search text).
pub(super) fn prefilter_with_filepath_retry<'a>(
    files: &'a [FileItem],
    constraints: &[Constraint<'_>],
    bigram_candidates: Option<&[u64]>,
    base_count: usize,
    options: &GrepSearchOptions,
    arenas: Arenas,
    scope: Option<&ResolvedScope>,
) -> (RankedFiles<'a>, usize) {
    let (files_to_search, filtered_file_count) = prefilter_files(
        files,
        constraints,
        bigram_candidates,
        base_count,
        options,
        arenas,
        scope,
    );

    if !files_to_search.is_empty()
        || options.classify_definitions == super::DefinitionMode::DefinitionOnly
    {
        return (files_to_search, filtered_file_count);
    }

    let Some(stripped) = strip_file_path_constraint_if_present(constraints) else {
        return (files_to_search, filtered_file_count);
    };

    prefilter_files(
        files,
        &stripped,
        bigram_candidates,
        base_count,
        options,
        arenas,
        scope,
    )
}

/// Single pass prefilter that doesn't involve file reading
/// allocates only amount of memory required for storing references of the FileItems have to be
/// opened for grepping unaviodably, in the worst case allocates N * <word> memory if no prefilter needed
///
/// A scope limits the pass to the index ranges that can hold its files.
pub(super) fn prefilter_files<'a>(
    files: &'a [FileItem],
    constraints: &[Constraint<'_>],
    bigram_candidates: Option<&[u64]>,
    base_count: usize,
    options: &GrepSearchOptions,
    arenas: Arenas,
    scope: Option<&ResolvedScope>,
) -> (RankedFiles<'a>, usize) {
    let max_file_size = options.max_file_size;
    let definition_only = options.classify_definitions == super::DefinitionMode::DefinitionOnly;
    let candidate_count = bigram_candidates.map(BigramFilter::count_candidates);
    // Sparse content candidates need only inline glob checks, not a full-index path pass.
    let sparse = candidate_count.is_some_and(|count| count < files.len() / 2);
    let plan = (!constraints.is_empty() || scope.is_some())
        .then(|| ConstraintPlan::build_with_prefilter(constraints, files, arenas, scope, sparse));

    let mut scratch = ConstraintsBuffers::new();

    #[inline(always)]
    fn basic_prefilter(file: &FileItem, max: u64, definition_only: bool) -> bool {
        !file.is_deleted()
            && !file.is_binary()
            && file.size > 0
            && file.size <= max
            && (!definition_only || file.supports_definitions())
    }

    // Files below the boundary are covered by the bigram index; the rest
    // (non-bigrammable and watcher-appended files) are always checked in order.
    let boundary = base_count.min(files.len());
    let spans: SmallVec<[Range<usize>; 3]> = match scope {
        Some(scope) => scope.file_spans(files.len()).into_iter().collect(),
        None => smallvec![0..files.len()],
    };

    // squeeze as much prefilters into a single loop as possible
    let mut prefiltered: Vec<&FileItem> = match bigram_candidates {
        Some(_) => Vec::with_capacity(candidate_count.unwrap_or(0) + (files.len() - boundary)),
        // this will be executed if there is no bigram, in the worst case it will allocate
        // whole array of files but probability in the real repo of NO preflter working is so
        // low that we just ignore that, usually there would be at least a few files excluded
        None => Vec::new(),
    };
    let mut consider = |file_idx: usize| {
        let f = &files[file_idx];
        if !basic_prefilter(f, max_file_size, definition_only) {
            return;
        }
        if let Some(plan) = plan.as_ref()
            && !plan.matches(f, file_idx, arenas, &mut scratch)
        {
            return;
        }
        prefiltered.push(f);
    };

    for span in spans {
        let linear_start = match bigram_candidates {
            Some(candidates) => {
                let indexed_end = span.end.min(boundary);
                if span.start < indexed_end {
                    for_each_candidate(candidates, span.start..indexed_end, &mut consider);
                }
                span.start.max(indexed_end)
            }
            None => span.start,
        };
        for file_idx in linear_start..span.end {
            consider(file_idx);
        }
    }

    let total_count = prefiltered.len();

    (RankedFiles::new(prefiltered, options), total_count)
}

/// Calls `f` with every candidate bit set inside `range`. Bits outside the
/// range are masked off at word load, so a partial word never leaks files the
/// bigram index does not cover.
fn for_each_candidate(candidates: &[u64], range: Range<usize>, f: &mut impl FnMut(usize)) {
    let first_word = range.start / 64;
    let last_word = (range.end - 1) / 64;
    for (word_idx, &word) in candidates
        .iter()
        .enumerate()
        .take(last_word + 1)
        .skip(first_word)
    {
        let mut bits = word;
        if word_idx == first_word {
            bits &= !0u64 << (range.start % 64);
        }
        let word_end = range.end - word_idx * 64;
        if word_end < 64 {
            bits &= (1u64 << word_end) - 1;
        }
        while bits != 0 {
            let bit = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            f(word_idx * 64 + bit);
        }
    }
}

fn strip_file_path_constraint_if_present<'a>(
    constraints: &[Constraint<'a>],
) -> Option<fff_query_parser::ConstraintVec<'a>> {
    if !constraints
        .iter()
        .any(|c| matches!(c, Constraint::FilePath(_)))
    {
        return None;
    }

    let filtered: fff_query_parser::ConstraintVec<'a> = constraints
        .iter()
        .filter(|c| !matches!(c, Constraint::FilePath(_)))
        .cloned()
        .collect();

    Some(filtered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grep::DefinitionMode;
    use crate::simd_path::ArenaPtr;

    #[test]
    fn partial_ranking_matches_stable_sort_across_batch_boundaries() {
        let files: Vec<_> = (0..100_000)
            .map(|i| {
                let mut file = FileItem::new_raw(0, 1, (i * 17 % 31) as u64, None, false);
                file.access_frecency_score = (i * 13 % 7) as i16;
                file
            })
            .collect();
        let candidates: Vec<_> = files.iter().filter(|file| file.modified != 8).collect();
        let mut expected = candidates.clone();
        expected.sort_by(rank);
        for (offset, limit) in [
            (0, 50),
            (971, 50),
            (50_000, 50),
            (0, 10_000),
            (usize::MAX, 50),
        ] {
            let mut list = RankedFiles::new(
                candidates.clone(),
                &GrepSearchOptions {
                    file_offset: offset,
                    page_limit: limit,
                    ..Default::default()
                },
            );
            let expected = &expected[offset.min(expected.len())..];
            assert_eq!(list.len(), expected.len());
            let mut start = 0;
            for end in [1, FIRST_RANKED - 1, FIRST_RANKED + 3, list.len()] {
                let end = end.min(list.len());
                assert!(
                    list.batch(start..end)
                        .iter()
                        .zip(&expected[start..end])
                        .all(|(a, b)| std::ptr::eq(*a, *b))
                );
                start = end;
            }
        }
    }

    #[test]
    fn definition_only_prefilters_before_sorting_and_offsets() {
        let files: Vec<_> = (0..70)
            .map(|idx| {
                let size = match idx {
                    9 => 0,
                    12 => 101,
                    _ => 10,
                };
                let mut file = FileItem::new_raw(0, size, 1000 - idx, None, idx == 6);
                file.mark_definition_language(["file.rs", "file.txt", "file.py"][idx as usize % 3]);
                file.set_deleted(idx == 8);
                file.set_overflow(idx >= 65);
                file
            })
            .collect();
        let all = [u64::MAX, 1];
        let even = [0x5555_5555_5555_5555, 1];
        for candidates in [None, Some(all.as_slice()), Some(even.as_slice())] {
            for mode in [
                DefinitionMode::Off,
                DefinitionMode::Classify,
                DefinitionMode::DefinitionOnly,
                DefinitionMode::UsageOnly,
            ] {
                let expected: Vec<_> = (0..70)
                    .filter(|idx| ![6, 8, 9, 12].contains(idx))
                    .filter(|idx| mode != DefinitionMode::DefinitionOnly || idx % 3 != 1)
                    .filter(|idx| {
                        *idx >= 65
                            || candidates.is_none_or(|bits| bits[idx / 64] & (1 << (idx % 64)) != 0)
                    })
                    .map(|idx| 1000 - idx as u64)
                    .collect();
                for offset in [0, 2, expected.len(), expected.len() + 1] {
                    let options = GrepSearchOptions {
                        max_file_size: 100,
                        classify_definitions: mode,
                        file_offset: offset,
                        ..Default::default()
                    };
                    let (mut result, count) = prefilter_files(
                        &files,
                        &[],
                        candidates,
                        65,
                        &options,
                        Arenas::single(ArenaPtr::null()),
                        None,
                    );
                    assert_eq!(count, expected.len());
                    assert_eq!(
                        result
                            .batch(0..result.len())
                            .iter()
                            .map(|file| file.modified)
                            .collect::<Vec<_>>(),
                        expected[offset.min(expected.len())..],
                    );
                }
            }
        }
    }
}
