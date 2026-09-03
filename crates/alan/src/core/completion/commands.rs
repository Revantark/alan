//! Slash-command completion.
//!
//! Candidates come from [`SlashCommand`] itself, so the popup cannot offer a
//! command that does not exist.

use super::{
    Accept, CompletionBackend, CompletionItem, CompletionRequest, CompletionResult,
    CompletionStatus, ranked_items,
};
use crate::core::SlashCommand;
use strum::IntoEnumIterator;

pub struct Commands {
    commands: Vec<SlashCommand>,
}

impl Default for Commands {
    fn default() -> Self {
        Self {
            commands: SlashCommand::iter().collect(),
        }
    }
}

impl CompletionBackend for Commands {
    fn trigger(&self) -> char {
        '/'
    }

    fn complete(&self, request: &CompletionRequest) -> Option<CompletionResult> {
        // A command is the whole input, so it can only open the buffer. The
        // range starts after the one-byte trigger, so 1 is column 0.
        if request.row != 0 || request.range.start != 1 {
            return None;
        }

        Some(CompletionResult {
            range: request.range.clone(),
            status: CompletionStatus::Ready,
            items: ranked_items(&request.pattern, &self.commands, |command| CompletionItem {
                display: format!("{} — {}", command.name(), command.description()),
                replacement: command.as_ref().to_owned(),
                accept: Accept::Complete,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::completion::CompletionController;

    fn engine() -> CompletionController {
        CompletionController::new(vec![Box::new(Commands::default())])
    }

    #[test]
    fn a_lone_slash_lists_every_command() {
        let mut engine = engine();
        engine.sync("/", 1, 0);

        assert!(engine.is_open());
        assert_eq!(engine.item_count(), SlashCommand::iter().count());
    }

    #[test]
    fn a_pattern_narrows_to_the_matching_commands() {
        let mut engine = engine();
        engine.sync("/he", 3, 0);

        let items = engine.items(0, engine.item_count());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].replacement, "help");
    }

    /// Guards the name-to-variant lookup in [`describe`], without which the
    /// item would fall back to a bare `/help`.
    #[test]
    fn an_item_is_displayed_with_its_description() {
        let mut engine = engine();
        engine.sync("/help", 5, 0);

        assert_eq!(
            engine.items(0, 1)[0].display,
            format!("/help — {}", SlashCommand::Help.description())
        );
    }

    /// Everywhere but the first column a `/` is a path separator.
    #[test]
    fn a_slash_inside_the_line_is_not_a_command() {
        let mut engine = engine();
        engine.sync("explain /usr", 12, 0);

        assert!(!engine.is_open());
    }

    /// A command is the whole input, so a `/` opening a continuation line is
    /// prose. Offering one there would submit the half-written prompt around it.
    #[test]
    fn a_slash_opening_a_later_line_is_not_a_command() {
        let mut engine = engine();
        engine.sync("/he", 3, 1);

        assert!(!engine.is_open());
    }

    /// The trigger survives the replacement, so the item carries the bare name.
    #[test]
    fn accepting_replaces_the_name_and_keeps_the_slash() {
        let mut engine = engine();
        engine.sync("/he", 3, 0);

        let (item, range) = engine.accept().unwrap();
        assert_eq!(item.replacement, "help");
        assert_eq!(range, 1..3);
        assert_eq!(
            item.accept,
            Accept::Complete,
            "a command is the whole input"
        );
        assert!(!engine.is_open());
    }
}
