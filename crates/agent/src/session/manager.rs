use super::error::SessionError;
use super::record::{SESSION_SCHEMA_VERSION, Session, SessionRecord, now_ms};
use super::store::StoreError;
use super::store::{JSONL_EXTENSION, JsonlStore, set_permissions, split_complete_lines};
use crate::context::AgentMessage;
use crate::session::dir::pwd_key;
use llm::{ReasoningEffort, Usage};
use std::path::{Path, PathBuf};

fn normalize(pwd: PathBuf) -> Result<PathBuf, SessionError> {
    let dir = pwd.clone();
    super::dir::normalize_pwd(pwd)
        .map_err(|source| SessionError::Store(StoreError::CreateDir { dir, source }))
}

/// Filesystem-backed, append-only sessions.
pub struct SessionManager {
    root: PathBuf,
}

impl SessionManager {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn file_path(&self, key: &str, name: &str) -> PathBuf {
        self.root
            .join(key)
            .join(format!("{name}.{JSONL_EXTENSION}"))
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
        reasoning_effort: ReasoningEffort,
    ) -> Result<Session, SessionError> {
        let pwd = normalize(pwd.into())?;
        let key = pwd_key(&pwd);
        let session = Session::new(pwd, provider, model, reasoning_effort);
        validate_session_id(&session.id)?;
        let path = self.file_path(&key, &session.id);
        // `file_path` always builds `root/key/name.jsonl`, so parent is
        // the key directory.
        let dir = match path.parent() {
            Some(dir) => dir.to_path_buf(),
            None => unreachable!("file_path always builds root/key/name.jsonl"),
        };

        // Builds the root as well as the key directory, so both have to be
        // narrowed afterwards rather than before.
        tokio::fs::create_dir_all(&dir).await.map_err(|source| {
            SessionError::Store(StoreError::CreateDir {
                dir: dir.clone(),
                source,
            })
        })?;
        set_permissions(&self.root, true).await?;
        set_permissions(&dir, true).await?;

        let header =
            session
                .header_record()
                .to_jsonl()
                .map_err(|source| SessionError::Serialize {
                    path: path.clone(),
                    source,
                })?;
        JsonlStore::create(&path, &header).await?;

        Ok(session)
    }

    pub async fn append_message(
        &self,
        session_id: &str,
        pwd: &Path,
        message: &AgentMessage,
    ) -> Result<(), SessionError> {
        let record = SessionRecord::Message {
            message: message.clone(),
            timestamp_ms: now_ms(),
        };
        self.append_record(session_id, pwd, &record).await
    }

    pub async fn append_usage(
        &self,
        session_id: &str,
        pwd: &Path,
        usage: &Usage,
    ) -> Result<(), SessionError> {
        let record = SessionRecord::Usage {
            usage: usage.clone(),
            timestamp_ms: now_ms(),
        };
        self.append_record(session_id, pwd, &record).await
    }

    pub async fn append_model(
        &self,
        session_id: &str,
        pwd: &Path,
        provider: impl Into<String>,
        model: impl Into<String>,
        reasoning_effort: ReasoningEffort,
    ) -> Result<SessionRecord, SessionError> {
        let record = SessionRecord::Model {
            provider: provider.into(),
            model: model.into(),
            reasoning_effort,
            timestamp_ms: now_ms(),
        };

        self.append_record(session_id, pwd, &record).await?;
        Ok(record)
    }

    pub async fn get_session(&self, session_id: &str, pwd: &Path) -> Result<Session, SessionError> {
        validate_session_id(session_id)?;
        let normalized = normalize(pwd.to_path_buf())?;
        let path = self.file_path(&pwd_key(&normalized), session_id);
        let content = JsonlStore::read(&path).await?;

        let mut lines = split_complete_lines(&content);

        let first_line =
            lines
                .next()
                .map(|(line, _)| line)
                .ok_or_else(|| SessionError::InvalidHeader {
                    path: path.clone(),
                    reason: "file is empty".into(),
                })?;
        let mut session = parse_header(first_line, &path, session_id, &normalized)?;

        let capacity = content
            .bytes()
            .filter(|b| *b == b'\n')
            .count()
            .saturating_sub(1);
        session.messages.reserve(capacity);

        for (line, complete) in lines {
            if !complete {
                continue;
            }
            let record =
                SessionRecord::parse(line).map_err(|err| SessionError::MalformedRecord {
                    path: path.clone(),
                    reason: err.to_string(),
                })?;
            if matches!(&record, SessionRecord::Session { .. }) {
                return Err(SessionError::MalformedRecord {
                    path: path.clone(),
                    reason: "unexpected session header inside file".into(),
                });
            }
            session.record(record);
        }

        Ok(session)
    }

