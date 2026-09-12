use crate::context::AgentMessage;
use llm::{ReasoningEffort, Usage};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub const SESSION_SCHEMA_VERSION: u16 = 1;

pub(crate) fn now_ms() -> u64 {
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
    /// The session this one was derived from (e.g. after a `/summarize-new`).
    /// `None` for sessions started fresh. Backwards compatible: old header
    /// lines without this field deserialize to `None`.
    #[serde(default)]
    pub parent: Option<String>,
}

impl Session {
    pub fn new(
        pwd: impl Into<PathBuf>,
        provider: impl Into<String>,
        model: impl Into<String>,
        thinking_level: Option<ReasoningEffort>,
        parent: Option<String>,
    ) -> Self {
        let now = now_ms();
        Self {
            version: SESSION_SCHEMA_VERSION,
            id: uuid::Uuid::new_v4().to_string(),
            pwd: pwd.into(),
            provider: provider.into(),
            model: model.into(),
            thinking_level,
            messages: Vec::new(),
            usage: Usage::default(),
            created_at_ms: now,
            updated_at_ms: now,
            parent,
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
            parent: self.parent.clone(),
        }
    }

    /// Apply one replayed record to this in-memory session.
    pub fn record(&mut self, record: SessionRecord) {
        let timestamp_ms = record.timestamp_ms();
        match record {
            SessionRecord::Session { .. } => {}
            SessionRecord::Message { message, .. } => self.messages.push(message),
            SessionRecord::Usage { usage, .. } => {
                self.usage = usage;
            }
        }
        self.updated_at_ms = self.updated_at_ms.max(timestamp_ms);
    }

    /// Update the session's model identity. Only the in-memory copy
    /// changes; persisting is the caller's job (see
    /// [`SessionManager::update_header_model`]).
    pub fn set_model(
        &mut self,
        provider: &str,
        model: &str,
        thinking_level: Option<ReasoningEffort>,
    ) {
        self.provider = provider.to_owned();
        self.model = model.to_owned();
        self.thinking_level = thinking_level;
        self.updated_at_ms = now_ms();
    }
}

/// One JSONL record in a session file.
///
/// ```jsonl
/// {"type":"session","version":1,"id":"018f..","pwd":"/tmp/project",..}
/// {"type":"message","message":{..},"timestamp_ms":1700000000000}
/// {"type":"usage","usage":{..},"timestamp_ms":1700000001000}
/// ```
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
        #[serde(default)]
        parent: Option<String>,
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

impl SessionRecord {
    /// Timestamp used to reconstruct `updated_at_ms` while loading.
    pub(super) fn timestamp_ms(&self) -> u64 {
        match self {
            Self::Session { updated_at_ms, .. } => *updated_at_ms,
            Self::Message { timestamp_ms, .. } | Self::Usage { timestamp_ms, .. } => *timestamp_ms,
        }
    }

    /// Serialize as one complete JSONL line (trailing newline included).
    pub(super) fn to_jsonl(&self) -> Result<String, serde_json::Error> {
        let mut line = serde_json::to_string(self)?;
        line.push('\n');
        Ok(line)
    }

