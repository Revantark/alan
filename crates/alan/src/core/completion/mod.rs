//! Completion for the prompt editor.
//!
//! The character a token starts with picks the backend, so no backend parses
//! the line itself. Ranking is not their concern either: [`matcher`] orders
//! every backend the same way.

mod matcher;
mod paths;
mod token;

use super::Poll;
pub use paths::Paths;
use std::collections::HashMap;
use std::ops::Range;

/// How many matches one keystroke turns into popup items. Unlike the index
/// this costs nothing to miss: narrowing the pattern surfaces the rest.
const MAX_SUGGESTIONS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionRequest {
    /// What was typed after the trigger character.
    pub pattern: String,
    /// Bytes of the line the pattern occupies, which accepting overwrites.
    pub range: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    pub display: String,
    /// Text substituted for [`CompletionResult::range`].
    pub replacement: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionStatus {
    Loading,
    Ready,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionResult {
    /// Bytes of the line that accepting overwrites.
    pub range: Range<usize>,
    pub status: CompletionStatus,
    /// Ranked best first.
    pub items: Vec<CompletionItem>,
}

pub trait CompletionBackend {
    /// The character a token must start with for this backend to answer.
    fn trigger(&self) -> char;

    /// The completion offered at the cursor. `None` when the trigger matched
    /// but this backend still does not apply, which closes the popup rather
    /// than showing an empty one.
    fn complete(&self, request: &CompletionRequest) -> Option<CompletionResult>;

    /// Called when this backend becomes the active one.
    fn refresh(&mut self) {}

    fn poll(&mut self) -> Poll {
        Poll::Idle
    }
}

struct Active {
    /// Trigger of the backend that claimed the request, and so the key it is
    /// filed under.
    trigger: char,
    request: CompletionRequest,
    result: CompletionResult,
    selected: usize,
}

pub struct CompletionController {
    backends: HashMap<char, Box<dyn CompletionBackend>>,
    active: Option<Active>,
}

impl CompletionController {
    /// Keyed by each backend's own [`CompletionBackend::trigger`], so the key
    /// can never disagree with the backend filed under it.
    ///
    /// # Panics
    ///
    /// If two backends share a trigger. The list is written in source, so a
    /// clash is a programming error with no sensible recovery: dropping one
    /// silently would make completion mysteriously dead for that character.
    pub fn new(backends: Vec<Box<dyn CompletionBackend>>) -> Self {
        let mut keyed = HashMap::with_capacity(backends.len());
        for backend in backends {
            let trigger = backend.trigger();
            let clash = keyed.insert(trigger, backend).is_some();
            assert!(
                !clash,
                "two completion backends claim the trigger {trigger:?}"
            );
        }
        Self {
            backends: keyed,
            active: None,
        }
    }

    /// Re-evaluate after every editor change.
    pub fn sync(&mut self, line: &str, cursor: usize) {
        self.active = self.claim(line, cursor);
    }

    /// Any step failing means no completion applies here: no token under the
    /// cursor, no backend for its trigger, or the backend declining.
    fn claim(&mut self, line: &str, cursor: usize) -> Option<Active> {
        let token = token::at(line, cursor)?;
        // Read before the backend is borrowed mutably.
        let switching = self.active.as_ref().map(|active| active.trigger) != Some(token.trigger);
        let backend = self.backends.get_mut(&token.trigger)?;
        // Becoming active is the one moment a backend's data is worth reading.
        if switching {
            backend.refresh();
        }

        // Slicing the line happens here, once, rather than in every backend.
        let request = CompletionRequest {
            pattern: line[token.range.clone()].to_owned(),
            range: token.range,
        };

        let result = backend.complete(&request)?;
        Some(Active {
            trigger: token.trigger,
            request,
            result,
            selected: 0,
        })
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
        self.active = None;
        Some((item, range))
    }

    pub fn poll(&mut self) -> Poll {
        let poll = self
            .backends
            .values_mut()
            .fold(Poll::Idle, |poll, backend| poll.combine(backend.poll()));

        if poll == Poll::Changed {
            self.recompute();
        }
        poll
    }

    /// A backend whose data changed under an open popup answers again. The
    /// request is unchanged, so the backend that claimed it still owns it.
    fn recompute(&mut self) {
        let Some((trigger, request)) = self
            .active
            .as_ref()
            .map(|active| (active.trigger, active.request.clone()))
        else {
            return;
        };

        let Some(result) = self
            .backends
            .get(&trigger)
            .and_then(|backend| backend.complete(&request))
        else {
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
    F: Fn(&str) -> CompletionItem,
{
    matcher::rank_all(pattern, candidates)
        .into_iter()
        .take(MAX_SUGGESTIONS)
        .map(|index| item(&candidates[index]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(index: &[&str]) -> CompletionController {
        CompletionController::new(vec![Box::new(Paths::with_index(
            index.iter().map(|path| (*path).to_owned()).collect(),
        ))])
    }

    fn displayed(engine: &CompletionController) -> Vec<String> {
        engine
            .items(0, engine.item_count())
            .iter()
            .map(|item| item.display.clone())
            .collect()
    }

    /// A trigger no backend is filed under closes the popup, exactly as if
    /// there were no token at all.
    #[test]
    fn an_unclaimed_trigger_opens_nothing() {
        let mut engine = engine(&["src/main.rs"]);
        engine.sync("/help", 5);

        assert!(!engine.is_open());
    }

    #[test]
    #[should_panic(expected = "two completion backends claim the trigger")]
    fn two_backends_cannot_share_a_trigger() {
        CompletionController::new(vec![
            Box::new(Paths::with_index(Vec::new())),
            Box::new(Paths::with_index(Vec::new())),
        ]);
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

    /// A directory is a reference in its own right, so accepting one finishes.
    #[test]
    fn accepting_a_directory_closes_the_popup() {
        let mut engine = engine(&["crates/", "crates/alan/"]);
        engine.sync("@crat", 5);

        let (item, range) = engine.accept().unwrap();
        assert_eq!(item.replacement, "crates/");
        assert_eq!(range, 1..5);
        assert!(!engine.is_open());
    }

    /// Typing past the directory reopens the popup against the deeper paths.
    #[test]
    fn typing_past_a_directory_reopens_the_popup() {
        let mut engine = engine(&["crates/", "crates/alan/main.rs"]);
        engine.sync("@crates/", 8);
        engine.accept();
        assert!(!engine.is_open());

        engine.sync("@crates/m", 9);

        assert!(engine.is_open());
        assert_eq!(displayed(&engine), ["crates/alan/main.rs"]);
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