    async fn append_record(
        &self,
        session_id: &str,
        pwd: &Path,
        record: &SessionRecord,
    ) -> Result<(), SessionError> {
        validate_session_id(session_id)?;
        let key = pwd_key(pwd);
        let path = self.file_path(&key, session_id);
        let line = record
            .to_jsonl()
            .map_err(|source| SessionError::Serialize {
                path: path.clone(),
                source,
            })?;
        Ok(JsonlStore::append(&path, &line).await?)
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

/// Parse the first JSONL line as a session header and validate it matches the
/// expected id and pwd. Returns a ready-to-use [`Session`] with an empty
/// messages vec.
fn parse_header(
    line: &str,
    path: &Path,
    expected_id: &str,
    normalized_pwd: &Path,
) -> Result<Session, SessionError> {
    let record = SessionRecord::parse(line).map_err(|err| SessionError::InvalidHeader {
        path: path.to_path_buf(),
        reason: err.to_string(),
    })?;

    let SessionRecord::Session {
        id,
        version,
        pwd,
        provider,
        model,
        reasoning_effort,
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
    if id != expected_id {
        return Err(SessionError::InvalidHeader {
            path: path.to_path_buf(),
            reason: format!("header id {id:?} does not match requested id {expected_id:?}"),
        });
    }
    if normalize(pwd.clone())? != normalized_pwd {
        return Err(SessionError::InvalidHeader {
            path: path.to_path_buf(),
            reason: format!(
                "header pwd {:?} does not match requested pwd {:?}",
                pwd.display(),
                normalized_pwd.display()
            ),
        });
    }

    Ok(Session {
        id,
        version,
        pwd,
        provider,
        model,
        reasoning_effort,
        messages: Vec::new(),
        usage: Usage::default(),
        created_at_ms,
        updated_at_ms: updated_at_ms.max(created_at_ms),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::AgentMessage;

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
    async fn create_writes_single_header_file_under_pwd_dir() {
        let root = temp_root("create");
        let manager = SessionManager::new(&root);

        let session = manager
            .create(
                "/tmp/project",
                "openrouter",
                "test-model",
                ReasoningEffort::Auto,
            )
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

    /// The root is created on demand, so the first session on a machine that
    /// has never run one starts without it.
    #[tokio::test]
    async fn create_makes_a_missing_root() {
        let root =
            std::env::temp_dir().join(format!("alan-session-fresh-{}", uuid::Uuid::new_v4()));
        assert!(!root.exists(), "root must not exist yet");
        let manager = SessionManager::new(&root);

        manager
            .create(
                "/tmp/project",
                "openrouter",
                "test-model",
                ReasoningEffort::Auto,
            )
            .await
            .expect("create session without a pre-existing root");

        assert!(root.is_dir(), "create built the root");
        // Narrowing it is the reason the call exists, so a root built here
        // has to end up as private as one that already existed.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&root)
                .expect("root metadata")
                .permissions();
            assert_eq!(mode.mode() & 0o777, 0o700, "root mode");
        }
        cleanup(&root);
    }

    #[tokio::test]
    async fn different_pwds_use_different_directories() {
        let root = temp_root("pwds");
        let manager = SessionManager::new(&root);

        manager
            .create("/tmp/a", "openrouter", "m", ReasoningEffort::Auto)
            .await
            .expect("create a");
        manager
            .create("/tmp/b", "openrouter", "m", ReasoningEffort::Auto)
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
                ReasoningEffort::High,
            )
            .await
            .expect("create");

        manager
            .append_message(&session.id, &session.pwd, &AgentMessage::user("hello"))
            .await
            .expect("append user");
        manager
            .append_message(&session.id, &session.pwd, &AgentMessage::user("second"))
            .await
            .expect("append second");
        manager
            .append_usage(&session.id, &session.pwd, &usage(10, 5))
            .await
            .expect("usage 1");
        manager
            .append_usage(&session.id, &session.pwd, &usage(25, 8))
            .await
            .expect("usage 2");

        let loaded = manager
            .get_session(&session.id, Path::new("/tmp/project"))
            .await
            .expect("load");

        assert_eq!(loaded.id, session.id);
        assert_eq!(loaded.version, SESSION_SCHEMA_VERSION);
        assert_eq!(loaded.pwd, session.pwd);
        assert_eq!(loaded.provider, "openrouter");
        assert_eq!(loaded.model, "test-model");
        assert_eq!(loaded.reasoning_effort, ReasoningEffort::High);
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
            .create("/tmp/p", "o", "m", ReasoningEffort::Auto)
            .await
            .expect("create");

        manager
            .append_usage(&session.id, &session.pwd, &usage(10, 5))
            .await
            .expect("usage 1");
        manager
            .append_usage(&session.id, &session.pwd, &usage(20, 7))
            .await
            .expect("usage 2");

        let loaded = manager
            .get_session(&session.id, Path::new("/tmp/p"))
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
            .create("/tmp/p", "o", "m", ReasoningEffort::Auto)
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
            .get_session(&session.id, Path::new("/tmp/p"))
            .await
            .expect("truncated tail tolerated");
        assert_eq!(loaded.messages.len(), 1);

        // A malformed complete line is an error, even at the end.
        content.push_str("\nthis is not json\n");
        std::fs::write(&path, &content).unwrap();

        let err = manager
            .get_session(&session.id, Path::new("/tmp/p"))
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
            .create("/tmp/p", "o", "m", ReasoningEffort::Auto)
            .await
            .expect("create");

        let err = manager
            .get_session(&session.id, Path::new("/tmp/other"))
            .await
            .expect_err("wrong pwd must not load");
        assert!(matches!(err, SessionError::Store(StoreError::NotFound(_))));

        let err = manager
            .get_session("not-a-real-id", Path::new("/tmp/p"))
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
            .create(pwd, "o", "m", ReasoningEffort::Auto)
            .await
            .expect("first create");
        let before = std::fs::read_to_string(session_file(&root, pwd, &session.id)).unwrap();

        // Simulate a second create racing onto the same id: the exclusive
        // file creation must refuse rather than truncate the existing file.
        let mut collision = Session::new(pwd, "o", "m", ReasoningEffort::Auto);
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
        let session = manager
            .create(pwd, "o", "m", ReasoningEffort::Auto)
            .await
            .expect("create");
        let path = session_file(&root, pwd, &session.id);

        // Wrong schema version.
        let content = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            content.replace(r#""version":1,"#, r#""version":99,"#),
        )
        .unwrap();
        let err = manager
            .get_session(&session.id, Path::new(pwd))
            .await
            .expect_err("unsupported version rejected");
        assert!(matches!(err, SessionError::UnsupportedVersion { .. }));

        // Mismatched header id (fresh file).
        let session = manager
            .create(pwd, "o", "m", ReasoningEffort::Auto)
            .await
            .expect("create");
        let path = session_file(&root, pwd, &session.id);
        let content = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, content.replace(&session.id, "other-id")).unwrap();
        let err = manager
            .get_session(&session.id, Path::new(pwd))
            .await
            .expect_err("mismatched header id rejected");
        assert!(matches!(err, SessionError::InvalidHeader { .. }));

        // Mismatched header pwd / cross-directory load (fresh file).
        let session = manager
            .create(pwd, "o", "m", ReasoningEffort::Auto)
            .await
            .expect("create");
        let path = session_file(&root, pwd, &session.id);
        let content = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, content.replace(pwd, "/elsewhere")).unwrap();
        let err = manager
            .get_session(&session.id, Path::new(pwd))
            .await
            .expect_err("mismatched header pwd rejected");
        assert!(matches!(err, SessionError::InvalidHeader { .. }));
        cleanup(&root);
    }

    #[tokio::test]
    async fn append_to_missing_session_fails() {
        let root = temp_root("missing-append");
        let manager = SessionManager::new(&root);
        let session = Session::new("/tmp/p", "o", "m", ReasoningEffort::Auto);

        let err = manager
            .append_message(&session.id, &session.pwd, &AgentMessage::user("hi"))
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
            .create("/tmp/p", "o", "m", ReasoningEffort::Auto)
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
            let path = self.file_path(&key, &session.id);
            let header =
                session
                    .header_record()
                    .to_jsonl()
                    .map_err(|source| SessionError::Serialize {
                        path: path.clone(),
                        source,
                    })?;
            set_permissions(&self.root, true).await?;
            Ok(JsonlStore::create(&path, &header).await?)
        }
    }
}
