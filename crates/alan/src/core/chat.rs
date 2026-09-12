//! Chat feature state and agent stream coordination.

use super::action::ImageAttachment;

use agent::{Agent, AgentEvent, AgentStream};
use llm::Usage;
use std::sync::Arc;

/// What the prompt is doing, and so what Enter does to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activity {
    /// Streaming a response.
    Thinking,

    /// A blocking, non-agent operation is in flight (for example
    /// `/summarize-new`); the string is the label shown in the status line.
    Loading(String),

    /// Waiting on a prompt.
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Prompt(String),
    Response(String),
    Reasoning(String),
    Info(String),
    ToolCall {
        id: String,
        name: String,
        arguments: String,
        output: String,
        status: ToolStatus,
    },
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Completed,
    Failed(String),
}

pub struct ChatController {
    agent: Arc<Agent>,
    entries: Vec<Entry>,
    busy: bool,
    /// Blocking operation label in flight, e.g. `Some("summarizing")` during
    /// `/summarize-new`. While set, input is blocked and the status line shows
    /// the label in place of the idle indicator.
    loading: Option<String>,
    revision: u64,
    usage: Usage,
    model_name: String,

    /// Maximum context window from the model catalog. `None` means unknown.
    max_context: Option<u64>,
}

impl ChatController {
    pub fn new(agent: Agent, name: String) -> Self {
        Self {
            agent: Arc::new(agent),
            entries: Vec::new(),
            busy: false,
            loading: None,
            revision: 0,
            usage: Usage::default(),
            model_name: name,
            max_context: Some(0),
        }
    }

    pub fn agent(&self) -> Arc<Agent> {
        Arc::clone(&self.agent)
    }

