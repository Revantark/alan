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
    ApiKey { key: String },
}

impl fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credential")
            .field(
                "type",
                &match self {
                    Self::ApiKey { .. } => "api_key",
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
            Ok(contents) => serde_json::from_str(&contents).map_err(CredentialError::Serialization),
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

#[cfg(not(unix))]
fn set_unix_mode(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

pub type SharedCredentialStore = Arc<dyn CredentialStore>;
