//! Slash-command completion backend for the v2 [`Completer`].
//!
//! Candidates come from [`SlashCommand`] itself, so the popup cannot offer a
//! command that does not exist. The command list is immutable config, so the
//! backend reads it from its context and never mutates anything.

use super::{
    Accept, CommandsContext, CompletionBackendV2, CompletionContext, CompletionItem,
    CompletionRequest, CompletionResult, ranked_items,
};

/// The slash-command backend. Stateless: it ranks the context's commands
/// against the request pattern.
///
/// Reuses the same display, replacement, and `Accept::Complete` rules as the
/// legacy [`Commands`](super::Commands) backend.
pub struct CommandCompleterBackend;

impl CompletionBackendV2 for CommandCompleterBackend {
    fn trigger(&self) -> char {
        '/'
    }

    fn complete(
        &self,
        request: &CompletionRequest,
        context: &dyn CompletionContext,
    ) -> Option<CompletionResult> {
        // Only ever see our own context; a mismatch is a programming error
        // that degrades to "no completion" rather than panicking.
        let context = context.as_any().downcast_ref::<CommandsContext>()?;
        let commands = &context.commands;

        // A command is the whole input, so it can only open the buffer. The
        // range starts after the one-byte trigger, so 1 is column 0.
        if request.row != 0 || request.range.start != 1 {
            return None;
        }

        Some(CompletionResult {
            range: request.range.clone(),
            status: super::CompletionStatus::Ready,
            items: ranked_items(&request.pattern, commands, |command| CompletionItem {
                display: format!("{} — {}", command.name(), command.description()),
                replacement: command.as_ref().to_owned(),
                accept: Accept::Complete,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use strum::IntoEnumIterator;

    use super::*;
    use crate::core::{SlashCommand, completion::CommandsContext};

    fn request(pattern: &str) -> CompletionRequest {
        CompletionRequest {
            trigger: '/',
            pattern: pattern.to_owned(),
            range: 1..1 + pattern.len(),
            row: 0,
        }
    }

    #[test]
    fn a_lone_slash_lists_every_command() {
        let backend = CommandCompleterBackend;
        let context = CommandsContext {
            commands: SlashCommand::iter().collect(),
        };

        let result = backend.complete(&request(""), &context).unwrap();

        assert_eq!(result.items.len(), SlashCommand::iter().count());
    }

    #[test]
    fn a_pattern_narrows_to_the_matching_commands() {
        let backend = CommandCompleterBackend;
        let context = CommandsContext {
            commands: SlashCommand::iter().collect(),
        };

        let result = backend.complete(&request("he"), &context).unwrap();

        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].replacement, "help");
    }

    #[test]
    fn an_item_is_displayed_with_its_description() {
        let backend = CommandCompleterBackend;
        let context = CommandsContext {
            commands: SlashCommand::iter().collect(),
        };

        let result = backend.complete(&request("help"), &context).unwrap();

        assert_eq!(
            result.items[0].display,
            format!("/help — {}", SlashCommand::Help.description())
        );
    }

    /// Everywhere but the first column a `/` is a path separator.
    #[test]
    fn a_slash_inside_the_line_is_not_a_command() {
        let backend = CommandCompleterBackend;
        let context = CommandsContext {
            commands: SlashCommand::iter().collect(),
        };

        let mut request = request("usr");
        request.range = 8..12;
        request.row = 0;

        assert!(backend.complete(&request, &context).is_none());
    }

    /// A command is the whole input, so a `/` opening a continuation line is
    /// prose.
    #[test]
    fn a_slash_opening_a_later_line_is_not_a_command() {
        let backend = CommandCompleterBackend;
        let context = CommandsContext {
            commands: SlashCommand::iter().collect(),
        };

        let mut request = request("he");
        request.row = 1;

        assert!(backend.complete(&request, &context).is_none());
    }

    /// The trigger survives the replacement, so the item carries the bare name.
    #[test]
    fn accepting_replaces_the_name_and_keeps_the_slash() {
        let backend = CommandCompleterBackend;
        let context = CommandsContext {
            commands: SlashCommand::iter().collect(),
        };

        let result = backend.complete(&request("he"), &context).unwrap();

        assert_eq!(result.items[0].replacement, "help");
        assert_eq!(result.range, 1..3);
        assert_eq!(result.items[0].accept, Accept::Complete);
    }

    #[test]
    fn a_mismatched_context_yields_no_completion() {
        use crate::core::completion::PathsContext;
        let backend = CommandCompleterBackend;
        let context = PathsContext {
            paths: Vec::new(),
            status: crate::core::CompletionStatus::Ready,
        };

        assert!(backend.complete(&request(""), &context).is_none());
    }
}
