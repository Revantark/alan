//! Path-completion backend for the v2 [`Completer`].
//!
//! Typing `@` offers files and folders from one in-memory index of the
//! workspace, in which directories carry a trailing `/`. The scan runs on
//! the blocking thread pool as a spawned task; its result is written back to
//! [`PathsContext`] through `cx.update`, so the index is served stale rather
//! than waited on. This backend is stateless — it reads its data from the
//! context, never from `self`.

use super::{
    Accept, CompletionBackendV2, CompletionContext, CompletionItem, CompletionRequest,
    CompletionResult, PathsContext, ranked_items,
};

/// The path-completion backend. Stateless: it ranks the context's paths
/// against the request pattern.
///
/// Reuses [`scan_dir`](super::paths::scan_dir) and the same ranking, skipped
/// directories, maximum index size, maximum depth, and directory ordering as
/// the legacy [`Paths`](super::Paths) backend.
pub struct PathCompleterBackend;

impl CompletionBackendV2 for PathCompleterBackend {
    fn trigger(&self) -> char {
        '@'
    }

    fn complete(
        &self,
        request: &CompletionRequest,
        context: &dyn CompletionContext,
    ) -> Option<CompletionResult> {
        // Only ever see our own context; a mismatch is a programming error
        // that degrades to "no completion" rather than panicking.
        let context = context.as_any().downcast_ref::<PathsContext>()?;

        Some(CompletionResult {
            range: request.range.clone(),
            status: context.status.clone(),
            items: ranked_items(&request.pattern, &context.paths, |path| CompletionItem {
                display: path.to_owned(),
                replacement: path.to_owned(),
                accept: Accept::Insert,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::completion::PathsContext;

    fn request(pattern: &str) -> CompletionRequest {
        CompletionRequest {
            trigger: '@',
            pattern: pattern.to_owned(),
            range: 1..1 + pattern.len(),
            row: 0,
        }
    }

    #[test]
    fn a_lone_trigger_lists_paths_from_the_context() {
        let backend = PathCompleterBackend;
        let context = PathsContext {
            paths: vec!["src/main.rs".to_owned(), "docs/".to_owned()],
            status: crate::core::completion::CompletionStatus::Ready,
        };

        let result = backend.complete(&request(""), &context).unwrap();

        assert_eq!(
            result.status,
            crate::core::completion::CompletionStatus::Ready
        );
        // An empty pattern keeps the context's own order (the matcher does
        // not reorder when nothing was typed).
        assert_eq!(
            result.items.iter().map(|i| &i.display).collect::<Vec<_>>(),
            ["src/main.rs", "docs/"]
        );
        assert_eq!(result.items[0].accept, Accept::Insert);
    }

    #[test]
    fn a_pattern_narrows_the_context_paths() {
        let backend = PathCompleterBackend;
        let context = PathsContext {
            paths: vec!["src/main.rs".to_owned(), "src/lib.rs".to_owned()],
            status: crate::core::completion::CompletionStatus::Ready,
        };

        let result = backend.complete(&request("mai"), &context).unwrap();

        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].replacement, "src/main.rs");
    }

    #[test]
    fn a_mismatched_context_yields_no_completion() {
        use crate::core::completion::CommandsContext;
        let backend = PathCompleterBackend;
        let context = CommandsContext::default();

        assert!(backend.complete(&request(""), &context).is_none());
    }
}
