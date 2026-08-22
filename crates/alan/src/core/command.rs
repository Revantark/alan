//! Slash commands typed into the prompt.
//!
//! `/help` and the prompt highlight are both derived from the variants, so
//! they cannot disagree about which commands exist.

use strum::{EnumIter, EnumString, IntoEnumIterator, IntoStaticStr};

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, EnumIter, IntoStaticStr)]
#[strum(serialize_all = "kebab-case")]
pub enum SlashCommand {
    Login,
    Plan,
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

    pub fn name(self) -> String {
        format!("/{}", <&'static str>::from(self))
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Login => "sign in to a provider",
            Self::Plan => "toggle plan mode (also Shift+Tab)",
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
}
