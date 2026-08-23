use crate::context::AgentMessage;
use llm::{ReasoningEffort, Usage};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub const SESSION_SCHEMA_VERSION: u16 = 1;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before Unix epoch")
        .as_millis() as u64
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub version: u16,
    // workind directory
    pub pwd: PathBuf,
    pub provider: String,
    pub model: String,
    pub thinking_level: Option<ReasoningEffort>,
    pub messages: Vec<AgentMessage>,
    pub usage: Usage,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

impl Session {
    pub fn new(
        pwd: impl Into<PathBuf>,
        provider: impl Into<String>,
        model: impl Into<String>,
        thinking_level: Option<ReasoningEffort>,
    ) -> Self {
        Self {
            version: SESSION_SCHEMA_VERSION,
            id: uuid::Uuid::new_v4().to_string(),
            pwd: pwd.into(),
            provider: provider.into(),
            model: model.into(),
            thinking_level,
            messages: Vec::new(),
            usage: Usage::default(),
            created_at_ms: now_ms(),
            updated_at_ms: now_ms(),
        }
    }

    /// The immutable session header record for this session.
    pub fn header_record(&self) -> SessionRecord {
        SessionRecord::Session {
            version: self.version,
            id: self.id.clone(),
            pwd: self.pwd.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            thinking_level: self.thinking_level,
            created_at_ms: self.created_at_ms,
            updated_at_ms: self.updated_at_ms,
        }
    }

    pub fn record(&mut self, record: SessionRecord) {
        match record {
            SessionRecord::Session { .. } => {}
            SessionRecord::Message { message, .. } => self.messages.push(message),
            SessionRecord::Usage { usage, .. } => self.usage = usage,
        }
        self.updated_at_ms = self.updated_at_ms.max(now_ms());
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionRecord {
    Session {
        id: String,
        version: u16,
        pwd: PathBuf,
        provider: String,
        model: String,
        thinking_level: Option<ReasoningEffort>,
        created_at_ms: u64,
        updated_at_ms: u64,
    },
    Message {
        message: AgentMessage,
        timestamp_ms: u64,
    },
    Usage {
        usage: Usage,
        timestamp_ms: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::AgentMessage;

    #[test]
    fn header_record_preserves_session_metadata() {
        let mut session = Session::new("/tmp/project", "openrouter", "test-model", None);
        session.created_at_ms = 1_000;
        session.updated_at_ms = 2_000;

        match session.header_record() {
            SessionRecord::Session {
                version: schema_version,
                id,
                pwd,
                provider,
                model,
                thinking_level,
                created_at_ms,
                updated_at_ms,
            } => {
                assert_eq!(schema_version, SESSION_SCHEMA_VERSION);
                assert_eq!(id, session.id);
                assert_eq!(pwd, PathBuf::from("/tmp/project"));
                assert_eq!(provider, "openrouter");
                assert_eq!(model, "test-model");
                assert_eq!(thinking_level, None);
                assert_eq!(created_at_ms, 1_000);
                assert_eq!(updated_at_ms, 2_000);
            }
            other => panic!("expected header record, got {other:?}"),
        }
    }

    #[test]
    fn apply_record_appends_messages_and_replaces_usage_snapshot() {
        let mut session = Session::new("/tmp/project", "openrouter", "test-model", None);

        session.record(SessionRecord::Message {
            message: AgentMessage::user("first"),
            timestamp_ms: 1_000,
        });
        session.record(SessionRecord::Message {
            message: AgentMessage::user("second"),
            timestamp_ms: 2_000,
        });
        session.record(SessionRecord::Usage {
            usage: Usage {
                input_tokens: 100,
                output_tokens: 50,
                ..Usage::default()
            },
            timestamp_ms: 3_000,
        });
        // A later snapshot replaces the earlier one; it must not be summed.
        session.record(SessionRecord::Usage {
            usage: Usage {
                input_tokens: 150,
                output_tokens: 60,
                ..Usage::default()
            },
            timestamp_ms: 4_000,
        });

        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.usage.input_tokens, 150);
        assert_eq!(session.usage.output_tokens, 60);
    }
}
