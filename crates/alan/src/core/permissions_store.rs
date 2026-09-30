use crate::core::paths;
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

    /// Append every grant in one read-modify-write, so a compound approval
    /// costs one lock, read, parse and write. Duplicates collapse.
    pub async fn insert_all(&self, grants: &[Grant]) -> Result<()> {
        self.modify(|set| {
            let mut added = false;
            for grant in grants {
                added |= set.insert(grant.clone());
            }
            added
        })
        .await
        .map(|_| ())
    }

    /// Remove `grant` from the persisted allow list. Returns `true` when the
    /// grant existed.
    #[cfg(test)]
    pub async fn remove(&self, grant: &Grant) -> Result<bool> {
        self.modify(|grants| grants.remove(grant)).await
    }

    /// Read-modify-write the allow list under a single store lock.
    async fn modify<F>(&self, f: F) -> Result<bool>
    where
        F: FnOnce(&mut HashSet<Grant>) -> bool,
    {
        self.store
            .update(ALLOW_KEY, |previous| {
                let mut grants: HashSet<Grant> = match previous.clone() {
                    Some(value) => serde_json::from_value::<Vec<Grant>>(value)
                        .with_context(|| {
                            format!("failed to parse grants in {}", self.store.path().display())
                        })?
                        .into_iter()
                        .collect(),
                    None => HashSet::new(),
                };

                // `update` reads an unchanged value as "nothing to write";
                // `None` would ask it to drop the key.
                if !f(&mut grants) {
                    return Ok((previous, false));
                }

                let mut grants: Vec<_> = grants.into_iter().collect();
                grants.sort_by(|a, b| (&a.command, &a.args).cmp(&(&b.command, &b.args)));

                Ok((Some(serde_json::to_value(grants)?), true))
            })
            .await
    }
}

/// Default location for the current project's permission file:
/// `<data dir>/projects/<slug(cwd)>/permissions.json`.
pub fn default_permissions_path(cwd: &Path) -> Result<PathBuf> {
    Ok(paths::alan_data_dir()?
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
        store
            .insert_all(&[grant("bun", Some("dev"))])
            .await
            .unwrap();
        assert!(path.exists());
        let loaded = store.load_all().await.unwrap();
        assert_eq!(loaded, HashSet::from([grant("bun", Some("dev"))]));
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[tokio::test]
    async fn round_trips_family_and_exact_grants() {
        let (store, path) = temp_store("roundtrip");
        store.insert_all(&[grant("cargo", None)]).await.unwrap();
        store
            .insert_all(&[grant("bun", Some("dev"))])
            .await
            .unwrap();
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
        store.insert_all(std::slice::from_ref(&g)).await.unwrap();
        store.insert_all(std::slice::from_ref(&g)).await.unwrap();
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["allow"].as_array().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[tokio::test]
    async fn insert_all_writes_the_whole_set_once() {
        // One batch is one write: no intermediate state on disk.
        let (store, path) = temp_store("batch");
        store
            .insert_all(&[
                grant("rg", Some("x")),
                grant("cargo", Some("test")),
                grant("cargo", Some("test")),
            ])
            .await
            .unwrap();
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // The duplicate in the batch collapsed.
        assert_eq!(raw["allow"].as_array().unwrap().len(), 2);
        assert_eq!(
            store.load_all().await.unwrap(),
            HashSet::from([grant("rg", Some("x")), grant("cargo", Some("test"))])
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[tokio::test]
    async fn insert_all_appends_to_existing_entries() {
        let (store, path) = temp_store("batch-append");
        store.insert_all(&[grant("bun", None)]).await.unwrap();
        store
            .insert_all(&[grant("bun", None), grant("cargo", None)])
            .await
            .unwrap();
        assert_eq!(
            store.load_all().await.unwrap(),
            HashSet::from([grant("bun", None), grant("cargo", None)])
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[tokio::test]
    async fn insert_appends_without_touching_others() {
        let (store, path) = temp_store("append");
        store.insert_all(&[grant("bun", None)]).await.unwrap();
        store.insert_all(&[grant("cargo", None)]).await.unwrap();
        let loaded = store.load_all().await.unwrap();
        assert!(loaded.contains(&grant("bun", None)));
        assert!(loaded.contains(&grant("cargo", None)));
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[tokio::test]
    async fn remove_deletes_only_target() {
        let (store, path) = temp_store("remove");
        store.insert_all(&[grant("bun", None)]).await.unwrap();
        store.insert_all(&[grant("cargo", None)]).await.unwrap();

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
        store.insert_all(&[grant("bun", None)]).await.unwrap();
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
        assert!(store.insert_all(&[grant("bun", None)]).await.is_err());
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
                match store.insert_all(std::slice::from_ref(&grant)).await {
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
