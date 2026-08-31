//! UI-independent application coordinator.

use super::action::{Command, ImageAttachment, InputMode};
use super::chat::{ChatController, Entry};
use super::command::SlashCommand;
use super::completion::{Commands, CompletionController, Paths};
use super::login::{LoginController, LoginState};
use super::settings::{self, SettingsController};
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
    Settings,
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

/// Coordinates feature controllers. It does not render or handle terminal types.
pub struct Controller {
    chat: ChatController,
    login: LoginController,
    completion: CompletionController,
    settings: SettingsController,
    providers: ProviderRegistry,
    overlay: Overlay,
}

impl Controller {
    pub fn new(
        agent: Agent,
        providers: ProviderRegistry,
        credentials: Arc<dyn CredentialStore>,
        settings: SettingsController,
    ) -> Self {
        Self {
            chat: ChatController::new(agent),
            login: LoginController::new(providers.clone(), credentials),
            completion: CompletionController::new(vec![
                Box::new(Paths::default()),
                Box::new(Commands::default()),
            ]),
            settings,
            providers,
            overlay: Overlay::None,
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

    pub async fn session_id(&self) -> Option<String> {
        self.chat.session_id().await
    }

    pub fn login_state(&self) -> &LoginState {
        self.login.state()
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

    pub fn input_mode(&self, overlay: Overlay) -> InputMode {
        let showing_list = match overlay {
            Overlay::Login => matches!(self.login.state(), LoginState::Selecting { .. }),
            Overlay::Settings => !self.settings.editing(),
            Overlay::None => false,
        };

        if showing_list {
            InputMode::List
        } else {
            InputMode::Prompt
        }
    }

    pub fn poll(&mut self) -> Poll {
        self.chat
            .poll()
            .combine(self.login.poll())
            .combine(self.completion.poll())
            .combine(self.poll_settings())
    }

    /// Skipped while streaming: swapping the model mid-response would leave a
    /// transcript half-answered by each. The next idle poll picks it up.
    fn poll_settings(&mut self) -> Poll {
        if self.chat.is_busy() {
            return Poll::Idle;
        }

        match self.settings.poll() {
            Ok(Poll::Changed) => {
                self.apply_settings();
                Poll::Changed
            }
            Ok(poll) => poll,
            Err(error) => {
                self.chat.push_error(format!("settings: {error}"));
                Poll::Error
            }
        }
    }

    fn apply_settings(&mut self) {
        let next = self.settings.current().clone();
        match settings::bind(&self.providers, &next) {
            Ok(model) => self.chat.set_model(model),
            Err(error) => self
                .chat
                .push_error(format!("settings: cannot use {}: {error}", next.model)),
        }
    }

    pub fn handle(&mut self, command: Command) -> bool {
        match command {
            Command::Interrupt => self.abort_or_quit(),
            Command::Cancel => {
                match self.overlay {
                    Overlay::Login => self.close_login(),
                    Overlay::Settings if self.settings.editing() => self.settings.cancel_edit(),
                    Overlay::Settings => self.close_settings(),
                    Overlay::None => {}
                }
                false
            }
            Command::Submit { text, images } => {
                self.submit(text, images);
                false
            }
            Command::Cycle => {
                match self.overlay {
                    Overlay::Settings => self.settings.toggle_scope(),
                    // Login hides the footer the mode is shown in.
                    Overlay::Login => {}
                    Overlay::None => self.chat.toggle_mode(),
                }
                false
            }
            Command::MoveSelection(delta) => {
                match self.overlay {
                    Overlay::Login => self.login.move_selection(delta),
                    Overlay::Settings => self.settings.move_selection(delta),
                    Overlay::None => {}
                }
                false
            }
            Command::ClearSelection => {
                if self.overlay == Overlay::Settings {
                    let outcome = self.settings.clear();
                    self.settings_action(outcome);
                }
                false
            }
        }
    }

    fn abort_or_quit(&mut self) -> bool {
        match self.overlay {
            Overlay::Login => {
                self.close_login();
                return false;
            }
            Overlay::Settings => {
                self.close_settings();
                return false;
            }
            Overlay::None => {}
        }
        if self.chat.is_busy() {
            self.chat.abort();
            return false;
        }
        true
    }

    pub fn submit(&mut self, text: String, images: Vec<ImageAttachment>) {
        if self.overlay == Overlay::Login {
            self.login.submit(text);
            return;
        }
        if self.overlay == Overlay::Settings {
            let outcome = if self.settings.editing() {
                self.settings.submit_edit(&text)
            } else {
                self.settings.activate()
            };
            self.settings_action(outcome);
            return;
        }

        // Not trimmed: a leading space means this is a prompt.
        if let Some(command) = SlashCommand::parse(&text) {
            match command {
                SlashCommand::Login => self.open_login(),
                SlashCommand::Settings => self.open_settings(),
                SlashCommand::Plan => self.chat.set_mode(agent::Mode::Plan),
                SlashCommand::Review => self.chat.set_mode(agent::Mode::Review),
                SlashCommand::Normal => self.chat.set_mode(agent::Mode::Normal),
                SlashCommand::Help => self.chat.push_info(SlashCommand::help()),
            }
            return;
        }

        let text = text.trim();
        if (text.is_empty() && images.is_empty()) || self.chat.is_busy() {
            return;
        }

        self.chat.submit(text.to_owned(), images);
    }

    fn open_settings(&mut self) {
        self.settings.open();
        self.overlay = Overlay::Settings;
    }

    fn close_settings(&mut self) {
        self.settings.close();
        self.overlay = Overlay::None;
    }

    pub fn settings(&self) -> &SettingsController {
        &self.settings
    }

    /// Take the value a just-opened overlay prompt should start from.
    pub fn take_input_seed(&mut self) -> Option<String> {
        self.settings.take_seed()
    }

    /// Report a settings action's outcome and apply whatever it changed.
    fn settings_action(&mut self, outcome: settings::Outcome) {
        match outcome {
            Ok(true) => self.apply_settings(),
            Ok(false) => {}
            Err(reason) => self.chat.push_error(format!("settings: {reason}")),
        }
    }

    fn open_login(&mut self) {
        self.login.open();
        self.overlay = Overlay::Login;
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
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    /// Owns the settings directory, so a test that writes a setting cannot
    /// reach outside its scratch space.
    struct TestController {
        controller: Controller,
        settings_dir: PathBuf,
    }

    /// So a test leaves the filesystem as it found it, panic or not.
    impl Drop for TestController {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.settings_dir);
        }
    }

    impl std::ops::Deref for TestController {
        type Target = Controller;

        fn deref(&self) -> &Controller {
            &self.controller
        }
    }

    impl std::ops::DerefMut for TestController {
        fn deref_mut(&mut self) -> &mut Controller {
            &mut self.controller
        }
    }

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

    /// A controller with no provider, no credentials, and default settings.
    fn controller_with(api: Arc<dyn LlmApi>) -> TestController {
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
            .with_api(api)
            .build()
            .unwrap()
            .bind("test")
            .unwrap();
        // Counted: several tests build a controller, and two sharing a name
        // would delete each other's directory.
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let settings_dir = std::env::temp_dir().join(format!(
            "alan-controller-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&settings_dir);
        std::fs::create_dir_all(&settings_dir).expect("settings dir");
        TestController {
            controller: Controller::new(
                Agent::builder(model).build().unwrap(),
                ProviderRegistry::default(),
                Arc::new(providers::InMemoryCredentialStore::new()),
                SettingsController::new(&settings_dir, &settings_dir).expect("defaults"),
            ),
            settings_dir,
        }
    }

    fn make_controller() -> TestController {
        controller_with(Arc::new(FakeApi))
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
        let mut controller = controller_with(Arc::new(ReasoningFakeApi));

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

        controller.handle(Command::Cycle);
        assert_eq!(controller.mode(), agent::Mode::Plan);

        controller.handle(Command::Cycle);
        assert_eq!(controller.mode(), agent::Mode::Review);

        controller.handle(Command::Cycle);
        assert_eq!(controller.mode(), agent::Mode::Normal);
    }

    /// The same key reaches the surface in front of you, so with the list open
    /// it must not reach past it to the agent.
    #[test]
    fn shift_tab_cycles_the_settings_scope_while_that_list_is_open() {
        let mut controller = make_controller();
        controller.submit("/settings".into(), vec![]);
        let before = controller.settings().overlay().expect("open").scope;

        controller.handle(Command::Cycle);

        let overlay = controller.settings().overlay().expect("open");
        assert_ne!(overlay.scope, before, "the scope moved");
        assert_eq!(controller.mode(), agent::Mode::Normal, "the agent did not");
    }

    /// The login overlay covers the footer, so the mode it would change is not
    /// visible while it is open.
    #[test]
    fn cycling_does_nothing_while_logging_in() {
        let mut controller = make_controller();
        controller.submit("/login".into(), vec![]);
        assert_eq!(controller.overlay(), Overlay::Login);

        controller.handle(Command::Cycle);

        assert_eq!(controller.mode(), agent::Mode::Normal, "the agent did not");
    }

    /// A row's value differs per scope, so a half-typed one belongs to the
    /// scope being left.
    #[test]
    fn cycling_scope_abandons_a_row_being_edited() {
        let mut controller = make_controller();
        controller.submit("/settings".into(), vec![]);
        // Enter on `model`, a text row, opens its prompt.
        controller.submit(String::new(), vec![]);
        assert!(controller.settings().editing());

        controller.handle(Command::Cycle);

        assert!(!controller.settings().editing(), "the prompt closed");
        assert_eq!(controller.overlay(), Overlay::Settings, "the list stayed");
    }

    /// Which overlay is open decides where a shared key lands.
    #[test]
    fn selection_keys_route_to_whichever_overlay_is_open() {
        let mut controller = make_controller();
        controller.submit("/settings".into(), vec![]);

        controller.handle(Command::MoveSelection(1));
        assert_eq!(
            controller.settings().overlay().expect("open").selected,
            1,
            "the settings list moved"
        );

        // Backspace clears a settings row, and means nothing to the login list.
        controller.handle(Command::ClearSelection);
        controller.handle(Command::Cancel);

        controller.submit("/login".into(), vec![]);
        assert_eq!(controller.overlay(), Overlay::Login);
        controller.handle(Command::ClearSelection);
        assert_eq!(controller.overlay(), Overlay::Login, "login ignores it");
    }

    /// The footer offers "Enter save · Esc cancel" for a row's prompt, so Esc
    /// has to mean the prompt rather than the whole overlay.
    #[test]
    fn esc_while_typing_closes_the_prompt_not_the_overlay() {
        let mut controller = make_controller();
        controller.submit("/settings".into(), vec![]);
        assert_eq!(controller.overlay(), Overlay::Settings);

        // Enter on `model`, a text row, opens its prompt.
        controller.submit(String::new(), vec![]);
        assert!(controller.settings().editing());

        controller.handle(Command::Cancel);
        assert!(!controller.settings().editing(), "the prompt closed");
        assert_eq!(
            controller.overlay(),
            Overlay::Settings,
            "the list is still open"
        );

        // A second Esc, now with no prompt open, closes the overlay.
        controller.handle(Command::Cancel);
        assert_eq!(controller.overlay(), Overlay::None);
    }

    /// Submitting a prompt spawns an agent task, so this needs a runtime.
    #[tokio::test]
    async fn unknown_slash_text_is_sent_as_a_prompt() {
        let mut controller = make_controller();
        controller.submit("/logn".into(), vec![]);

        assert!(matches!(&controller.chat()[0], Entry::Prompt(text) if text == "/logn"));
    }
}
