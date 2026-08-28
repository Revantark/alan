//! Chat feature state and agent stream coordination.

use super::Poll;
use super::action::ImageAttachment;
use agent::{Agent, AgentEvent, AgentStream};
use llm::Usage;
use std::sync::Arc;
use std::time::{Duration, Instant};

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

const POLL_EVENT_LIMIT: usize = 256;
const POLL_TIME_BUDGET: Duration = Duration::from_millis(2);

pub struct ChatController {
    agent: Arc<Agent>,
    entries: Vec<Entry>,
    stream: Option<AgentStream>,
    busy: bool,
    aborting: bool,
    revision: u64,
    usage: Usage,
}

impl ChatController {
    pub fn new(agent: Agent) -> Self {
        Self {
            agent: Arc::new(agent),
            entries: Vec::new(),
            stream: None,
            busy: false,
            aborting: false,
            revision: 0,
            usage: Usage::default(),
        }
    }

    pub async fn session_id(&self) -> Option<String> {
        self.agent.session_id().await
    }

    pub async fn restore_session_history(&mut self) {
        let messages = self.agent.messages().await;
        self.usage = self.agent.usage().await;
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

    pub fn plan_mode(&self) -> bool {
        self.agent.plan_mode()
    }

    pub fn usage(&self) -> Usage {
        self.usage.clone()
    }

    pub fn toggle_plan_mode(&mut self) {
        self.agent.set_plan_mode(!self.agent.plan_mode());
    }

    pub fn push_info(&mut self, text: impl Into<String>) {
        self.entries.push(Entry::Info(text.into()));
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn submit(&mut self, text: impl Into<String>, images: Vec<ImageAttachment>) {
        let text = text.into();
        let text = text.trim();
        if (text.is_empty() && images.is_empty()) || self.busy {
            return;
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
                self.stream = Some(stream);
            }
            Err(error) => {
                // Replace the placeholder prompt with the failure so a
                // validation mismatch between crates can never panic.
                self.entries.pop();
                self.entries.push(Entry::Error(error.to_string()));
                self.revision = self.revision.wrapping_add(1);
            }
        }
    }

    pub fn abort(&mut self) {
        if !self.busy {
            return;
        }

        self.aborting = true;
        if let Some(stream) = &self.stream {
            stream.abort();
        }
    }

    pub fn poll(&mut self) -> Poll {
        let Some(mut stream) = self.stream.take() else {
            return Poll::Idle;
        };

        let mut outcome = Poll::Idle;
        let started = Instant::now();
        let mut processed = 0;
        let mut pending_text = String::new();
        let mut changed = false;

        while processed < POLL_EVENT_LIMIT && started.elapsed() < POLL_TIME_BUDGET {
            match stream.try_recv() {
                Ok(Ok(event)) => {
                    processed += 1;
                    match event {
                        AgentEvent::TextDelta(text) => {
                            pending_text.push_str(&text);
                        }
                        AgentEvent::ReasoningDelta(reasoning) => {
                            changed |= Self::append_delta(&mut self.entries, &pending_text);
                            pending_text.clear();
                            changed |= Self::append_reasoning(&mut self.entries, &reasoning);
                        }
                        AgentEvent::ToolCallStarted {
                            id,
                            name,
                            arguments,
                        } => {
                            Self::append_delta(&mut self.entries, &pending_text);
                            pending_text.clear();
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
                            changed |= Self::append_delta(&mut self.entries, &pending_text);
                            pending_text.clear();
                            changed |= Self::ensure_response_entry(&mut self.entries);
                            self.usage = usage;
                            self.busy = false;
                            outcome = Poll::Finished;
                            break;
                        }
                    }
                }
                Ok(Err(error)) => {
                    changed |= Self::append_delta(&mut self.entries, &pending_text);
                    pending_text.clear();
                    self.busy = false;
                    if matches!(error, agent::AgentError::Aborted) || self.aborting {
                        self.aborting = false;
                        outcome = Poll::Aborted;
                    } else {
                        self.entries.push(Entry::Error(error.to_string()));
                        changed = true;
                        outcome = Poll::Error;
                    }
                    break;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    changed |= Self::append_delta(&mut self.entries, &pending_text);
                    pending_text.clear();
                    self.busy = false;
                    if self.aborting {
                        self.aborting = false;
                        outcome = Poll::Aborted;
                    } else {
                        self.entries
                            .push(Entry::Error("agent stream disconnected".into()));
                        changed = true;
                        outcome = Poll::Error;
                    }
                    break;
                }
            }
        }
        changed |= Self::append_delta(&mut self.entries, &pending_text);
        if changed {
            self.revision = self.revision.wrapping_add(1);
            outcome = outcome.combine(Poll::Changed);
        }

        if self.busy {
            self.stream = Some(stream);
        }
        outcome
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