    /// Parse one complete JSONL line. Internally tagged enums reject unknown
    /// `type` values and missing fields, so malformed lines fail loudly
    /// instead of becoming empty values.
    pub fn parse(line: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::AgentMessage;

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            ..Usage::default()
        }
    }

    #[test]
    fn parse_rejects_malformed_record() {
        assert!(SessionRecord::parse("not json at all").is_err());
        assert!(SessionRecord::parse(r#"{"type":"unknown"}"#).is_err());
        assert!(SessionRecord::parse(r#"{"message":{}}"#).is_err());
    }

    #[test]
    fn parse_accepts_valid_message_record() {
        let record = SessionRecord::parse(
            r#"{"type":"message","message":{"kind":"user","content":"hi"},"timestamp_ms":42}"#,
        )
        .expect("valid message record");
        assert_eq!(
            record,
            SessionRecord::Message {
                message: AgentMessage::user("hi"),
                timestamp_ms: 42,
            }
        );
    }

    #[test]
    fn to_jsonl_round_trips_one_line_per_record() {
        let record = SessionRecord::Usage {
            usage: usage(3, 4),
            timestamp_ms: 42,
        };
        let line = record.to_jsonl().expect("serialize");
        assert!(line.ends_with('\n'));
        assert_eq!(line.lines().count(), 1);
        assert_eq!(
            SessionRecord::parse(line.trim_end()).expect("parse"),
            record
        );
    }

    #[test]
    fn header_record_preserves_session_metadata() {
        let mut session = Session::new("/tmp/project", "openrouter", "test-model", None, None);
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
                parent,
            } => {
                assert_eq!(schema_version, SESSION_SCHEMA_VERSION);
                assert_eq!(id, session.id);
                assert_eq!(pwd, PathBuf::from("/tmp/project"));
                assert_eq!(provider, "openrouter");
                assert_eq!(model, "test-model");
                assert_eq!(thinking_level, None);
                assert_eq!(created_at_ms, 1_000);
                assert_eq!(updated_at_ms, 2_000);
                assert_eq!(parent, None);
            }
            other => panic!("expected header record, got {other:?}"),
        }
    }

    /// A header line written before `parent` existed must still deserialize,
    /// defaulting the field to `None` — old session files keep loading.
    #[test]
    fn header_without_parent_is_backwards_compatible() {
        let line = "\
{\"type\":\"session\",\"version\":1,\"id\":\"018f\",\"pwd\":\"/tmp/project\",\
\"provider\":\"openrouter\",\"model\":\"test-model\",\"thinking_level\":null,\
\"created_at_ms\":1,\"updated_at_ms\":2}";
        let record = SessionRecord::parse(line).expect("parse");
        let SessionRecord::Session { parent, .. } = record else {
            panic!("expected session header, got {record:?}");
        };
        assert_eq!(parent, None);
    }

    /// A header with a parent round-trips through JSONL.
    #[test]
    fn header_with_parent_round_trips() {
        let session = Session::new(
            "/tmp/project",
            "openrouter",
            "test-model",
            None,
            Some("018e".to_owned()),
        );
        let line = session.header_record().to_jsonl().expect("serialize");
        let record = SessionRecord::parse(line.trim_end()).expect("parse");
        let SessionRecord::Session { parent, .. } = record else {
            panic!("expected session header, got {record:?}");
        };
        assert_eq!(parent, Some("018e".to_owned()));
    }

    #[test]
    fn apply_record_appends_messages_and_replaces_usage_snapshot() {
        let mut session = Session::new("/tmp/project", "openrouter", "test-model", None, None);

        session.record(SessionRecord::Message {
            message: AgentMessage::user("first"),
            timestamp_ms: 1_000,
        });
        session.record(SessionRecord::Usage {
            usage: usage(100, 50),
            timestamp_ms: 3_000,
        });
        // A later snapshot replaces the earlier one; it must not be summed.
        session.record(SessionRecord::Usage {
            usage: usage(150, 60),
            timestamp_ms: 4_000,
        });

        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.usage.input_tokens, 150);
        assert_eq!(session.usage.output_tokens, 60);
    }

    #[test]
    fn set_model_updates_fields_and_timestamp() {
        let mut session = Session::new(
            "/tmp/project",
            "old-provider",
            "old-model",
            None,
            Some("018e".to_owned()),
        );
        session.created_at_ms = 1_000;
        session.updated_at_ms = 1_000;

        session.set_model("new-provider", "new-model", Some(ReasoningEffort::High));

        assert_eq!(session.provider, "new-provider");
        assert_eq!(session.model, "new-model");
        assert_eq!(session.thinking_level, Some(ReasoningEffort::High));
        assert!(session.updated_at_ms > 1_000);
    }
}
