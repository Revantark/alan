//! Slash commands typed into the prompt.
//!
//! `/help` and the prompt highlight are both derived from the variants, so
//! they cannot disagree about which commands exist.

use std::str::FromStr;

use llm::ReasoningEffort;
use strum::{EnumIter, EnumString, IntoEnumIterator, IntoStaticStr};

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, EnumIter, IntoStaticStr)]
#[strum(serialize_all = "kebab-case")]
pub enum SlashCommand {
    Login,
    Models,
    New,
    SummarizeNew,
    Plan,
    // Fork this session from a checkpoint.
    Fork,
    // Specific to openrouter
    ModelProviders,
    Review,
    Normal,
    Effort,
    Help,
    // Rename the current session.
    Rename,
    // Manage local models (add, remove, edit).
    Local,
    Quit,
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

    /// Whether the command consumes arguments. Zero-argument commands run
    /// immediately on completion accept; argument-taking ones insert text
    /// so the user can keep typing. Adding a future command here makes its
    /// accept-time behavior flow through automatically.
    pub fn takes_args(self) -> bool {
        matches!(
            self,
            Self::Effort | Self::ModelProviders | Self::SummarizeNew | Self::Rename | Self::Local
        )
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
            Self::Effort => "set reasoning effort (e.g. /effort high)",
            Self::Fork => "fork this session from a checkpoint",
            Self::Help => "list the available commands",
            Self::ModelProviders => "pick a provider from openrouter for the selected model",
            Self::Rename => "rename the current session",
            Self::Local => "manage local models (add, remove, edit)",
            Self::Quit => "abort and quit",
        }
    }

    /// Parse a reasoning effort argument. `none` maps to `Some(None)`, a known
    /// level maps to `Some(Some(level))`, and anything else returns `None`.
    ///
    /// `None` is a valid argument: it disables reasoning. Use this rather than
    /// `ReasoningEffort::from_str` directly, since the enum serializes to
    /// `none` and the parser must keep that distinct from the literal string.
    pub fn parse_effort(args: &str) -> Option<ReasoningEffort> {
        let arg = args.trim().to_ascii_lowercase();
        ReasoningEffort::from_str(&arg).ok()
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

    #[test]
    fn parses_fork_command_with_and_without_argument() {
        assert_eq!(SlashCommand::parse("/fork"), Some(SlashCommand::Fork));
        assert_eq!(SlashCommand::parse("/fork now"), Some(SlashCommand::Fork));
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

    #[test]
    fn parses_effort_command() {
        assert_eq!(SlashCommand::parse("/effort"), Some(SlashCommand::Effort));
        assert_eq!(
            SlashCommand::parse("/effort high"),
            Some(SlashCommand::Effort)
        );
        assert_eq!(
            SlashCommand::parse_with_args("/effort medium"),
            Some((SlashCommand::Effort, "medium"))
        );
        assert_eq!(
            SlashCommand::parse_with_args("/effort"),
            Some((SlashCommand::Effort, ""))
        );
    }

    #[test]
    fn parse_effort_accepts_every_level_and_none() {
        assert_eq!(
            SlashCommand::parse_effort("none"),
            Some(ReasoningEffort::None)
        );
        assert_eq!(
            SlashCommand::parse_effort("minimal"),
            Some(ReasoningEffort::Minimal)
        );
        assert_eq!(
            SlashCommand::parse_effort("low"),
            Some(ReasoningEffort::Low)
        );
        assert_eq!(
            SlashCommand::parse_effort("medium"),
            Some(ReasoningEffort::Medium)
        );
        assert_eq!(
            SlashCommand::parse_effort("high"),
            Some(ReasoningEffort::High)
        );
        assert_eq!(
            SlashCommand::parse_effort("xhigh"),
            Some(ReasoningEffort::XHigh)
        );
        assert_eq!(
            SlashCommand::parse_effort("max"),
            Some(ReasoningEffort::Max)
        );
    }

    #[test]
    fn parse_effort_is_case_insensitive_and_trims_whitespace() {
        assert_eq!(
            SlashCommand::parse_effort("  High  "),
            Some(ReasoningEffort::High)
        );
        assert_eq!(
            SlashCommand::parse_effort("NONE"),
            Some(ReasoningEffort::None)
        );
    }

    #[test]
    fn parse_effort_rejects_unknown_values() {
        assert_eq!(SlashCommand::parse_effort(""), None);
        assert_eq!(SlashCommand::parse_effort("ultra"), None);
        assert_eq!(SlashCommand::parse_effort("high low"), None);
    }

    #[test]
    fn takes_args_classifies_commands() {
        // Zero-argument commands.
        assert!(!SlashCommand::Login.takes_args());
        assert!(!SlashCommand::Models.takes_args());
        assert!(!SlashCommand::New.takes_args());
        assert!(!SlashCommand::Fork.takes_args());
        assert!(!SlashCommand::Plan.takes_args());
        assert!(!SlashCommand::Review.takes_args());
        assert!(!SlashCommand::Normal.takes_args());
        assert!(!SlashCommand::Help.takes_args());
        // Argument-taking commands.
        assert!(SlashCommand::Local.takes_args());
        assert!(SlashCommand::Effort.takes_args());
        assert!(SlashCommand::ModelProviders.takes_args());
        assert!(SlashCommand::SummarizeNew.takes_args());
        assert!(SlashCommand::Rename.takes_args());
    }

    #[test]
    fn parses_local_command() {
        assert_eq!(SlashCommand::parse("/local"), Some(SlashCommand::Local));
    }

    #[test]
    fn parses_local_subcommands() {
        assert_eq!(
            SlashCommand::parse_with_args("/local add"),
            Some((SlashCommand::Local, "add"))
        );
        assert_eq!(
            SlashCommand::parse_with_args("/local remove"),
            Some((SlashCommand::Local, "remove"))
        );
        assert_eq!(
            SlashCommand::parse_with_args("/local edit"),
            Some((SlashCommand::Local, "edit"))
        );
    }

    #[test]
    fn parses_local_with_empty_args() {
        assert_eq!(
            SlashCommand::parse_with_args("/local"),
            Some((SlashCommand::Local, ""))
        );
    }
}
