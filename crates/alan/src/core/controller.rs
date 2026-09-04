//! UI-independent application coordinator.

use super::action::{Command, ImageAttachment};
use super::chat::{ChatController, Entry};
use super::command::SlashCommand;
use super::completion::{Commands, CompletionController, Paths};
use agent::Agent;
use llm::Usage;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Poll {
    Idle,
    Changed,
    Finished,
    Error,
    Aborted,
}

impl Poll {
    pub(crate) fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::Error, _) | (_, Self::Error) => Self::Error,
            (Self::Aborted, _) | (_, Self::Aborted) => Self::Aborted,
            (Self::Finished, _) | (_, Self::Finished) => Self::Finished,
            (Self::Changed, _) | (_, Self::Changed) => Self::Changed,
            _ => Self::Idle,
        }
    }
}

/// What the prompt is doing, and so what Enter does to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    /// Streaming a response.
    Thinking,
    /// Offering completions, which take Enter before the editor sees it.
    Suggesting,
    /// Waiting on a prompt.
    Idle,
}

/// Outcome of [`Controller::handle`]: whether the app should quit and
/// whether the root should open the login overlay entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandOutcome {
    pub quit: bool,
    pub open_login: bool,
}

impl CommandOutcome {
    pub const NONE: Self = Self {
        quit: false,
        open_login: false,
    };
    pub const OPEN_LOGIN: Self = Self {
        quit: false,
        open_login: true,
    };

    fn quit(quit: bool) -> Self {
        Self {
            quit,
            open_login: false,
        }
    }
}

/// Coordinates feature controllers. It does not render or handle terminal types.
pub struct Controller {
    chat: ChatController,
    completion: CompletionController,
}

impl Controller {
    pub fn new(agent: Agent) -> Self {
        Self {
            chat: ChatController::new(agent),
            completion: CompletionController::new(vec![
                Box::new(Paths::default()),
                Box::new(Commands::default()),
            ]),
        }
    }

    pub fn chat(&self) -> &[Entry] {
        self.chat.entries()
    }

    pub fn chat_revision(&self) -> u64 {
        self.chat.revision()
    }

    /// Ordered by precedence: a streaming response outranks an open popup.
    pub fn activity(&self) -> Activity {
        if self.chat.is_busy() {
            Activity::Thinking
        } else if self.completion.item_count() > 0 {
            Activity::Suggesting
        } else {
            Activity::Idle
        }
    }

    pub fn mode(&self) -> agent::Mode {
        self.chat.mode()
    }

    pub fn usage(&self) -> Usage {
        self.chat.usage()
    }

    pub async fn restore_session_history(&mut self) {
        self.chat.restore_session_history().await;
    }

    pub fn agent(&self) -> Arc<Agent> {
        self.chat.agent()
    }

    pub fn completion(&self) -> &CompletionController {
        &self.completion
    }

    pub fn completion_mut(&mut self) -> &mut CompletionController {
        &mut self.completion
    }

    pub fn poll(&mut self) -> Poll {
        self.chat.poll().combine(self.completion.poll())
    }

    pub fn handle(&mut self, command: Command) -> CommandOutcome {
        match command {
            Command::Interrupt => CommandOutcome::quit(if self.chat.is_busy() {
                self.chat.abort();
                false
            } else {
                true
            }),
            // Esc with no login overlay left to close; overlays cancel
            // themselves via `cleanup`, so this is inert.
            Command::Cancel => CommandOutcome::NONE,
            Command::Submit { text, images } => {
                if self
                    .submit(text, images)
                    .is_some_and(|command| matches!(command, Command::OpenLogin))
                {
                    CommandOutcome::OPEN_LOGIN
                } else {
                    CommandOutcome::NONE
                }
            }
            Command::TogglePlanMode => {
                self.chat.toggle_mode();
                CommandOutcome::NONE
            }
            // Produced by `Controller::submit`, interpreted by `AlanRoot`.
            // Reaching `handle` directly is a stale no-op.
            Command::OpenLogin => CommandOutcome::NONE,
        }
    }

