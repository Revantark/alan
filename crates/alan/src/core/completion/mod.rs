//! Completion for the prompt editor.
//!
//! A [`CompletionBackend`] decides for itself whether a request is its own.
//! Ranking is not its concern: [`matcher`] orders every backend the same way.

mod paths;

use super::Poll;
use super::matcher;
use std::ops::Range;

pub use paths::Paths;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionRequest {
    pub line: String,
    /// Byte offset of the cursor within `line`.
    pub cursor: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    FilePath,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    pub display: String,
    /// Text substituted for [`CompletionResult::range`].
    pub replacement: String,
    pub description: Option<String>,
    /// Directories keep the popup open so the user can drill deeper.
    pub stay_open: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionStatus {
    Loading,
    Ready,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionResult {
    pub backend: Backend,
    /// Bytes of the line that accepting overwrites.
    pub range: Range<usize>,
    pub status: CompletionStatus,
    /// Ranked best first.
    pub items: Vec<CompletionItem>,
}

pub trait CompletionBackend {
    /// The completion offered at the cursor, or `None` when this backend has
    /// nothing to do with the request.
    fn complete(&self, request: &CompletionRequest) -> Option<CompletionResult>;

    /// Called when this backend becomes the active one.
    fn refresh(&mut self) {}

    fn poll(&mut self) -> Poll {
        Poll::Idle
    }
}

struct Active {
    request: CompletionRequest,
    result: CompletionResult,
    selected: usize,
}

pub struct CompletionController {
    paths: Paths,
    active: Option<Active>,
}

impl CompletionController {
    pub fn new() -> Self {
        Self {
            paths: Paths::new(),
            active: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_paths(index: Vec<String>) -> Self {
        Self {
            paths: Paths::with_index(index),
            active: None,
        }
    }

    /// Re-evaluate after every editor change.
    pub fn sync(&mut self, line: &str, cursor: usize) {
        let request = CompletionRequest {
            line: line.to_owned(),
            cursor,
        };
        let Some(result) = self.paths.complete(&request) else {
            self.active = None;
            return;
        };
        // Becoming active is the one moment a backend's data is worth reading.
        if self.active.as_ref().map(|active| active.result.backend) != Some(result.backend) {
            self.paths.refresh();
        }
        self.active = Some(Active {
            request,
            result,
            selected: 0,
        });
    }

    pub fn is_open(&self) -> bool {
        self.active.is_some()
    }

    pub fn item_count(&self) -> usize {
        self.active
            .as_ref()
            .map_or(0, |active| active.result.items.len())
    }

    pub fn selected(&self) -> usize {
        self.active.as_ref().map_or(0, |active| active.selected)
    }

    pub fn status(&self) -> CompletionStatus {
        self.active
            .as_ref()
            .map_or(CompletionStatus::Ready, |active| {
                active.result.status.clone()
            })
    }

    /// In rank order.
    pub fn items(&self, start: usize, count: usize) -> &[CompletionItem] {
        let Some(active) = self.active.as_ref() else {
            return &[];
        };
        let start = start.min(active.result.items.len());
        let end = start.saturating_add(count).min(active.result.items.len());
        &active.result.items[start..end]
    }

    pub fn dismiss(&mut self) {
        self.active = None;
    }

    pub fn move_selection(&mut self, delta: isize) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if active.result.items.is_empty() {
            return;
        }
        let max = active.result.items.len() as isize - 1;
        active.selected = (active.selected as isize + delta).clamp(0, max) as usize;
    }

    /// The highlighted item and the byte range of the line it overwrites.
    pub fn accept(&mut self) -> Option<(CompletionItem, Range<usize>)> {
        let active = self.active.as_ref()?;
        let item = active.result.items.get(active.selected)?.clone();
        let range = active.result.range.clone();
        if !item.stay_open {
            self.active = None;
        }
        Some((item, range))
    }

    pub fn poll(&mut self) -> Poll {
        let poll = self.paths.poll();
        if poll == Poll::Changed {
            self.recompute();
        }
        poll
    }

    /// A backend whose data changed under an open popup answers again.
    fn recompute(&mut self) {
        let Some(request) = self.active.as_ref().map(|active| active.request.clone()) else {
            return;
        };
        let Some(result) = self.paths.complete(&request) else {
            self.active = None;
            return;
        };
        if let Some(active) = self.active.as_mut() {
            active.selected = active.selected.min(result.items.len().saturating_sub(1));
            active.result = result;
        }
    }
}

/// Shared so no backend can invent its own ordering.
fn ranked_items<F>(pattern: &str, candidates: &[String], item: F) -> Vec<CompletionItem>
where
    F: Fn(usize, &str) -> CompletionItem,
{
    matcher::match_all(pattern, candidates)
        .into_iter()
        .map(|index| item(index, &candidates[index]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(index: &[&str]) -> CompletionController {
        CompletionController::with_paths(index.iter().map(|path| (*path).to_owned()).collect())
    }

    fn displayed(engine: &CompletionController) -> Vec<String> {
        engine
            .items(0, engine.item_count())
            .iter()
            .map(|item| item.display.clone())
            .collect()
    }

    #[test]
    fn an_at_token_opens_completion_anywhere_in_the_line() {
        let mut engine = engine(&["src/main.rs", "docs/"]);
        engine.sync("explain @mai", 12);

        assert!(engine.is_open());
        assert_eq!(displayed(&engine), ["src/main.rs"]);
    }

    #[test]
    fn plain_text_closes_the_popup() {
        let mut engine = engine(&["src/main.rs"]);
        engine.sync("@src", 4);
        engine.sync("hello", 5);

        assert!(!engine.is_open());
    }

    #[test]
    fn selection_stays_inside_the_items() {
        let mut engine = engine(&["a.txt", "b.txt"]);
        engine.sync("@", 1);
        assert_eq!(engine.item_count(), 2);

        engine.move_selection(50);
        assert_eq!(engine.selected(), 1);

        engine.move_selection(-50);
        assert_eq!(engine.selected(), 0);
    }

    #[test]
    fn accepting_reports_the_range_it_overwrites() {
        let mut engine = engine(&["src/main.rs"]);
        engine.sync("explain @mai", 12);

        let (item, range) = engine.accept().unwrap();
        assert_eq!(item.replacement, "src/main.rs");
        // The `@` at byte 8 is outside the range, so it survives.
        assert_eq!(range, 9..12);
        assert!(!engine.is_open());
    }

    #[test]
    fn accepting_a_directory_keeps_the_popup_open() {
        let mut engine = engine(&["crates/"]);
        engine.sync("@crat", 5);

        let (item, _) = engine.accept().unwrap();
        assert!(item.stay_open);
        assert!(engine.is_open());
    }

    #[test]
    fn accepting_nothing_when_no_candidate_matched() {
        let mut engine = engine(&["src/main.rs"]);
        engine.sync("@zzz", 4);

        assert_eq!(engine.item_count(), 0);
        assert!(engine.accept().is_none());
    }

    #[test]
    fn items_are_bounded_by_the_window_asked_for() {
        let mut engine = engine(&["a.txt", "b.txt", "c.txt"]);
        engine.sync("@", 1);

        assert_eq!(engine.items(0, 2).len(), 2);
        assert_eq!(engine.items(2, 5).len(), 1);
        assert_eq!(engine.items(9, 5).len(), 0);
    }
}
