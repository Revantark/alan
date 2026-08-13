//! UI-independent application coordinator.

use super::action::Command;
use super::chat::{ChatController, Entry};
use super::login::{LoginController, LoginState};
use agent::Agent;
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
    overlay: Overlay,
}

impl Controller {
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

    pub fn login_state(&self) -> &LoginState {
        self.login.state()
    }

    pub fn login_selection_active(&self) -> bool {
        matches!(self.login.state(), LoginState::Selecting { .. })
    }

    pub fn overlay(&self) -> Overlay {
        self.overlay
    }

    pub fn poll(&mut self) -> Poll {
        let outcome = self.chat.poll().combine(self.login.poll());
        outcome
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

        let text = text.trim();
        if text == "/login" {
            self.open_login();
            return;
        }
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
        Controller::new(Agent::builder(model).build())
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

    #[test]
    fn submit_ignores_empty_and_busy() {
        let mut controller = make_controller();
        controller.submit("   ".into());
        assert!(controller.chat().is_empty());
        assert!(!controller.is_busy());
    }
}