    pub fn submit(&mut self, text: String, images: Vec<ImageAttachment>) -> Option<Command> {
        // Not trimmed: a leading space means this is a prompt.
        if let Some(command) = SlashCommand::parse(&text) {
            match command {
                SlashCommand::Login => return Some(Command::OpenLogin),
                SlashCommand::Plan => self.chat.set_mode(agent::Mode::Plan),
                SlashCommand::Review => self.chat.set_mode(agent::Mode::Review),
                SlashCommand::Normal => self.chat.set_mode(agent::Mode::Normal),
                SlashCommand::Help => self.chat.push_info(SlashCommand::help()),
            }
            return None;
        }

        let text = text.trim();
        if (text.is_empty() && images.is_empty()) || self.chat.is_busy() {
            return None;
        }

        self.chat.submit(text.to_owned(), images);
        None
    }

    pub fn push_info(&mut self, text: impl Into<String>) {
        self.chat.push_info(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use llm::{LlmApi, LlmError, LlmEvent, StopReason};
    use providers::{
        ApiId, ModelCapabilities, ModelInfo, OpenRouterProvider, Provider, ProviderId,
    };
    use std::sync::Arc;
    use std::time::Duration;

    struct FakeApi;

    #[async_trait]
    impl LlmApi for FakeApi {
        async fn stream(&self, request: llm::LlmRequest<'_>) -> Result<llm::LlmStream, LlmError> {
            Ok(Box::pin(futures_util::stream::iter([
                Ok(LlmEvent::TextDelta { text: "hel".into() }),
                Ok(LlmEvent::TextDelta { text: "lo".into() }),
                Ok(LlmEvent::Done {
                    stop_reason: StopReason::Stop,
                    usage: None,
                    model: Some(request.model_id.to_owned()),
                }),
            ])))
        }
    }

    fn make_controller() -> Controller {
        let info = ModelInfo {
            provider: ProviderId::new("openrouter"),
            id: "test".into(),
            name: "Test".into(),
            api: ApiId::ChatCompletions,
            capabilities: ModelCapabilities::default(),
            pricing: None,
        };
        let model = OpenRouterProvider::builder("key")
            .with_models([info])
            .with_api(Arc::new(FakeApi))
            .build()
            .unwrap()
            .bind("test")
            .unwrap();
        Controller::new(Agent::builder(model).build().unwrap())
    }

    #[tokio::test]
    async fn submit_streams_incremental_text() {
        let mut controller = make_controller();
        controller.submit("hi".into(), vec![]);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(controller.poll(), Poll::Finished);
        assert_eq!(controller.chat().len(), 2);
        assert!(matches!(&controller.chat()[0], Entry::Prompt(text) if text == "hi"));
        assert!(matches!(&controller.chat()[1], Entry::Response(text) if text == "hello"));
        assert_eq!(controller.activity(), Activity::Idle);
    }

    struct ReasoningFakeApi;

    #[async_trait]
    impl LlmApi for ReasoningFakeApi {
        async fn stream(&self, request: llm::LlmRequest<'_>) -> Result<llm::LlmStream, LlmError> {
            Ok(Box::pin(futures_util::stream::iter([
                Ok(LlmEvent::ReasoningDelta {
                    reasoning: "thinking...".into(),
                    details: vec![],
                }),
                Ok(LlmEvent::TextDelta {
                    text: "answer".into(),
                }),
                Ok(LlmEvent::Done {
                    stop_reason: StopReason::Stop,
                    usage: None,
                    model: Some(request.model_id.to_owned()),
                }),
            ])))
        }
    }

    #[tokio::test]
    async fn submit_streams_reasoning_and_response() {
        let info = ModelInfo {
            provider: ProviderId::new("openrouter"),
            id: "test".into(),
            name: "Test".into(),
            api: ApiId::ChatCompletions,
            capabilities: ModelCapabilities::default(),
            pricing: None,
        };
        let model = OpenRouterProvider::builder("key")
            .with_models([info])
            .with_api(Arc::new(ReasoningFakeApi))
            .build()
            .unwrap()
            .bind("test")
            .unwrap();
        let mut controller = Controller::new(Agent::builder(model).build().unwrap());

        controller.submit("hi".into(), vec![]);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(controller.poll(), Poll::Finished);
        assert_eq!(controller.chat().len(), 3);
        assert!(matches!(&controller.chat()[0], Entry::Prompt(text) if text == "hi"));
        assert!(matches!(&controller.chat()[1], Entry::Reasoning(text) if text == "thinking..."));
        assert!(matches!(&controller.chat()[2], Entry::Response(text) if text == "answer"));
        assert_eq!(controller.activity(), Activity::Idle);
    }

    #[test]
    fn submit_ignores_empty_and_busy() {
        let mut controller = make_controller();
        controller.submit("   ".into(), vec![]);
        assert!(controller.chat().is_empty());
        assert_eq!(controller.activity(), Activity::Idle);
    }

    #[test]
    fn help_writes_to_the_transcript_without_reaching_the_agent() {
        let mut controller = make_controller();
        controller.submit("/help".into(), vec![]);

        assert_eq!(controller.chat().len(), 1);
        assert!(matches!(
            &controller.chat()[0],
            Entry::Info(text) if text.contains("/help")
        ));
        // A command must never start an agent turn.
        assert_eq!(controller.activity(), Activity::Idle);
    }

    /// Streamed text merges into a trailing `Response`, and commands run
    /// while the agent is streaming. `Info` must not be merged into.
    #[tokio::test]
    async fn info_is_not_absorbed_by_a_streaming_response() {
        let mut controller = make_controller();
        controller.submit("hi".into(), vec![]);
        controller.submit("/help".into(), vec![]);
        tokio::time::sleep(Duration::from_millis(50)).await;
        controller.poll();

        assert_eq!(controller.chat().len(), 3);
        assert!(matches!(&controller.chat()[0], Entry::Prompt(text) if text == "hi"));
        assert!(matches!(&controller.chat()[1], Entry::Info(text) if text.contains("/help")));
        assert!(matches!(&controller.chat()[2], Entry::Response(text) if text == "hello"));
    }

    #[test]
    fn plan_command_toggles_the_same_state_as_the_shortcut() {
        let mut controller = make_controller();
        assert_eq!(controller.mode(), agent::Mode::Normal);

        controller.submit("/plan".into(), vec![]);
        assert_eq!(controller.mode(), agent::Mode::Plan);

        controller.submit("/normal".into(), vec![]);
        assert_eq!(controller.mode(), agent::Mode::Normal);
    }

    #[test]
    fn review_command_turns_on_review_mode_and_rearms_on_reentry() {
        let mut controller = make_controller();
        controller.submit("/review".into(), vec![]);
        assert_eq!(controller.mode(), agent::Mode::Review);

        controller.submit("/normal".into(), vec![]);
        assert_eq!(controller.mode(), agent::Mode::Normal);

        controller.submit("/review".into(), vec![]);
        assert_eq!(controller.mode(), agent::Mode::Review);
    }

    #[test]
    fn shift_tab_cycles_normal_plan_review_normal() {
        let mut controller = make_controller();
        assert_eq!(controller.mode(), agent::Mode::Normal);

        controller.handle(Command::TogglePlanMode);
        assert_eq!(controller.mode(), agent::Mode::Plan);

        controller.handle(Command::TogglePlanMode);
        assert_eq!(controller.mode(), agent::Mode::Review);

        controller.handle(Command::TogglePlanMode);
        assert_eq!(controller.mode(), agent::Mode::Normal);
    }

    /// Submitting a prompt spawns an agent task, so this needs a runtime.
    #[tokio::test]
    async fn unknown_slash_text_is_sent_as_a_prompt() {
        let mut controller = make_controller();
        controller.submit("/logn".into(), vec![]);

        assert!(matches!(&controller.chat()[0], Entry::Prompt(text) if text == "/logn"));
    }
}