    pub async fn restore_session_history(&mut self) {
        let messages = self.agent.messages().await;
        let usage = self.agent.usage().await;
        let info = self.agent.info().await;
        self.model_name = info.name;
        self.usage = usage;
        self.max_context = info.context_length;
        self.entries.clear();

        for message in messages {
            match message {
                agent::AgentMessage::User { text, .. } => self.entries.push(Entry::Prompt(text)),
                agent::AgentMessage::Assistant(response) => {
                    if let Some(reasoning) = response.reasoning.as_deref()
                        && !reasoning.is_empty()
                    {
                        self.entries.push(Entry::Reasoning(reasoning.to_owned()));
                    }
                    for call in response.tool_calls() {
                        self.entries.push(Entry::ToolCall {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            arguments: call.arguments.clone(),
                            output: String::new(),
                            status: ToolStatus::Completed,
                        });
                    }
                    let text = response.text();
                    if !text.is_empty() {
                        self.entries.push(Entry::Response(text));
                    }
                }
                agent::AgentMessage::ToolResult {
                    tool_call_id,
                    content,
                    ..
                } => {
                    if let Some(Entry::ToolCall { output, .. }) = self
                        .entries
                        .iter_mut()
                        .rev()
                        .find(|entry| {
                            matches!(entry, Entry::ToolCall { id, .. } if id == &tool_call_id)
                        })
                    {
                        *output = content;
                    }
                }
            }
        }
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn is_busy(&self) -> bool {
        self.busy
    }

    /// The blocking-operation label, if one is in flight.
    pub fn loading(&self) -> Option<&str> {
        self.loading.as_deref()
    }

    /// Set (or clear) the blocking-operation label. Bumps the revision only
    /// when the value actually changes, so `refresh` rebuilds exactly once.
    pub fn set_loading(&mut self, loading: Option<String>) {
        if self.loading == loading {
            return;
        }
        self.loading = loading;
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn mode(&self) -> agent::Mode {
        self.agent.mode()
    }

    pub fn usage(&self) -> Usage {
        self.usage.clone()
    }

    pub fn model_name(&self) -> String {
        self.model_name.clone()
    }

    pub fn max_context(&self) -> Option<u64> {
        self.max_context
    }

    pub fn set_max_context(&mut self, max_context: Option<u64>) {
        if self.max_context == max_context {
            return;
        }
        self.max_context = max_context;
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn toggle_mode(&mut self) {
        let next = match self.agent.mode() {
            agent::Mode::Normal => agent::Mode::Plan,
            agent::Mode::Plan => agent::Mode::Review,
            agent::Mode::Review => agent::Mode::Normal,
        };
        self.agent.set_mode(next);
    }

    pub fn set_mode(&mut self, mode: agent::Mode) {
        self.agent.set_mode(mode);
    }

    pub fn push_info(&mut self, text: impl Into<String>) {
        self.entries.push(Entry::Info(text.into()));
        self.revision = self.revision.wrapping_add(1);
    }

    /// Clear the visible transcript and reset the displayed usage total.
    pub fn clear_transcript(&mut self) {
        self.entries.clear();
        self.usage = Usage::default();
        self.revision = self.revision.wrapping_add(1);
    }

    /// Push the prompt and start the agent run, returning the stream of
    /// display events for the caller to feed back via [`apply_event`](Self::apply_event).
    ///
    /// Returns `None` when the prompt is empty, the controller is busy, or the
    /// agent rejects the request (in which case an error entry replaces the
    /// placeholder prompt).
    pub fn submit(&mut self, text: String, images: Vec<ImageAttachment>) -> Option<AgentStream> {
        let text = text.trim();
        if (text.is_empty() && images.is_empty()) || self.busy || self.loading.is_some() {
            return None;
        }

        self.entries.push(Entry::Prompt(text.to_owned()));
        self.revision = self.revision.wrapping_add(1);
        let image_urls: Vec<llm::ImageUrl> = images
            .into_iter()
            .map(|img| llm::ImageUrl {
                url: format!("data:{};base64,{}", img.mime_type, img.base64_data),
            })
            .collect();
        let builder = self
            .agent
            .prompt()
            .content(text.to_owned())
            .images(image_urls)
            .stream(true);

        match self.agent.ask(builder) {
            Ok(stream) => {
                self.busy = true;
                Some(stream)
            }
            Err(error) => {
                // Replace the placeholder prompt with the failure so a
                // validation mismatch between crates can never panic.
                self.entries.pop();
                self.entries.push(Entry::Error(error.to_string()));
                self.revision = self.revision.wrapping_add(1);
                None
            }
        }
    }

    /// Apply one display event from the agent stream, mutating the transcript
    /// and status. `Finished` and error events clear the busy state.
    pub fn apply_event(&mut self, result: Result<AgentEvent, agent::AgentError>) {
        let mut changed = false;
        match result {
            Ok(event) => match event {
                AgentEvent::TextDelta(text) => {
                    changed |= Self::append_delta(&mut self.entries, &text);
                }
                AgentEvent::ReasoningDelta(reasoning) => {
                    changed |= Self::append_reasoning(&mut self.entries, &reasoning);
                }
                AgentEvent::ToolCallStarted {
                    id,
                    name,
                    arguments,
                } => {
                    self.entries.push(Entry::ToolCall {
                        id,
                        name,
                        arguments,
                        output: String::new(),
                        status: ToolStatus::Running,
                    });
                    changed = true;
                }
                AgentEvent::ToolCallFinished { id, output } => {
                    changed |= Self::update_tool_call(
                        &mut self.entries,
                        &id,
                        output,
                        ToolStatus::Completed,
                    );
                }
                AgentEvent::ToolCallFailed { id, error } => {
                    changed |= Self::update_tool_call(
                        &mut self.entries,
                        &id,
                        String::new(),
                        ToolStatus::Failed(error),
                    );
                }
                AgentEvent::Usage { usage } => {
                    self.usage = usage;
                    changed = true;
                }
                AgentEvent::Finished { usage, .. } => {
                    Self::ensure_response_entry(&mut self.entries);
                    self.usage = usage;
                    self.busy = false;
                    changed = true;
                }
            },
            Err(error) => {
                self.busy = false;
                // An aborted run (the stream was dropped) carries no message.
                if !matches!(error, agent::AgentError::Aborted) {
                    self.entries.push(Entry::Error(error.to_string()));
                    changed = true;
                }
            }
        }

        if changed {
            self.revision = self.revision.wrapping_add(1);
        }
    }

    /// Clear the busy state without recording an error. Used when the run was
    /// intentionally cancelled (the stream subscription was dropped). No-op
    /// once `Finished`/error has already been applied.
    pub fn finish_stream(&mut self) {
        if self.busy {
            self.busy = false;
            self.revision = self.revision.wrapping_add(1);
        }
    }

    /// Handle the event stream closing. If the run is still busy, the task
    /// ended without a terminal event (e.g. it panicked); record a disconnect
    /// error. A completed or errored run has already cleared `busy`.
    pub fn disconnect_stream(&mut self) {
        if self.busy {
            self.busy = false;
            self.entries
                .push(Entry::Error("agent stream disconnected".into()));
            self.revision = self.revision.wrapping_add(1);
        }
    }

    /// Apply a completed model switch: refresh the cached model name and
    /// record the change in the transcript.
    pub fn apply_model_switch(&mut self, model_name: String) {
        self.model_name = model_name.clone();
        self.entries
            .push(Entry::Info(format!("Switched to {model_name}")));
        self.revision = self.revision.wrapping_add(1);
    }

    /// Record a failed switch attempt in the transcript.
    pub fn apply_model_switch_failed(&mut self, error: String) {
        self.entries
            .push(Entry::Error(format!("Model switch failed: {error}")));
        self.revision = self.revision.wrapping_add(1);
    }

    fn append_delta(entries: &mut Vec<Entry>, delta: &str) -> bool {
        if delta.is_empty() {
            return false;
        }

        match entries.last_mut() {
            Some(Entry::Response(text)) => text.push_str(delta),
            _ => entries.push(Entry::Response(delta.to_owned())),
        }
        true
    }

    fn append_reasoning(entries: &mut Vec<Entry>, delta: &str) -> bool {
        if delta.is_empty() {
            return false;
        }

        match entries.last_mut() {
            Some(Entry::Reasoning(text)) => text.push_str(delta),
            _ => entries.push(Entry::Reasoning(delta.to_owned())),
        }
        true
    }

    fn ensure_response_entry(entries: &mut Vec<Entry>) -> bool {
        if matches!(entries.last(), Some(Entry::Response(_))) {
            return false;
        }
        entries.push(Entry::Response(String::new()));
        true
    }

    fn update_tool_call(
        entries: &mut [Entry],
        id: &str,
        output: String,
        status: ToolStatus,
    ) -> bool {
        let Some(entry) = entries
            .iter_mut()
            .find(|entry| matches!(entry, Entry::ToolCall { id: entry_id, .. } if entry_id == id))
        else {
            return false;
        };

        if let Entry::ToolCall {
            output: entry_output,
            status: entry_status,
            ..
        } = entry
        {
            *entry_output = output;
            *entry_status = status;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use providers::{Model, Provider, ProviderId};

    fn test_model() -> Model {
        providers::OpenRouterProvider::builder("key")
            .with_models([providers::ModelInfo {
                provider: ProviderId::new("openrouter"),
                id: "test".into(),
                name: "Test".into(),
                api: providers::ApiId::ChatCompletions,
                capabilities: providers::ModelCapabilities::default(),
                pricing: None,
                context_length: None,
            }])
            .with_api(std::sync::Arc::new(FakeApi))
            .build()
            .unwrap()
            .bind("test")
            .unwrap()
    }

    struct FakeApi;

    #[async_trait::async_trait]
    impl llm::LlmApi for FakeApi {
        async fn stream(
            &self,
            _request: llm::LlmRequest<'_>,
        ) -> Result<llm::LlmStream, llm::LlmError> {
            Ok(Box::pin(futures_util::stream::iter([
                Ok(llm::LlmEvent::TextDelta {
                    text: "test".to_string(),
                }),
                Ok(llm::LlmEvent::Done {
                    stop_reason: llm::StopReason::Stop,
                    usage: None,
                    model: None,
                }),
            ])))
        }
    }

    fn make_controller(name: &str) -> ChatController {
        let agent = Agent::builder(test_model()).build().unwrap();
        ChatController::new(agent, name.to_string())
    }

    #[test]
    fn apply_model_switch_updates_name_and_logs() {
        let mut controller = make_controller("old-model");
        controller.entries.push(Entry::Prompt("test".to_string()));
        controller.apply_model_switch("new-model".to_string());
        assert_eq!(controller.model_name(), "new-model");
        assert_eq!(
            controller.entries().last(),
            Some(&Entry::Info("Switched to new-model".to_string()))
        );
        // revision incremented by the prompt entry
        assert_eq!(controller.revision(), 1);
    }

    #[test]
    fn apply_model_switch_failed_records_error() {
        let mut controller = make_controller("old-model");
        controller.entries.push(Entry::Prompt("test".to_string()));
        let old_revision = controller.revision();
        controller.apply_model_switch_failed("timeout".to_string());
        assert_eq!(controller.model_name(), "old-model");
        assert_eq!(
            controller.entries().last(),
            Some(&Entry::Error("Model switch failed: timeout".to_string()))
        );
        assert!(controller.revision() > old_revision);
    }
}
