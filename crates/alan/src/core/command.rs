//! Slash commands typed into the prompt.
//!
//! `/help` and the prompt highlight are both derived from the variants, so
//! they cannot disagree about which commands exist.

use strum::{EnumIter, EnumString, IntoEnumIterator, IntoStaticStr};

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, EnumIter, IntoStaticStr)]
#[strum(serialize_all = "kebab-case")]
pub enum SlashCommand {
    Login,
    Models,
    New,
    SummarizeNew,
    Plan,
    Review,
    Normal,
    Help,
}

impl SlashCommand {
    /// A command is the whole input: a single line starting with `/`.
    /// Anything else is a prompt. Arguments are accepted and ignored.
    pub fn parse(input: &str) -> Option<Self> {
        let rest = input.strip_prefix('/')?;
        // Only spaces and tabs separate arguments; any other whitespace breaks
        // the line, and text past it would be discarded without saying so.
        if rest
            .chars()
            .any(|c| c.is_whitespace() && !matches!(c, ' ' | '\t'))
        {
            return None;
        }
        rest.split_whitespace().next()?.parse().ok()
    }

    /// Like [`parse`](Self::parse), but keeps the text after the command.
    pub fn parse_with_args(input: &str) -> Option<(Self, &str)> {
        let rest = input.strip_prefix('/')?;
        if rest
            .chars()
            .any(|c| c.is_whitespace() && !matches!(c, ' ' | '\t'))
        {
            return None;
        }
        let trimmed = rest.trim_end_matches([' ', '\t']);
        let (name, args) = match trimmed.split_once([' ', '\t']) {
            Some((name, args)) => (name, args.trim_start_matches([' ', '\t'])),
            None => (trimmed, ""),
        };
        let command = name.parse().ok()?;
        Some((command, args))
    }

    pub fn name(self) -> String {
        format!("/{}", <&'static str>::from(self))
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Login => "sign in to a provider",
            Self::Models => "pick a model for this conversation",
            Self::New => "start a new session",
            Self::SummarizeNew => "summarize this session into a new one",
            Self::Plan => "turn on plan mode (also Shift+Tab)",
            Self::Review => "turn on review mode (also Shift+Tab)",
            Self::Normal => "turn off plan and review mode",
            Self::Help => "list the available commands",
        }
    }

    /// Markdown listing every command, rendered into the transcript by `/help`.
    pub fn help() -> String {
        let mut text = String::from("**Commands**\n");
        for command in Self::iter() {
            text.push_str(&format!(
                "\n- `{}` — {}",
                command.name(),
                command.description()
            ));
        }
        text
    }
}

impl AsRef<str> for SlashCommand {
    fn as_ref(&self) -> &str {
        self.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_commands_and_ignores_arguments() {
        assert_eq!(SlashCommand::parse("/login"), Some(SlashCommand::Login));
        assert_eq!(SlashCommand::parse("/help  "), Some(SlashCommand::Help));
        assert_eq!(SlashCommand::parse("/plan now"), Some(SlashCommand::Plan));
    }

    #[test]
    fn parses_models_command_with_and_without_argument() {
        assert_eq!(SlashCommand::parse("/models"), Some(SlashCommand::Models));
        assert_eq!(
            SlashCommand::parse("/models openai/gpt-4o"),
            Some(SlashCommand::Models)
        );
    }

    #[test]
    fn rejects_prompts_and_unknown_commands() {
        assert_eq!(SlashCommand::parse("hello"), None);
        assert_eq!(SlashCommand::parse("/logn"), None);
        assert_eq!(SlashCommand::parse(""), None);
        assert_eq!(SlashCommand::parse("/"), None);
    }

    /// Otherwise everything after the break is silently discarded. A bare `\r`
    /// reaches the buffer because the widget only strips a trailing one.
    #[test]
    fn a_line_break_of_any_kind_is_never_a_command() {
        assert_eq!(SlashCommand::parse("/plan\nand also do this"), None);
        assert_eq!(SlashCommand::parse("/plan\rand also do this"), None);
        assert_eq!(SlashCommand::parse("/plan\u{0b}and also do this"), None);
        assert_eq!(SlashCommand::parse("/help\n"), None);
    }

    #[test]
    fn spaces_and_tabs_still_separate_arguments() {
        assert_eq!(SlashCommand::parse("/plan now"), Some(SlashCommand::Plan));
        assert_eq!(SlashCommand::parse("/plan\tnow"), Some(SlashCommand::Plan));
    }

    /// Keeps this in step with the prompt highlight.
    #[test]
    fn leading_whitespace_is_not_a_command() {
        assert_eq!(SlashCommand::parse(" /plan"), None);
        assert_eq!(SlashCommand::parse("\t/help"), None);
    }

    #[test]
    fn parse_with_args_keeps_arguments() {
        assert_eq!(
            SlashCommand::parse_with_args("/help"),
            Some((SlashCommand::Help, ""))
        );
        assert_eq!(
            SlashCommand::parse_with_args("/plan now"),
            Some((SlashCommand::Plan, "now"))
        );
        assert_eq!(
            SlashCommand::parse_with_args("/plan"),
            Some((SlashCommand::Plan, ""))
        );
        assert_eq!(SlashCommand::parse_with_args("hello"), None);
    }

    /// The argument is everything after the command, verbatim — even when the
    /// command takes no args (e.g. `/new`). Validation, if any, is the handler's
    /// job, not the parser's; `/new foo` is accepted and the `foo` is dropped
    /// by `start_new_session`.
    #[test]
    fn parse_with_args_keeps_the_whole_tail_for_any_command() {
        assert_eq!(
            SlashCommand::parse_with_args("/new someting jf"),
            Some((SlashCommand::New, "someting jf"))
        );
        assert_eq!(
            SlashCommand::parse_with_args("/help a b c"),
            Some((SlashCommand::Help, "a b c"))
        );
    }

    /// `/summarize-new` is its own command, distinct from `/new`, and its
    /// argument is read by the handler via `parse_with_args`.
    #[test]
    fn parses_summarize_new() {
        assert_eq!(
            SlashCommand::parse("/summarize-new"),
            Some(SlashCommand::SummarizeNew)
        );
        assert_eq!(
            SlashCommand::parse_with_args("/summarize-new \"XYZ\""),
            Some((SlashCommand::SummarizeNew, "\"XYZ\""))
        );
        // Must not be confused with the unrelated `/new` command.
        assert_eq!(SlashCommand::parse("/new"), Some(SlashCommand::New));
    }
}
