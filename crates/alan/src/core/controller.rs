//! UI-independent application coordinator.

use super::action::Command;
use super::chat::{ChatController, Entry};
use super::command::SlashCommand;
use super::completion::CompletionController;
use super::login::{LoginController, LoginState};
use agent::Agent;
use llm::Usage;
use providers::{CredentialStore, ProviderRegistry};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    None,
    Login,
}

/// Coordinates feature controllers. It does not render or handle terminal types.
pub struct Controller {
    chat: ChatController,
    login: LoginController,
    completion: CompletionController,
    overlay: Overlay,
}

impl Controller {
    // TODO: drop the attribute once a non-test caller exists. Only `#[cfg(test)]`
    // code reaches this today, so the binary build reports it as dead.
    #[allow(dead_code)]
    pub fn new(agent: Agent) -> Self {
        Self::with_runtime(
            agent,
            ProviderRegistry::default(),
            Arc::new(providers::InMemoryCredentialStore::new()),
        )
    }

    pub fn with_runtime(
        agent: Agent,
        providers: ProviderRegistry,
        credentials: Arc<dyn CredentialStore>,
    ) -> Self {
        Self {
            chat: ChatController::new(agent),
            login: LoginController::new(providers, credentials),
            completion: CompletionController::new(),
            overlay: Overlay::None,
        }
    }

    pub fn chat(&self) -> &[Entry] {
        self.chat.entries()
    }

    pub fn chat_revision(&self) -> u64 {
        self.chat.revision()
    }

    pub fn is_busy(&self) -> bool {
        self.chat.is_busy()
    }

    pub fn plan_mode(&self) -> bool {
        self.chat.plan_mode()
    }

    pub fn usage(&self) -> Usage {
        self.chat.usage()
    }

    pub fn login_state(&self) -> &LoginState {
        self.login.state()
    }

    pub fn login_selection_active(&self) -> bool {
        matches!(self.login.state(), LoginState::Selecting { .. })
    }

    pub fn completion(&self) -> &CompletionController {
        &self.completion
    }

    pub fn completion_mut(&mut self) -> &mut CompletionController {
        &mut self.completion
    }

    pub fn overlay(&self) -> Overlay {
        self.overlay
    }

    pub fn poll(&mut self) -> Poll {
        self.chat
            .poll()
            .combine(self.login.poll())
            .combine(self.completion.poll())
    }

    pub fn handle(&mut self, command: Command) -> bool {
        match command {
            Command::Interrupt => self.abort_or_quit(),
            Command::Cancel => {
                if self.overlay == Overlay::Login {
                    self.close_login();
                }
                false
            }
            Command::Submit(text) => {
                self.submit(text);
                false
            }
            Command::MoveLoginSelection(delta) => {
                self.move_login_selection(delta);
                false
            }
            Command::TogglePlanMode => {
                self.chat.toggle_plan_mode();
                false
            }
        }
    }

    fn abort_or_quit(&mut self) -> bool {
        if self.overlay == Overlay::Login {
            self.close_login();
            return false;
        }
        if self.chat.is_busy() {
            self.chat.abort();
            return false;
        }
        true
    }

    pub fn submit(&mut self, text: String) {
        if self.overlay == Overlay::Login {
            self.login.submit(text);
            return;
        }

        // Not trimmed: a leading space means this is a prompt.
        if let Some(command) = SlashCommand::parse(&text) {
            match command {
                SlashCommand::Login => self.open_login(),
                SlashCommand::Plan => self.chat.toggle_plan_mode(),
                SlashCommand::Help => self.chat.push_info(SlashCommand::help()),
            }
            return;
        }

        let text = text.trim();
        if text.is_empty() || self.chat.is_busy() {
            return;
        }

        self.chat.submit(text.to_owned());
    }

    pub fn open_login(&mut self) {
        self.login.open();
        self.overlay = Overlay::Login;
    }

    pub fn move_login_selection(&mut self, delta: isize) {
        self.login.move_selection(delta);
    }

    fn close_login(&mut self) {
        self.login.cancel();
        self.overlay = Overlay::None;
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
        controller.submit("hi".into());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(controller.poll(), Poll::Finished);
        assert_eq!(controller.chat().len(), 2);
        assert!(matches!(&controller.chat()[0], Entry::Prompt(text) if text == "hi"));
        assert!(matches!(&controller.chat()[1], Entry::Response(text) if text == "hello"));
        assert!(!controller.is_busy());
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

        controller.submit("hi".into());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(controller.poll(), Poll::Finished);
        assert_eq!(controller.chat().len(), 3);
        assert!(matches!(&controller.chat()[0], Entry::Prompt(text) if text == "hi"));
        assert!(matches!(&controller.chat()[1], Entry::Reasoning(text) if text == "thinking..."));
        assert!(matches!(&controller.chat()[2], Entry::Response(text) if text == "answer"));
        assert!(!controller.is_busy());
    }

    #[test]
    fn submit_ignores_empty_and_busy() {
        let mut controller = make_controller();
        controller.submit("   ".into());
        assert!(controller.chat().is_empty());
        assert!(!controller.is_busy());
    }

    #[test]
    fn help_writes_to_the_transcript_without_reaching_the_agent() {
        let mut controller = make_controller();
        controller.submit("/help".into());

        assert_eq!(controller.chat().len(), 1);
        assert!(matches!(
            &controller.chat()[0],
            Entry::Info(text) if text.contains("/help")
        ));
        // A command must never start an agent turn.
        assert!(!controller.is_busy());
    }

    /// Streamed text merges into a trailing `Response`, and commands run
    /// while the agent is streaming. `Info` must not be merged into.
    #[tokio::test]
    async fn info_is_not_absorbed_by_a_streaming_response() {
        let mut controller = make_controller();
        controller.submit("hi".into());
        controller.submit("/help".into());
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
        assert!(!controller.plan_mode());

        controller.submit("/plan".into());
        assert!(controller.plan_mode());

        controller.submit("/plan".into());
        assert!(!controller.plan_mode());
    }

    /// Submitting a prompt spawns an agent task, so this needs a runtime.
    #[tokio::test]
    async fn unknown_slash_text_is_sent_as_a_prompt() {
        let mut controller = make_controller();
        controller.submit("/logn".into());

        assert!(matches!(&controller.chat()[0], Entry::Prompt(text) if text == "/logn"));
    }
}
