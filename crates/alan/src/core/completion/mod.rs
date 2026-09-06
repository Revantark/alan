//! Completion for the prompt editor.
//!
//! The character a token starts with picks the backend, so no backend parses
//! the line itself. Ranking is not their concern either: [`matcher`] orders
//! every backend the same way.

mod command_backend;
mod matcher;
mod path_backend;
pub(crate) mod scan;
pub(crate) mod token;

use crate::core::SlashCommand;
pub use command_backend::CommandCompleterBackend;
pub use path_backend::PathCompleterBackend;
use std::collections::HashMap;
use std::ops::Range;

/// How many matches one keystroke turns into popup items. Unlike the index
/// this costs nothing to miss: narrowing the pattern surfaces the rest.
const MAX_SUGGESTIONS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionRequest {
    /// First character of the token: what selects the backend.
    pub trigger: char,
    /// What was typed after the trigger character.
    pub pattern: String,
    /// Bytes of the line the pattern occupies, which accepting overwrites.
    pub range: Range<usize>,
    /// Line of the buffer the token sits on, which is what tells a backend
    pub row: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    pub display: String,
    pub replacement: String,
    pub accept: Accept,
}

/// What accepting an item leaves the input in. Set by the backend that offered the item
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accept {
    Insert,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionStatus {
    Loading,
    Ready,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionResult {
    /// Bytes of the line that accepting overwrites.
    pub range: Range<usize>,
    pub status: CompletionStatus,
    /// Ranked best first.
    pub items: Vec<CompletionItem>,
}

/// A typed, erasable context bag. Each backend defines its own context type
/// and downcasts to it, so a backend only ever sees its own data — there is
/// no shared field to ignore. A mismatch returns `None` rather than panicking.
pub trait CompletionContext: std::any::Any + Send + Sync {
    /// Erased `&self` for downcasting to the backend's own context type.
    fn as_any(&self) -> &dyn std::any::Any;
}

impl<T: std::any::Any + Send + Sync> CompletionContext for T {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Data the path backend needs. Written by the spawned scan task via
/// `cx.update` on the completer entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathsContext {
    pub paths: Vec<String>,
    pub status: CompletionStatus,
}

/// Data the command backend needs. Immutable config; nothing writes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandsContext {
    pub commands: Vec<SlashCommand>,
}

/// A context-bearing backend for the v2 [`Completer`]. Each backend owns its
/// own data via a typed context that a spawned task writes through
/// `cx.update`; the backend downcasts to its own type and never ignores a
/// shared field.
///
/// This is a separate trait from [`CompletionBackend`] so the legacy
/// `Paths`/`Commands` backends and their callers are untouched.
pub trait CompletionBackendV2 {
    /// The character a token must start with for this backend to answer.
    fn trigger(&self) -> char;

    /// The completion offered at the cursor. `None` when the trigger matched
    /// but this backend still does not apply, which closes the popup rather
    /// than showing an empty one. The backend downcasts `context` to its own
    /// typed context; a mismatch returns `None` rather than panicking.
    fn complete(
        &self,
        request: &CompletionRequest,
        context: &dyn CompletionContext,
    ) -> Option<CompletionResult>;
}

/// A stateless completer: it dispatches a request to the backend filed under
/// its trigger and returns the result. It never scans, never starts work, and
/// never owns selection — each backend owns its own data via a typed context
/// that a spawned task writes through `cx.update`.
///
/// `complete` is `&self` and pure — it takes no lock and can be called from a
/// `cx.read` closure.
pub struct Completer {
    backends: HashMap<char, BackendEntry>,
}

struct BackendEntry {
    backend: Box<dyn CompletionBackendV2>,
    context: Box<dyn CompletionContext>,
}

impl Completer {
    /// An empty completer with no backends. Add them with [`Self::with_backend`].
    pub fn new() -> Self {
        Self {
            backends: HashMap::new(),
        }
    }

    /// Register a backend with its typed context. Keyed by the backend's own
    /// trigger, so the key can never disagree with the backend filed under it.
    ///
    /// # Panics
    ///
    /// If two backends share a trigger. The list is written in source, so a
    /// clash is a programming error with no sensible recovery: dropping one
    /// silently would make completion mysteriously dead for that character.
    pub fn with_backend(
        mut self,
        backend: Box<dyn CompletionBackendV2>,
        context: Box<dyn CompletionContext>,
    ) -> Self {
        let trigger = backend.trigger();
        assert!(
            !self.backends.contains_key(&trigger),
            "two completion backends claim the trigger {trigger:?}"
        );
        self.backends
            .insert(trigger, BackendEntry { backend, context });
        self
    }

    /// Dispatch `request` to the backend filed under its trigger. `None` means
    /// no backend answers for this trigger, which closes the popup.
    pub fn complete(&self, request: CompletionRequest) -> Option<CompletionResult> {
        let entry = self.backends.get(&request.trigger)?;
        entry.backend.complete(&request, entry.context.as_ref())
    }

    /// Replace the context filed under `trigger`. Called by a spawned task via
    /// `cx.update`, never from `complete`.
    pub(crate) fn set_context(&mut self, trigger: char, context: Box<dyn CompletionContext>) {
        let Some(entry) = self.backends.get_mut(&trigger) else {
            return;
        };
        entry.context = context;
    }
}

impl Default for Completer {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared so no backend can invent its own ordering.
fn ranked_items<C, F>(pattern: &str, candidates: &[C], item: F) -> Vec<CompletionItem>
where
    C: AsRef<str>,
    F: Fn(&C) -> CompletionItem,
{
    matcher::rank_all(pattern, candidates)
        .into_iter()
        .take(MAX_SUGGESTIONS)
        .map(|index| item(&candidates[index]))
        .collect()
}
