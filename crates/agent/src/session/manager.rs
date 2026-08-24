use super::error::SessionError;
use super::record::{SESSION_SCHEMA_VERSION, Session, SessionRecord, SessionSummary, now_ms};
use super::store::StoreError;
use super::store::{JsonlStore, split_complete_lines};
use crate::context::AgentMessage;
use crate::session::dir::pwd_key;
use llm::{ReasoningEffort, Usage};
use std::path::{Path, PathBuf};

/// Normalize a pwd, mapping I/O failure into the store error shape.
fn normalize(pwd: PathBuf) -> Result<PathBuf, SessionError> {
    let dir = pwd.clone();
    super::dir::normalize_pwd(pwd)
        .map_err(|source| SessionError::Store(StoreError::CreateDir { dir, source }))
}

/// Filesystem-backed session store over [`JsonlStore`].
pub struct SessionManager {
    store: JsonlStore,
}

impl SessionManager {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            store: JsonlStore::new(root),
        }
    }

    /// Create a new session and write its header record immediately.
    ///
    /// Directories are created only here, so constructing the manager does
    /// not touch the filesystem.
    pub async fn create(
        &self,
        pwd: impl Into<PathBuf>,
        provider: impl Into<String>,
        model: impl Into<String>,
        thinking_level: Option<ReasoningEffort>,
    ) -> Result<Session, SessionError> {
        let pwd = normalize(pwd.into())?;
        let key = pwd_key(&pwd);
        let session = Session::new(pwd, provider, model, thinking_level);

        // Serialize fully before writing; exclusive creation refuses to
        // overwrite an existing session with the same id.
        let header =
            session
                .header_record()
                .to_jsonl()
                .map_err(|source| SessionError::Serialize {
                    path: self.store.file_path(&key, &session.id),
                    source,
                })?;
        self.store.create_file(&key, &session.id, &header).await?;

        Ok(session)
    }

    /// Append exactly one message record to the session file.
    pub async fn append_message(
        &self,
        session: &Session,
        message: &AgentMessage,
    ) -> Result<(), SessionError> {
        let record = SessionRecord::Message {
            message: message.clone(),
            timestamp_ms: now_ms(),
        };
        self.append_record(session, &record).await
    }

    /// Append an aggregate usage snapshot. Loading replaces (never sums)
    /// `Session.usage` with the newest snapshot.
    pub async fn save_usage(&self, session: &Session, usage: &Usage) -> Result<(), SessionError> {
        let record = SessionRecord::Usage {
            usage: usage.clone(),
            timestamp_ms: now_ms(),
        };
        self.append_record(session, &record).await
    }

    /// Reconstruct a complete session by replaying its records.
    pub async fn load(&self, pwd: &Path, id: &str) -> Result<Session, SessionError> {
        validate_session_id(id)?;
        let normalized = normalize(pwd.to_path_buf())?;
        let key = pwd_key(&normalized);
        let content = self.store.read(&key, id).await?;

        let mut lines = split_complete_lines(&content);
        let path = self.store.file_path(&key, id);

        let header_line =
            lines
                .next()
                .map(|(line, _)| line)
                .ok_or_else(|| SessionError::InvalidHeader {
                    path: path.clone(),
                    reason: "file is empty".into(),
                })?;
        let record =
            SessionRecord::parse(header_line).map_err(|err| SessionError::InvalidHeader {
                path: path.clone(),
                reason: err.to_string(),
            })?;
        let SessionRecord::Session {
            id: header_id,
            version,
            pwd: header_pwd,
            provider,
            model,
            thinking_level,
            created_at_ms,
            updated_at_ms,
        } = record
        else {
            return Err(SessionError::InvalidHeader {
                path: path.clone(),
                reason: "first record is not a session header".into(),
            });
        };

        if version > SESSION_SCHEMA_VERSION {
            return Err(SessionError::UnsupportedVersion {
                version,
                supported: SESSION_SCHEMA_VERSION,
                path: path.clone(),
            });
        }
        if header_id != id {
            return Err(SessionError::InvalidHeader {
                path: path.clone(),
                reason: format!("header id {header_id:?} does not match requested id {id:?}"),
            });
        }
        if normalize(header_pwd.clone())? != normalized {
            return Err(SessionError::InvalidHeader {
                path: path.clone(),
                reason: format!(
                    "header pwd {:?} does not match requested pwd {:?}",
                    header_pwd.display(),
                    normalized.display()
                ),
            });
        }

        let mut session = Session {
            id: header_id,
            version,
            pwd: header_pwd,
            provider,
            model,
            thinking_level,
            messages: Vec::new(),
            usage: Usage::default(),
            created_at_ms,
            updated_at_ms: updated_at_ms.max(created_at_ms),
        };

        for (line, complete) in lines {
            if !complete {
                continue;
            }
            let record =
                SessionRecord::parse(line).map_err(|err| SessionError::MalformedRecord {
                    path: path.clone(),
                    reason: err.to_string(),
                })?;
            let timestamp_ms = record.timestamp_ms();
            match record {
                SessionRecord::Session { .. } => {
                    return Err(SessionError::MalformedRecord {
                        path: path.clone(),
                        reason: "unexpected session header inside file".into(),
                    });
                }
                SessionRecord::Message { message, .. } => session.messages.push(message),
                SessionRecord::Usage { usage, .. } => session.usage = usage,
            }
            session.updated_at_ms = session.updated_at_ms.max(timestamp_ms);
        }

        Ok(session)
    }

    /// Summaries for every session recorded under `pwd`, newest first.
    ///
    /// Only headers and usage snapshots are read; message bodies are skipped.
    /// Unreadable or corrupt files are skipped rather than failing the whole
    /// listing.
    pub async fn list(&self, pwd: &Path) -> Result<Vec<SessionSummary>, SessionError> {
        let normalized = normalize(pwd.to_path_buf())?;
        let key = pwd_key(&normalized);

        let files = self.store.files(&key).await?;
        let mut summaries: Vec<SessionSummary> = Vec::with_capacity(files.len());
        for path in files {
            if let Ok(summary) = self.summarize(&path).await {
                summaries.push(summary);
            }
        }

        summaries.sort_by_key(|summary| std::cmp::Reverse(summary.updated_at_ms));
        Ok(summaries)
    }

    async fn summarize(&self, path: &Path) -> Result<SessionSummary, SessionError> {
        let content = tokio::fs::read_to_string(path).await.map_err(|source| {
            SessionError::Store(StoreError::ReadFile {
                path: path.to_path_buf(),
                source,
            })
        })?;
        let mut lines = split_complete_lines(&content);

        let (header_line, _) = lines.next().ok_or_else(|| SessionError::InvalidHeader {
            path: path.to_path_buf(),
            reason: "file is empty".into(),
        })?;
        let record =
            SessionRecord::parse(header_line).map_err(|err| SessionError::InvalidHeader {
                path: path.to_path_buf(),
                reason: err.to_string(),
            })?;
        let SessionRecord::Session {
            id,
            version,
            pwd,
            provider,
            model,
            thinking_level,
            created_at_ms,
            updated_at_ms,
        } = record
        else {
            return Err(SessionError::InvalidHeader {
                path: path.to_path_buf(),
                reason: "first record is not a session header".into(),
            });
        };
        if version > SESSION_SCHEMA_VERSION {
            return Err(SessionError::UnsupportedVersion {
                version,
                supported: SESSION_SCHEMA_VERSION,
                path: path.to_path_buf(),
            });
        }

        let mut latest_usage = Usage::default();
        let mut max_updated_at_ms = updated_at_ms.max(created_at_ms);
        for (line, complete) in lines {
            if !complete {
                continue;
            }
            if let Ok(record) = SessionRecord::parse(line) {
                max_updated_at_ms = max_updated_at_ms.max(record.timestamp_ms());
                if let SessionRecord::Usage { usage, .. } = record {
                    latest_usage = usage;
                }
            }
        }

        Ok(SessionSummary {
            id,
            pwd,
            provider,
            model,
            thinking_level,
            created_at_ms,
            updated_at_ms: max_updated_at_ms,
            usage: latest_usage,
        })
    }

    async fn append_record(
        &self,
        session: &Session,
        record: &SessionRecord,
    ) -> Result<(), SessionError> {
        let key = pwd_key(&session.pwd);
        let line = record
            .to_jsonl()
            .map_err(|source| SessionError::Serialize {
                path: self.store.file_path(&key, &session.id),
                source,
            })?;
        Ok(self.store.append(&key, &session.id, &line).await?)
    }
}

