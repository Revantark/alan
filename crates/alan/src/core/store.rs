//! Minimal JSON key-value store backed by a single file on disk.
#![allow(dead_code)]

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

/// A flat JSON object persisted at `path`.
#[derive(Debug)]
pub struct JsonStore {
    path: PathBuf,
}

impl JsonStore {
    /// Create a store handle for `path`. Performs no I/O; the file is read on
    /// demand.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Path of the backing file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the value stored under `key`, if any.
    pub async fn get(&self, key: &str) -> Result<Option<Value>> {
        let _lock = self.acquire_lock()?;
        let values = self.read().await?;
        Ok(values.get(key).cloned())
    }

    /// Store `value` under `key`, merging into the on-disk object.
    pub async fn set(&self, key: impl Into<String>, value: impl Into<Value>) -> Result<()> {
        let _lock = self.acquire_lock()?;
        let mut values = self.read().await?;
        values.insert(key.into(), value.into());
        self.write(&values).await
    }

    /// Remove `key`.
    ///
    /// Returns `true` when the key existed. Writes only when something changed.
    pub async fn remove(&self, key: &str) -> Result<bool> {
        let _lock = self.acquire_lock()?;
        let mut values = self.read().await?;
        if values.remove(key).is_some() {
            self.write(&values).await?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Whether `key` is present.
    pub async fn contains(&self, key: &str) -> Result<bool> {
        let _lock = self.acquire_lock()?;
        let values = self.read().await?;
        Ok(values.contains_key(key))
    }

    /// Acquire an exclusive lock guarding the backing file.
    ///
    /// A sidecar `<path>.lock` file is locked with `fs2` so concurrent
    /// processes serialise their read-modify-write cycles. The sidecar is
    /// created on demand; the backing file itself is never created by locking.
    fn acquire_lock(&self) -> Result<std::fs::File> {
        let mut lock_path = self.path.clone().into_os_string();
        lock_path.push(".lock");
        let lock_path = PathBuf::from(lock_path);
        if let Some(parent) = lock_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("failed to open lock file {}", lock_path.display()))?;
        fs2::FileExt::try_lock_exclusive(&file).map_err(|source| {
            let contended = source.kind() == std::io::ErrorKind::WouldBlock
                || source.raw_os_error() == fs2::lock_contended_error().raw_os_error();
            if contended {
                anyhow::anyhow!(
                    "json store {} is locked by another process",
                    self.path.display()
                )
            } else {
                anyhow::anyhow!(
                    "failed to lock json store {}: {source}",
                    self.path.display()
                )
            }
        })?;
        Ok(file)
    }

    /// Read and parse the backing file into a map. Missing file => empty map.
    async fn read(&self) -> Result<BTreeMap<String, Value>> {
        match tokio::fs::read_to_string(&self.path).await {
            Ok(contents) => parse_object(&contents)
                .with_context(|| format!("failed to parse json store {}", self.path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(err) => Err(err)
                .with_context(|| format!("failed to read json store {}", self.path.display())),
        }
    }

    /// Serialize `values` to the backing file, creating parent dirs as needed.
    async fn write(&self, values: &BTreeMap<String, Value>) -> Result<()> {
        let json =
            serde_json::to_string_pretty(values).context("failed to serialize json store")?;
        if let Some(parent) = self.path.parent().filter(|p| !p.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }
        tokio::fs::write(&self.path, json)
            .await
            .with_context(|| format!("failed to write json store {}", self.path.display()))
    }
}

fn parse_object(contents: &str) -> Result<BTreeMap<String, Value>> {
    match serde_json::from_str::<Value>(contents)? {
        Value::Object(map) => Ok(map.into_iter().collect()),
        _ => anyhow::bail!("expected a json object"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "alan-store-test-{}-{name}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[tokio::test]
    async fn missing_file_opens_empty() {
        let path = temp_path("missing");
        let store = JsonStore::new(&path);
        assert!(!store.contains("model").await.unwrap());
        assert_eq!(store.get("model").await.unwrap(), None);
        assert!(!path.exists(), "reading must not create the file");
    }

    #[tokio::test]
    async fn set_get_remove_roundtrip() {
        let path = temp_path("roundtrip");
        let store = JsonStore::new(&path);

        store.set("model", json!("gpt-4o-mini")).await.unwrap();
        store.set("web_fetch", json!(true)).await.unwrap();
        assert_eq!(
            store.get("model").await.unwrap(),
            Some(json!("gpt-4o-mini"))
        );
        assert!(store.contains("web_fetch").await.unwrap());

        // A fresh handle reads the same file back from disk.
        let reloaded = JsonStore::new(&path);
        assert_eq!(
            reloaded.get("model").await.unwrap(),
            Some(json!("gpt-4o-mini"))
        );
        assert_eq!(reloaded.get("web_fetch").await.unwrap(), Some(json!(true)));

        assert!(reloaded.remove("model").await.unwrap());
        assert!(!reloaded.remove("model").await.unwrap());
        assert_eq!(reloaded.get("model").await.unwrap(), None);

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn invalid_content_is_an_error() {
        let path = temp_path("invalid");
        tokio::fs::write(&path, "[1, 2, 3]").await.unwrap();
        let store = JsonStore::new(&path);
        assert!(store.get("model").await.is_err());
        let _ = std::fs::remove_file(&path);
    }
}
