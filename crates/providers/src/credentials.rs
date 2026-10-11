use crate::ProviderId;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Credential {
    ApiKey {
        key: String,
    },
    /// Generic OAuth credentials. Provider-specific claims (for example a
    /// ChatGPT account id) live in `metadata`; secret material there is
    /// shielded by the manual `Debug` impl below.
    OAuth {
        access_token: String,
        refresh_token: String,
        id_token: String,
        /// Unix timestamp (seconds) after which the access token must be
        /// refreshed.
        expires_at: u64,
        metadata: BTreeMap<String, String>,
    },
}

impl fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credential")
            .field(
                "type",
                &match self {
                    Self::ApiKey { .. } => "api_key",
                    Self::OAuth { .. } => "oauth",
                },
            )
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialInfo {
    pub provider: ProviderId,
    pub kind: CredentialKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    ApiKey,
    OAuth,
}

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("credential storage I/O failed: {0}")]
    Io(#[source] std::io::Error),
    #[error("credential storage contains invalid data: {0}")]
    Serialization(#[source] serde_json::Error),
    #[error("credential storage lock is busy")]
    LockBusy,
    #[error("credential storage lock was interrupted")]
    LockInterrupted,
}

#[async_trait]
pub trait CredentialStore: Send + Sync {
    async fn read(&self, provider: &ProviderId) -> Result<Option<Credential>, CredentialError>;

    async fn list(&self) -> Result<Vec<CredentialInfo>, CredentialError>;

    async fn put(
        &self,
        provider: &ProviderId,
        credential: Credential,
    ) -> Result<(), CredentialError>;

    async fn delete(&self, provider: &ProviderId) -> Result<(), CredentialError>;
}

#[derive(Default)]
pub struct InMemoryCredentialStore {
    credentials: Mutex<BTreeMap<String, Credential>>,
}

impl InMemoryCredentialStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl CredentialStore for InMemoryCredentialStore {
    async fn read(&self, provider: &ProviderId) -> Result<Option<Credential>, CredentialError> {
        Ok(self
            .credentials
            .lock()
            .expect("credential store mutex poisoned")
            .get(&provider.0)
            .cloned())
    }

    async fn list(&self) -> Result<Vec<CredentialInfo>, CredentialError> {
        Ok(self
            .credentials
            .lock()
            .expect("credential store mutex poisoned")
            .iter()
            .map(|(provider, credential)| CredentialInfo {
                provider: ProviderId::new(provider),
                kind: match credential {
                    Credential::ApiKey { .. } => CredentialKind::ApiKey,
                    Credential::OAuth { .. } => CredentialKind::OAuth,
                },
            })
            .collect())
    }

    async fn put(
        &self,
        provider: &ProviderId,
        credential: Credential,
    ) -> Result<(), CredentialError> {
        self.credentials
            .lock()
            .expect("credential store mutex poisoned")
            .insert(provider.0.clone(), credential);
        Ok(())
    }

    async fn delete(&self, provider: &ProviderId) -> Result<(), CredentialError> {
        self.credentials
            .lock()
            .expect("credential store mutex poisoned")
            .remove(&provider.0);
        Ok(())
    }
}

pub struct FileCredentialStore {
    path: PathBuf,
    lock_path: PathBuf,
}

impl FileCredentialStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let lock_path = PathBuf::from(format!("{}.lock", path.display()));
        Self { path, lock_path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn ensure_parent(&self) -> Result<(), CredentialError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(CredentialError::Io)?;
            set_unix_mode(parent, 0o700).map_err(CredentialError::Io)?;
        }
        Ok(())
    }

    fn read_all(&self) -> Result<BTreeMap<String, Credential>, CredentialError> {
        match fs::read_to_string(&self.path) {
            Ok(contents) => {
                // Migrate before typing: pre-rename files carry the old
                // `chat_gpt_o_auth` tag and a flat `account_id` field, which
                // would otherwise fail to parse or be silently dropped.
                let mut entries: BTreeMap<String, serde_json::Value> =
                    serde_json::from_str(&contents).map_err(CredentialError::Serialization)?;
                for entry in entries.values_mut() {
                    migrate_legacy_oauth(entry);
                }
                entries
                    .into_iter()
                    .map(|(provider, entry)| {
                        let credential: Credential = serde_json::from_value(entry)
                            .map_err(CredentialError::Serialization)?;
                        Ok((provider, credential))
                    })
                    .collect()
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(error) => Err(CredentialError::Io(error)),
        }
    }

    fn with_lock<T>(
        &self,
        operation: impl FnOnce(&mut BTreeMap<String, Credential>) -> Result<T, CredentialError>,
    ) -> Result<T, CredentialError> {
        self.ensure_parent()?;
        let _lock = LockFile::acquire(&self.lock_path)?;
        let mut credentials = self.read_all()?;
        let result = operation(&mut credentials)?;
        let contents =
            serde_json::to_string_pretty(&credentials).map_err(CredentialError::Serialization)?;
        self.write_atomic(&contents)?;
        Ok(result)
    }

    fn write_atomic(&self, contents: &str) -> Result<(), CredentialError> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        let temp_path = parent.join(format!(".{}.tmp", std::process::id()));
        {
            let mut file = File::create(&temp_path).map_err(CredentialError::Io)?;
            set_unix_mode(&temp_path, 0o600).map_err(CredentialError::Io)?;
            use std::io::Write;
            file.write_all(contents.as_bytes())
                .map_err(CredentialError::Io)?;
            file.sync_all().map_err(CredentialError::Io)?;
        }
        fs::rename(&temp_path, &self.path).map_err(CredentialError::Io)?;
        set_unix_mode(&self.path, 0o600).map_err(CredentialError::Io)?;
        Ok(())
    }
}

