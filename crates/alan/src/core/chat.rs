//! Chat feature state and agent stream coordination.

use super::Poll;
use agent::{Agent, AgentEvent, AgentStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub enum Entry {
    Prompt(String),
    Response(String),
    Error(String),
}

const POLL_EVENT_LIMIT: usize = 256;
const POLL_TIME_BUDGET: Duration = Duration::from_millis(2);

pub struct ChatController {
    agent: Arc<Agent>,
    entries: Vec<Entry>,
    stream: Option<AgentStream>,
    busy: bool,
    aborting: bool,
}

impl ChatController {
    pub fn new(agent: Agent) -> Self {
        Self {
            agent: Arc::new(agent),
            entries: Vec::new(),
            stream: None,
            busy: false,
            aborting: false,
        }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn is_busy(&self) -> bool {
        self.busy
    }

    pub fn submit(&mut self, text: impl Into<String>) {
        let text = text.into();
        let text = text.trim();
        if text.is_empty() || self.busy {
            return;
        }

        self.entries.push(Entry::Prompt(text.to_owned()));
        self.busy = true;
        self.stream = Some(self.agent.prompt_stream(text.to_owned()));
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

        while processed < POLL_EVENT_LIMIT && started.elapsed() < POLL_TIME_BUDGET {
            match stream.try_recv() {
                Ok(Ok(AgentEvent::TextDelta(text))) => {
                    pending_text.push_str(&text);
                    processed += 1;
                }
                Ok(Ok(AgentEvent::Finished)) => {
                    Self::append_delta(&mut self.entries, &pending_text);
                    pending_text.clear();
                    Self::ensure_response_entry(&mut self.entries);
                    self.busy = false;
                    outcome = Poll::Finished;
                    break;
                }
                Ok(Err(error)) => {
                    Self::append_delta(&mut self.entries, &pending_text);
                    pending_text.clear();
                    self.busy = false;
                    if matches!(error, agent::AgentError::Aborted) || self.aborting {
                        self.aborting = false;
                        outcome = Poll::Aborted;
                    } else {
                        self.entries.push(Entry::Error(error.to_string()));
                        outcome = Poll::Error;
                    }
                    break;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    Self::append_delta(&mut self.entries, &pending_text);
                    pending_text.clear();
                    self.busy = false;
                    if self.aborting {
                        self.aborting = false;
                        outcome = Poll::Aborted;
                    } else {
                        self.entries
                            .push(Entry::Error("agent stream disconnected".into()));
                        outcome = Poll::Error;
                    }
                    break;
                }
            }
        }

        if !pending_text.is_empty() {
            Self::append_delta(&mut self.entries, &pending_text);
            outcome = outcome.combine(Poll::Changed);
        }

        if self.busy {
            self.stream = Some(stream);
        }
        outcome
    }

    fn append_delta(entries: &mut Vec<Entry>, delta: &str) {
        if delta.is_empty() {
            return;
        }

        match entries.last_mut() {
            Some(Entry::Response(text)) => text.push_str(delta),
            _ => entries.push(Entry::Response(delta.to_owned())),
        }
    }

    fn ensure_response_entry(entries: &mut Vec<Entry>) {
        if !matches!(entries.last(), Some(Entry::Response(_))) {
            entries.push(Entry::Response(String::new()));
        }
    }
}
