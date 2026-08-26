use crate::AgentError;
use llm::{LlmResponse, Usage};
use tokio::sync::{
    mpsc::{self, Receiver, Sender},
    watch,
};

/// Receiver for display-level events emitted by one agent prompt.
pub struct AgentStream {
    pub(super) receiver: Receiver<Result<AgentEvent, AgentError>>,
    pub(super) cancellation: watch::Sender<bool>,
}

impl AgentStream {
    pub async fn recv(&mut self) -> Option<Result<AgentEvent, AgentError>> {
        self.receiver.recv().await
    }

    pub fn try_recv(
        &mut self,
    ) -> Result<Result<AgentEvent, AgentError>, mpsc::error::TryRecvError> {
        self.receiver.try_recv()
    }

    pub fn abort(&self) {
        let _ = self.cancellation.send(true);
    }

    /// Drain all remaining events and return the final [`LlmResponse`].
    ///
    /// Returns an error if the stream fails or is aborted before producing
    /// a [`Finished`](AgentEvent::Finished) event.
    pub async fn into_response(mut self) -> Result<LlmResponse, AgentError> {
        let mut response = None;
        while let Some(result) = self.receiver.recv().await {
            match result {
                Ok(AgentEvent::Finished { response: resp, .. }) => {
                    response = Some(*resp);
                    break;
                }
                Ok(_other) => {}
                Err(e) => return Err(e),
            }
        }
        response.ok_or(AgentError::EventStreamClosed)
    }
}

impl Drop for AgentStream {
    fn drop(&mut self) {
        let _ = self.cancellation.send(true);
    }
}

/// Display-level events emitted while an agent prompt runs.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallStarted {
        id: String,
        name: String,
        arguments: String,
    },
    ToolCallFinished {
        id: String,
        output: String,
    },
    ToolCallFailed {
        id: String,
        error: String,
    },
    Finished {
        usage: Usage,
        response: Box<LlmResponse>,
    },
}

/// Emit a display-level event, aborting if the consumer channel is closed
/// or the prompt has been cancelled.
pub(super) async fn emit_event(
    events: Option<&Sender<Result<AgentEvent, AgentError>>>,
    event: AgentEvent,
    cancellation: &mut watch::Receiver<bool>,
) -> Result<(), AgentError> {
    let Some(events) = events else { return Ok(()) };
    tokio::select! {
        result = events.send(Ok(event)) => {
            result.map_err(|_| AgentError::EventStreamClosed)
        }
        changed = cancellation.changed() => {
            changed.map_err(|_| AgentError::Aborted)?;
            Err(AgentError::Aborted)
        }
    }
}
