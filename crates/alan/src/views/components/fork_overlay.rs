//! Checkpoint derivation for the `/fork` overlay.
//!
//! A checkpoint is every user message in the conversation. Selecting one
//! forks the session up to and including that user turn — its assistant reply
//! and any tool traffic — so the forked session can continue from there.
//!
//! The picker UI is the shared `SearchListOverlay`; `chat_view::session`
//! derives the list below and wires the selection back to the agent.

use agent::AgentMessage;
use unicode_width::UnicodeWidthChar;

const MAX_LABEL_WIDTH: usize = 60;

/// One checkpoint in the fork list: the exclusive index into the session's
/// message list where the forked history ends, plus a label.
#[derive(Debug, Clone)]
pub(crate) struct Checkpoint {
    /// Exclusive end index: `messages[..end_index]` is the forked history.
    pub(crate) end_index: usize,
    pub(crate) label: String,
}

/// Derive a checkpoint for every user message. Each checkpoint ends just
/// before the *next* user message.
pub(crate) fn derive_checkpoints(messages: &[AgentMessage]) -> Vec<Checkpoint> {
    let mut checkpoints = Vec::new();
    let mut last_user: Option<usize> = None;
    for (index, message) in messages.iter().enumerate() {
        if matches!(message, AgentMessage::User { .. }) {
            if let Some(prev) = last_user.take() {
                checkpoints.push(Checkpoint {
                    end_index: index,
                    label: label_for(&messages[prev], prev),
                });
            }
            last_user = Some(index);
        }
    }
    // The final checkpoint ends at the end of the message list.
    if let Some(prev) = last_user {
        checkpoints.push(Checkpoint {
            end_index: messages.len(),
            label: label_for(&messages[prev], prev),
        });
    }
    checkpoints
}

fn label_for(message: &AgentMessage, index: usize) -> String {
    let text = match message {
        AgentMessage::User { text, .. } => text.as_str(),
        AgentMessage::Assistant(_) => "",
        AgentMessage::ToolResult { content, .. } => content.as_str(),
    };

    let line = text.lines().next().unwrap_or("").trim();
    let mut label = String::new();
    let mut width = 0;
    let mut truncated = false;
    for c in line.chars() {
        let char_width = c.width().unwrap_or(0);
        if width + char_width > MAX_LABEL_WIDTH {
            truncated = true;
            break;
        }
        label.push(c);
        width += char_width;
    }
    if truncated {
        label.push_str("...");
    }

    if label.is_empty() {
        format!("#{}", index + 1)
    } else {
        format!("#{} — {}", index + 1, label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> AgentMessage {
        AgentMessage::User {
            text: text.to_owned(),
            images: Vec::new(),
        }
    }

    fn assistant() -> AgentMessage {
        AgentMessage::Assistant(llm::LlmResponse {
            content: Vec::new(),
            stop_reason: llm::StopReason::Stop,
            usage: None,
            model: None,
            reasoning: None,
            reasoning_details: Vec::new(),
        })
    }

    /// Every user message is a checkpoint; the last one ends at the end of the
    /// message list, and earlier ones end just before the next user message.
    #[test]
    fn checkpoints_are_every_user_message() {
        let messages = vec![
            user("first"),
            assistant(),
            user("second"),
            assistant(),
            user("third"),
        ];
        let checkpoints = derive_checkpoints(&messages);
        assert_eq!(checkpoints.len(), 3);
        // Each checkpoint ends just before the *next* user message, so the
        // fork includes the selected user turn plus its reply/tool traffic.
        assert_eq!(checkpoints[0].end_index, 2);
        assert_eq!(checkpoints[1].end_index, 4);
        assert_eq!(checkpoints[2].end_index, 5);
        assert!(checkpoints[0].label.contains("first"));
        assert!(checkpoints[1].label.contains("second"));
        assert!(checkpoints[2].label.contains("third"));
    }

    /// An empty conversation has no checkpoints.
    #[test]
    fn empty_conversation_has_no_checkpoints() {
        let checkpoints = derive_checkpoints(&[]);
        assert!(checkpoints.is_empty());
    }

    /// A single user turn yields one checkpoint ending at the message count.
    #[test]
    fn single_turn_yields_one_checkpoint() {
        let messages = vec![user("only"), assistant()];
        let checkpoints = derive_checkpoints(&messages);
        assert_eq!(checkpoints.len(), 1);
        assert_eq!(checkpoints[0].end_index, 2);
    }
}
