use crate::core::permissions::Grant;
use crate::core::store::JsonStore;
use agent::pwd_key;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Key under which the allow-grant list is stored in the backing file.
const ALLOW_KEY: &str = "allow";

/// Per-project allow-grant store backed by a [`JsonStore`].
pub struct PermissionStore {
    store: JsonStore,
}

impl PermissionStore {
    /// Create a store handle for `path`. Performs no I/O; the file is read on
    /// demand.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            store: JsonStore::new(path),
        }
    }

    /// Load every persisted allow grant. Missing file => empty set.
    pub async fn load_all(&self) -> Result<HashSet<Grant>> {
        let Some(value) = self.store.get(ALLOW_KEY).await? else {
            return Ok(HashSet::new());
        };
        let grants: Vec<Grant> = serde_json::from_value(value).with_context(|| {
            format!("failed to parse grants in {}", self.store.path().display())
        })?;
        Ok(grants.into_iter().collect())
    }

    /// Append `grant` to the persisted allow list. Re-inserting an existing
    /// grant is a no-op.
    pub async fn insert(&self, grant: &Grant) -> Result<()> {
        self.modify(|grants| grants.insert(grant.clone()))
            .await
            .map(|_| ())
    }

    /// Remove `grant` from the persisted allow list. Returns `true` when the
    /// grant existed.
    #[cfg(test)]
    pub async fn remove(&self, grant: &Grant) -> Result<bool> {
        self.modify(|grants| grants.remove(grant)).await
    }

    /// Read-modify-write the allow list; writes only when `f` changed it.
    async fn modify<F>(&self, f: F) -> Result<bool>
    where
        F: FnOnce(&mut HashSet<Grant>) -> bool,
    {
        let mut grants: HashSet<Grant> = match self.store.get(ALLOW_KEY).await? {
            Some(value) => serde_json::from_value::<Vec<Grant>>(value)
                .with_context(|| {
                    format!("failed to parse grants in {}", self.store.path().display())
                })?
                .into_iter()
                .collect(),
            None => HashSet::new(),
        };

        if !f(&mut grants) {
            return Ok(false);
        }

        let mut grants: Vec<_> = grants.into_iter().collect();
        grants.sort_by(|a, b| (&a.command, &a.args).cmp(&(&b.command, &b.args)));

        self.store
            .set(ALLOW_KEY, serde_json::to_value(grants)?)
            .await?;

        Ok(true)
    }
}

/// Default location for the current project's permission file:
/// `$ALAN_HOME/projects/<slug(cwd)>/permissions.json`, falling back to `$HOME`.
pub fn default_permissions_path(cwd: &Path) -> Result<PathBuf> {
    let home = std::env::var_os("ALAN_HOME")
        .or_else(|| std::env::var_os("HOME"))
        .ok_or_else(|| anyhow::anyhow!("cannot determine Alan home directory"))?;

    Ok(PathBuf::from(home)
        .join(".alan")
        .join("projects")
        .join(pwd_key(cwd))
        .join("permissions.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEMP_DIR_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "alan-permissions-test-{}-{name}",
            std::process::id()
        ));

        let path = path.join(format!(
            "{}",
            TEMP_DIR_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn grant(command: &str, args: Option<&str>) -> Grant {
        Grant {
            command: command.into(),
            args: args.map(Into::into),
        }
    }

    fn temp_store(name: &str) -> (PermissionStore, PathBuf) {
        let dir = temp_dir(name);
        let path = dir.join("permissions.json");
        (PermissionStore::new(&path), path)
    }

    #[tokio::test]
    async fn load_missing_file_returns_empty_without_creating_it() {
        let (store, path) = temp_store("missing");
        assert!(store.load_all().await.unwrap().is_empty());
        assert!(!path.exists(), "reading must not create the file");
        assert!(!store.remove(&grant("bun", None)).await.unwrap());
        assert!(!path.exists(), "removing must not create the file");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn insert_creates_file_and_dirs() {
        let (store, path) = temp_store("create");
        store.insert(&grant("bun", Some("dev"))).await.unwrap();
        assert!(path.exists());
        let loaded = store.load_all().await.unwrap();
        assert_eq!(loaded, HashSet::from([grant("bun", Some("dev"))]));
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[tokio::test]
    async fn round_trips_family_and_exact_grants() {
        let (store, path) = temp_store("roundtrip");
        store.insert(&grant("cargo", None)).await.unwrap();
        store.insert(&grant("bun", Some("dev"))).await.unwrap();
        let loaded = store.load_all().await.unwrap();
        assert_eq!(
            loaded,
            HashSet::from([grant("cargo", None), grant("bun", Some("dev"))])
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[tokio::test]
    async fn reinsert_is_a_noop() {
        let (store, path) = temp_store("dedup");
        let g = grant("bun", Some("dev"));
        store.insert(&g).await.unwrap();
        store.insert(&g).await.unwrap();
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["allow"].as_array().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[tokio::test]
    async fn insert_appends_without_touching_others() {
        let (store, path) = temp_store("append");
        store.insert(&grant("bun", None)).await.unwrap();
        store.insert(&grant("cargo", None)).await.unwrap();
        let loaded = store.load_all().await.unwrap();
        assert!(loaded.contains(&grant("bun", None)));
        assert!(loaded.contains(&grant("cargo", None)));
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[tokio::test]
    async fn remove_deletes_only_target() {
        let (store, path) = temp_store("remove");
        store.insert(&grant("bun", None)).await.unwrap();
        store.insert(&grant("cargo", None)).await.unwrap();

        assert!(store.remove(&grant("bun", None)).await.unwrap());
        // Removing again reports false.
        assert!(!store.remove(&grant("bun", None)).await.unwrap());

        let loaded = store.load_all().await.unwrap();
        assert_eq!(loaded, HashSet::from([grant("cargo", None)]));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn removing_last_grant_leaves_empty_allow_list() {
        let (store, path) = temp_store("remove-last");
        store.insert(&grant("bun", None)).await.unwrap();
        assert!(store.remove(&grant("bun", None)).await.unwrap());
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["allow"].as_array().unwrap().len(), 0);
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[tokio::test]
    async fn corrupt_file_is_an_error() {
        let dir = temp_dir("corrupt");
        let path = dir.join("permissions.json");
        tokio::fs::write(&path, "not json at all").await.unwrap();
        let store = PermissionStore::new(&path);
        assert!(store.load_all().await.is_err());
        assert!(store.insert(&grant("bun", None)).await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn non_object_file_is_an_error() {
        let dir = temp_dir("non-object");
        let path = dir.join("permissions.json");
        tokio::fs::write(&path, "[1, 2, 3]").await.unwrap();
        let store = PermissionStore::new(&path);
        assert!(store.load_all().await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn concurrent_inserts_both_survive() {
        let dir = temp_dir("concurrent");
        let path = dir.join("permissions.json");
        let store = std::sync::Arc::new(PermissionStore::new(&path));
        let store2 = store.clone();
        // JsonStore's lock is try-based, so contended writers retry.
        let insert_with_retry = |store: std::sync::Arc<PermissionStore>, grant: Grant| async move {
            loop {
                match store.insert(&grant).await {
                    Ok(()) => return,
                    Err(_) => tokio::time::sleep(std::time::Duration::from_millis(5)).await,
                }
            }
        };
        let (_, _) = tokio::join!(
            insert_with_retry(store.clone(), grant("bun", None)),
            insert_with_retry(store2, grant("cargo", None)),
        );
        let loaded = store.load_all().await.unwrap();
        assert_eq!(loaded.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