fn validate_session_id(id: &str) -> Result<(), SessionError> {
    let valid = !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(SessionError::MalformedRecord {
            path: PathBuf::from(id),
            reason: "session id must be 1-64 ascii alphanumerics or dashes".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::AgentMessage;
    use crate::session::store::JSONL_EXTENSION;

    fn temp_root(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("alan-session-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp root");
        dir
    }

    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            ..Usage::default()
        }
    }

    fn session_file(root: &Path, pwd: &str, id: &str) -> PathBuf {
        root.join(pwd_key(Path::new(pwd)))
            .join(format!("{id}.{JSONL_EXTENSION}"))
    }

    #[tokio::test]
    async fn constructing_manager_creates_nothing() {
        let root =
            std::env::temp_dir().join(format!("alan-session-construct-{}", uuid::Uuid::new_v4()));
        let _manager = SessionManager::new(&root);
        assert!(!root.exists(), "manager construction must not create files");
    }

    #[tokio::test]
    async fn create_writes_single_header_file_under_pwd_dir() {
        let root = temp_root("create");
        let manager = SessionManager::new(&root);

        let session = manager
            .create("/tmp/project", "openrouter", "test-model", None)
            .await
            .expect("create session");

        let pwd_dir = root.join(pwd_key(Path::new("/tmp/project")));
        assert!(pwd_dir.is_dir(), "pwd-specific directory exists");
        let entries: Vec<_> = std::fs::read_dir(&pwd_dir).expect("read pwd dir").collect();
        assert_eq!(entries.len(), 1, "exactly one file after create");

        let content = std::fs::read_to_string(session_file(&root, "/tmp/project", &session.id))
            .expect("read session file");
        let first = content.lines().next().expect("header line");
        let record: SessionRecord = serde_json::from_str(first).expect("valid header json");
        assert!(matches!(record, SessionRecord::Session { .. }));
        cleanup(&root);
    }

    #[tokio::test]
    async fn different_pwds_use_different_directories() {
        let root = temp_root("pwds");
        let manager = SessionManager::new(&root);

        manager
            .create("/tmp/a", "openrouter", "m", None)
            .await
            .expect("create a");
        manager
            .create("/tmp/b", "openrouter", "m", None)
            .await
            .expect("create b");

        assert_ne!(pwd_key(Path::new("/tmp/a")), pwd_key(Path::new("/tmp/b")));
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
        cleanup(&root);
    }

    #[tokio::test]
    async fn append_and_load_roundtrip() {
        let root = temp_root("roundtrip");
        let manager = SessionManager::new(&root);

        let session = manager
            .create(
                "/tmp/project",
                "openrouter",
                "test-model",
                Some(ReasoningEffort::High),
            )
            .await
            .expect("create");

        manager
            .append_message(&session, &AgentMessage::user("hello"))
            .await
            .expect("append user");
        manager
            .append_message(&session, &AgentMessage::user("second"))
            .await
            .expect("append second");
        manager
            .save_usage(&session, &usage(10, 5))
            .await
            .expect("usage 1");
        manager
            .save_usage(&session, &usage(25, 8))
            .await
            .expect("usage 2");

        let loaded = manager
            .load(Path::new("/tmp/project"), &session.id)
            .await
            .expect("load");

        assert_eq!(loaded.id, session.id);
        assert_eq!(loaded.version, SESSION_SCHEMA_VERSION);
        assert_eq!(loaded.pwd, session.pwd);
        assert_eq!(loaded.provider, "openrouter");
        assert_eq!(loaded.model, "test-model");
        assert_eq!(loaded.thinking_level, Some(ReasoningEffort::High));
        assert_eq!(loaded.created_at_ms, session.created_at_ms);
        assert_eq!(
            loaded.messages,
            vec![AgentMessage::user("hello"), AgentMessage::user("second")]
        );
        assert_eq!(loaded.usage, usage(25, 8));
        cleanup(&root);
    }

    #[tokio::test]
    async fn usage_snapshots_replace_not_sum() {
        let root = temp_root("usage");
        let manager = SessionManager::new(&root);
        let session = manager
            .create("/tmp/p", "o", "m", None)
            .await
            .expect("create");

        manager
            .save_usage(&session, &usage(10, 5))
            .await
            .expect("usage 1");
        manager
            .save_usage(&session, &usage(20, 7))
            .await
            .expect("usage 2");

        let loaded = manager
            .load(Path::new("/tmp/p"), &session.id)
            .await
            .expect("load");
        assert_eq!(loaded.usage, usage(20, 7));
        cleanup(&root);
    }

    #[tokio::test]
    async fn truncated_final_line_tolerated_malformed_complete_line_rejected() {
        let root = temp_root("truncated");
        let manager = SessionManager::new(&root);
        let session = manager
            .create("/tmp/p", "o", "m", None)
            .await
            .expect("create");
        let path = session_file(&root, "/tmp/p", &session.id);

        // Truncated final append (no trailing newline) is tolerated.
        let mut content = std::fs::read_to_string(&path).unwrap();
        content.push_str(
            r#"{"type":"message","message":{"kind":"user","content":"ok"},"timestamp_ms":1}"#,
        );
        content.push('\n');
        content.push_str(r#"{"type":"message","message":{"kind":"user""#);
        std::fs::write(&path, &content).unwrap();

        let loaded = manager
            .load(Path::new("/tmp/p"), &session.id)
            .await
            .expect("truncated tail tolerated");
        assert_eq!(loaded.messages.len(), 1);

        // A malformed complete line is an error, even at the end.
        content.push_str("\nthis is not json\n");
        std::fs::write(&path, &content).unwrap();

        let err = manager
            .load(Path::new("/tmp/p"), &session.id)
            .await
            .expect_err("malformed complete line rejected");
        assert!(matches!(err, SessionError::MalformedRecord { .. }));
        cleanup(&root);
    }

    #[tokio::test]
    async fn wrong_pwd_or_id_does_not_load() {
        let root = temp_root("cross");
        let manager = SessionManager::new(&root);
        let session = manager
            .create("/tmp/p", "o", "m", None)
            .await
            .expect("create");

        let err = manager
            .load(Path::new("/tmp/other"), &session.id)
            .await
            .expect_err("wrong pwd must not load");
        assert!(matches!(err, SessionError::Store(StoreError::NotFound(_))));

        let err = manager
            .load(Path::new("/tmp/p"), "not-a-real-id")
            .await
            .expect_err("wrong id must not load");
        assert!(matches!(err, SessionError::Store(StoreError::NotFound(_))));
        cleanup(&root);
    }

    #[tokio::test]
    async fn second_create_with_same_id_cannot_overwrite() {
        let root = temp_root("overwrite");
        let manager = SessionManager::new(&root);
        let pwd = "/tmp/p";

        let session = manager
            .create(pwd, "o", "m", None)
            .await
            .expect("first create");
        let before = std::fs::read_to_string(session_file(&root, pwd, &session.id)).unwrap();

        // Simulate a second create racing onto the same id: the exclusive
        // file creation must refuse rather than truncate the existing file.
        let mut collision = Session::new(pwd, "o", "m", None);
        collision.id = session.id.clone();
        collision.created_at_ms = 123_456;
        collision.updated_at_ms = 123_456;
        let err = manager
            .write_session_file_for_test(&collision)
            .await
            .expect_err("collision refused");
        assert!(matches!(
            err,
            SessionError::Store(StoreError::AlreadyExists(_))
        ));

        let after = std::fs::read_to_string(session_file(&root, pwd, &session.id)).unwrap();
        assert_eq!(before, after, "existing session untouched");
        cleanup(&root);
    }

    #[tokio::test]
    async fn load_validates_header_id_and_schema_and_pwd() {
        let root = temp_root("header");
        let manager = SessionManager::new(&root);
        let pwd = "/tmp/p";
        let session = manager.create(pwd, "o", "m", None).await.expect("create");
        let path = session_file(&root, pwd, &session.id);

        // Wrong schema version.
        let content = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            content.replace(r#""version":1,"#, r#""version":99,"#),
        )
        .unwrap();
        let err = manager
            .load(Path::new(pwd), &session.id)
            .await
            .expect_err("unsupported version rejected");
        assert!(matches!(err, SessionError::UnsupportedVersion { .. }));

        // Mismatched header id (fresh file).
        let session = manager.create(pwd, "o", "m", None).await.expect("create");
        let path = session_file(&root, pwd, &session.id);
        let content = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, content.replace(&session.id, "other-id")).unwrap();
        let err = manager
            .load(Path::new(pwd), &session.id)
            .await
            .expect_err("mismatched header id rejected");
        assert!(matches!(err, SessionError::InvalidHeader { .. }));

        // Mismatched header pwd / cross-directory load (fresh file).
        let session = manager.create(pwd, "o", "m", None).await.expect("create");
        let path = session_file(&root, pwd, &session.id);
        let content = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, content.replace(pwd, "/elsewhere")).unwrap();
        let err = manager
            .load(Path::new(pwd), &session.id)
            .await
            .expect_err("mismatched header pwd rejected");
        assert!(matches!(err, SessionError::InvalidHeader { .. }));
        cleanup(&root);
    }

    #[tokio::test]
    async fn list_returns_summaries_without_loading_messages() {
        let root = temp_root("list");
        let manager = SessionManager::new(&root);

        let older = manager
            .create("/tmp/p", "openrouter", "m", Some(ReasoningEffort::Low))
            .await
            .expect("older");
        manager
            .append_message(&older, &AgentMessage::user("hi"))
            .await
            .expect("append");
        manager
            .save_usage(&older, &usage(3, 4))
            .await
            .expect("usage");

        let newer = manager
            .create("/tmp/p", "openrouter", "m2", None)
            .await
            .expect("newer");

        let summaries = manager.list(Path::new("/tmp/p")).await.expect("list");
        assert_eq!(summaries.len(), 2);
        // Newest first; ids break ties deterministically for same-ms creates.
        assert!(
            (summaries[0].updated_at_ms, summaries[1].id.clone())
                >= (summaries[1].updated_at_ms, summaries[0].id.clone())
        );

        let summary = summaries
            .iter()
            .find(|summary| summary.id == older.id)
            .expect("older summary");
        assert_eq!(summary.pwd, older.pwd);
        assert_eq!(summary.provider, "openrouter");
        assert_eq!(summary.model, "m");
        assert_eq!(summary.thinking_level, Some(ReasoningEffort::Low));
        assert_eq!(summary.created_at_ms, older.created_at_ms);
        assert_eq!(summary.usage, usage(3, 4));

        let _ = newer;
        assert!(
            manager
                .list(Path::new("/tmp/none"))
                .await
                .expect("list")
                .is_empty()
        );
        cleanup(&root);
    }

    #[tokio::test]
    async fn append_to_missing_session_fails() {
        let root = temp_root("missing-append");
        let manager = SessionManager::new(&root);
        let session = Session::new("/tmp/p", "o", "m", None);

        let err = manager
            .append_message(&session, &AgentMessage::user("hi"))
            .await
            .expect_err("append without create fails");
        assert!(matches!(err, SessionError::Store(StoreError::NotFound(_))));
        cleanup(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_permissions_are_restrictive() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root("perms");
        let manager = SessionManager::new(&root);
        let session = manager
            .create("/tmp/p", "o", "m", None)
            .await
            .expect("create");

        let pwd_dir = root.join(pwd_key(Path::new("/tmp/p")));
        let dir_mode = std::fs::metadata(&pwd_dir).unwrap().permissions().mode();
        let file_mode = std::fs::metadata(session_file(&root, "/tmp/p", &session.id))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700, "directory mode");
        assert_eq!(file_mode & 0o777, 0o600, "session file mode");
        cleanup(&root);
    }

    /// Test-only re-entry into the create path used to verify collision
    /// handling without relying on UUID luck.
    impl SessionManager {
        async fn write_session_file_for_test(&self, session: &Session) -> Result<(), SessionError> {
            let key = pwd_key(&session.pwd);
            let header =
                session
                    .header_record()
                    .to_jsonl()
                    .map_err(|source| SessionError::Serialize {
                        path: self.store.file_path(&key, &session.id),
                        source,
                    })?;
            Ok(self.store.create_file(&key, &session.id, &header).await?)
        }
    }
}
