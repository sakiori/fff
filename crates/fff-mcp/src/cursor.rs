use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};

const MAX_CURSORS: usize = 20;

#[derive(Clone, Copy, Default)]
pub(crate) struct GrepCursor {
    pub file_offset: usize,
    pub match_offset: usize,
    pub match_file: u64,
    pub stage: GrepStage,
    pub found_matches: bool,
    pub missing_path: bool,
}

impl GrepCursor {
    pub fn page_limit(self, requested: usize) -> usize {
        self.match_offset
            .saturating_add(requested)
            .saturating_add(1)
    }

    // Matches already shown from the file where the last page stopped.
    pub fn resume_skip(&self, result: &fff::GrepResult<'_>, picker: &fff::FilePicker) -> usize {
        match result.matches.first() {
            Some(first)
                if self.match_offset > 0
                    && file_id(result, first.file_index, picker) == self.match_file =>
            {
                result
                    .matches
                    .iter()
                    .take(self.match_offset)
                    .take_while(|m| m.file_index == first.file_index)
                    .count()
            }
            _ => 0,
        }
    }

    pub fn paginate(
        &mut self,
        result: &mut fff::GrepResult<'_>,
        requested: usize,
        picker: &fff::FilePicker,
    ) {
        let start = self.resume_skip(result, picker);
        let end = start.saturating_add(requested).min(result.matches.len());
        if end < result.matches.len() {
            let last_file = result.matches.last().unwrap().file_index;
            self.file_offset = if result.next_file_offset > 0 {
                result.next_file_offset - 1
            } else {
                result.filtered_file_count.saturating_sub(1)
            };
            self.match_offset = result.matches[..end]
                .iter()
                .rev()
                .take_while(|m| m.file_index == last_file)
                .count();
            self.match_file = file_id(result, last_file, picker);
        } else {
            self.file_offset = result.next_file_offset;
            self.match_offset = 0;
        }
        result.matches.truncate(end);
        result.matches.drain(..start);
        self.found_matches |= !result.matches.is_empty();
    }

    pub fn has_more(self) -> bool {
        self.file_offset > 0 || self.match_offset > 0
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) enum GrepStage {
    #[default]
    Normal,
    Broadened,
    Definitions,
    References,
    FuzzyDefinitions,
    FuzzyMatches,
}

impl GrepStage {
    pub fn next(self) -> Option<Self> {
        match self {
            Self::Definitions => Some(Self::References),
            Self::References => Some(Self::FuzzyDefinitions),
            Self::FuzzyDefinitions => Some(Self::FuzzyMatches),
            _ => None,
        }
    }

    pub fn is_fuzzy(self) -> bool {
        matches!(self, Self::FuzzyDefinitions | Self::FuzzyMatches)
    }

    pub fn definition_mode(self) -> fff::DefinitionMode {
        match self {
            Self::Definitions | Self::FuzzyDefinitions => fff::DefinitionMode::DefinitionOnly,
            // A file-path retry can widen these stages, so keep definitions found elsewhere.
            Self::References | Self::FuzzyMatches | Self::Normal | Self::Broadened => {
                fff::DefinitionMode::Classify
            }
        }
    }

    pub fn notice(self) -> Option<&'static str> {
        match self {
            Self::References => Some("No matching definitions found. Showing other matches:"),
            Self::FuzzyDefinitions => {
                Some("No exact matches found. Maybe you meant these definitions:")
            }
            Self::FuzzyMatches => Some(
                "No exact matches or approximate definitions found. Maybe you meant these other matches:",
            ),
            _ => None,
        }
    }
}

struct Cursor {
    context: String,
    state: GrepCursor,
}

/// Stores cursor state for paginated grep results.
pub struct CursorStore {
    // Search context for cursors created by the request in progress.
    context: String,
    cursors: HashMap<String, Cursor>,
    /// Insertion order for LRU eviction.
    insertion_order: VecDeque<String>,
}

impl CursorStore {
    pub fn new() -> Self {
        Self {
            context: String::new(),
            cursors: HashMap::new(),
            insertion_order: VecDeque::new(),
        }
    }

    /// Starts a search in `context` and returns the file offset its cursor points to.
    pub fn begin(
        &mut self,
        context: String,
        cursor: Option<&str>,
    ) -> Result<usize, rmcp::ErrorData> {
        Ok(self
            .begin_grep(context, cursor)?
            .map_or(0, |cursor| cursor.file_offset))
    }

    /// Starts a search in `context` and returns the state its cursor resumes, or `None`
    /// without a cursor. A cursor only resumes the search (and index binding) that created it.
    pub(crate) fn begin_grep(
        &mut self,
        context: String,
        cursor: Option<&str>,
    ) -> Result<Option<GrepCursor>, rmcp::ErrorData> {
        let state = match cursor {
            None => None,
            Some(id) => match self.cursors.get(id) {
                Some(entry) if entry.context == context => Some(entry.state),
                _ => {
                    return Err(rmcp::ErrorData::invalid_params(
                        "Cursor expired or belongs to another search; restart without cursor",
                        None,
                    ));
                }
            },
        };
        self.context = context;
        Ok(state)
    }

    /// Store a cursor for the current search and return its opaque ID string.
    pub fn store(&mut self, file_offset: usize) -> String {
        self.store_grep(GrepCursor {
            file_offset,
            ..Default::default()
        })
    }

    pub(crate) fn store_grep(&mut self, cursor: GrepCursor) -> String {
        let id = uuid::Uuid::new_v4().to_string();

        self.cursors.insert(
            id.clone(),
            Cursor {
                context: self.context.clone(),
                state: cursor,
            },
        );
        self.insertion_order.push_back(id.clone());

        // Evict oldest cursors
        while self.cursors.len() > MAX_CURSORS {
            if let Some(oldest) = self.insertion_order.pop_front() {
                self.cursors.remove(&oldest);
            } else {
                break;
            }
        }

        id
    }

    #[cfg(test)]
    pub(crate) fn peek(&self, id: &str) -> Option<GrepCursor> {
        self.cursors.get(id).map(|cursor| cursor.state)
    }
}

fn file_id(result: &fff::GrepResult<'_>, index: usize, picker: &fff::FilePicker) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    result.files[index].relative_path(picker).hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleaved_searches_keep_their_cursors() {
        let mut store = CursorStore::new();
        store.begin("a".into(), None).unwrap();
        let first = store.store(10);
        store.begin("b".into(), None).unwrap();
        let second = store.store(20);
        assert_eq!(store.begin("a".into(), Some(&first)).unwrap(), 10);
        assert_eq!(store.begin("b".into(), Some(&second)).unwrap(), 20);
        assert!(store.begin("b".into(), Some(&first)).is_err());
    }

    #[test]
    fn oldest_cursors_are_evicted() {
        let mut store = CursorStore::new();
        store.begin("a".into(), None).unwrap();
        let oldest = store.store(0);
        for offset in 1..=MAX_CURSORS {
            store.store(offset);
        }
        assert!(store.begin("a".into(), Some(&oldest)).is_err());
    }
}