#[async_trait]
impl CredentialStore for FileCredentialStore {
    async fn read(&self, provider: &ProviderId) -> Result<Option<Credential>, CredentialError> {
        self.ensure_parent()?;
        Ok(self.read_all()?.remove(&provider.0))
    }

    async fn list(&self) -> Result<Vec<CredentialInfo>, CredentialError> {
        self.ensure_parent()?;
        Ok(self
            .read_all()?
            .into_iter()
            .map(|(provider, credential)| CredentialInfo {
                provider: ProviderId::new(provider),
                kind: match credential {
                    Credential::ApiKey { .. } => CredentialKind::ApiKey,
                    Credential::OAuth { .. } => CredentialKind::OAuth,
                },
            })
            .collect())
    }

    async fn put(
        &self,
        provider: &ProviderId,
        credential: Credential,
    ) -> Result<(), CredentialError> {
        self.with_lock(|credentials| {
            credentials.insert(provider.0.clone(), credential);
            Ok(())
        })
    }

    async fn delete(&self, provider: &ProviderId) -> Result<(), CredentialError> {
        self.with_lock(|credentials| {
            credentials.remove(&provider.0);
            Ok(())
        })
    }
}

struct LockFile {
    path: PathBuf,
    _file: File,
}

impl LockFile {
    fn acquire(path: &Path) -> Result<Self, CredentialError> {
        for _ in 0..50 {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(file) => {
                    return Ok(Self {
                        path: path.to_owned(),
                        _file: file,
                    });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => return Err(CredentialError::Io(error)),
            }
        }
        Err(CredentialError::LockBusy)
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn set_unix_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

/// Migrate one legacy credential entry in place: files written before the
/// OAuth rename serialize `Credential::ChatGptOAuth` as
/// `{"type": "chat_gpt_o_auth", ..., "account_id": "..."}`; the current
/// variant is `"oauth"` with provider claims in a nested `metadata` map.
fn migrate_legacy_oauth(entry: &mut serde_json::Value) {
    let Some(object) = entry.as_object_mut() else {
        return;
    };
    let is_legacy = object
        .get("type")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|tag| tag == "chat_gpt_o_auth" || tag == "chatgpt_oauth");
    if !is_legacy {
        return;
    }
    object.insert(
        "type".to_string(),
        // serde's snake_case of the `OAuth` variant is `o_auth`.
        serde_json::Value::String("o_auth".to_string()),
    );
    let account_id = object.remove("account_id");
    if let Some(account_id) = account_id {
        let mut metadata = serde_json::Map::new();
        metadata.insert("account_id".to_string(), account_id);
        object.insert("metadata".to_string(), serde_json::Value::Object(metadata));
    }
}

#[cfg(not(unix))]
fn set_unix_mode(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

pub type SharedCredentialStore = Arc<dyn CredentialStore>;

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("alan-credentials-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn legacy_oauth_files_still_parse() {
        let dir = temp_dir("legacy-oauth");
        let path = dir.join("credentials.json");
        // Written by the pre-rename binary: serde's snake_case of the old
        // `ChatGptOAuth` variant.
        fs::write(
            &path,
            r#"{"chatgpt": {"type": "chat_gpt_o_auth", "access_token": "at", "refresh_token": "rt", "id_token": "it", "expires_at": 12345, "account_id": "acc"}}"#,
        )
        .unwrap();

        let store = FileCredentialStore::new(&path);
        let read = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(store.read(&ProviderId::new("chatgpt")))
            .unwrap();
        assert!(
            matches!(read, Some(Credential::OAuth { ref metadata, .. })
                if metadata.get("account_id").map(String::as_str) == Some("acc")),
            "legacy OAuth entry must still parse, got {read:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_api_key_files_still_parse() {
        let dir = temp_dir("legacy");
        let path = dir.join("credentials.json");
        fs::write(
            &path,
            r#"{"deepseek": {"type": "api_key", "key": "sk-old"}}"#,
        )
        .unwrap();

        let store = FileCredentialStore::new(&path);
        let read = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(store.read(&ProviderId::new("deepseek")))
            .unwrap();
        assert!(
            matches!(read, Some(Credential::ApiKey { ref key }) if key == "sk-old"),
            "legacy ApiKey entry must still parse, got {read:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    fn oauth_credential() -> Credential {
        Credential::OAuth {
            access_token: "at".into(),
            refresh_token: "rt".into(),
            id_token: "it".into(),
            expires_at: 12345,
            metadata: BTreeMap::from([("account_id".to_string(), "acc".to_string())]),
        }
    }

    #[test]
    fn oauth_credentials_round_trip_through_the_file() {
        let dir = temp_dir("oauth");
        let path = dir.join("credentials.json");
        let store = FileCredentialStore::new(&path);
        let runtime = tokio::runtime::Runtime::new().unwrap();

        runtime
            .block_on(store.put(&ProviderId::new("chatgpt"), oauth_credential()))
            .unwrap();
        let read = runtime
            .block_on(store.read(&ProviderId::new("chatgpt")))
            .unwrap();
        let Some(Credential::OAuth { metadata, .. }) = &read else {
            panic!("expected an OAuth credential, got {read:?}");
        };
        // Provider-specific metadata survives the round trip.
        assert_eq!(metadata.get("account_id").map(String::as_str), Some("acc"));

        // Both variants coexist in one file.
        runtime
            .block_on(store.put(
                &ProviderId::new("deepseek"),
                Credential::ApiKey { key: "sk".into() },
            ))
            .unwrap();
        let kinds = runtime.block_on(store.list()).unwrap();
        assert_eq!(kinds.len(), 2);
        assert!(kinds.iter().any(|info| info.kind == CredentialKind::OAuth));
        assert!(kinds.iter().any(|info| info.kind == CredentialKind::ApiKey));
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn file_is_written_owner_only() {
        let dir = temp_dir("mode");
        let path = dir.join("credentials.json");
        let store = FileCredentialStore::new(&path);
        let runtime = tokio::runtime::Runtime::new().unwrap();

        runtime
            .block_on(store.put(
                &ProviderId::new("chatgpt"),
                Credential::ApiKey { key: "k".into() },
            ))
            .unwrap();

        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn debug_never_shows_token_values() {
        let api_key = Credential::ApiKey {
            key: "super-secret-key".into(),
        };
        let oauth = Credential::OAuth {
            access_token: "secret-access".into(),
            refresh_token: "secret-refresh".into(),
            id_token: "secret-id".into(),
            expires_at: 1,
            metadata: BTreeMap::from([("account_id".to_string(), "secret-account".to_string())]),
        };
        let rendered = format!("{api_key:?} {oauth:?}");
        assert!(rendered.contains("api_key"));
        assert!(rendered.contains("oauth"));
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains("super-secret-key"));
    }
}
