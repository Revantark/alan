use crate::core::store::JsonStore;
use anyhow::Result;
use providers::{LocalModelEntry, LocalModelStore, ProviderError};
use std::path::PathBuf;

/// `LocalModelStore` backed by `JsonStore` with the same
/// `<path>.lock` sidecar protection that `JsonStore` already provides.
pub struct JsonLocalModelStore {
    store: JsonStore,
}

impl JsonLocalModelStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            store: JsonStore::new(path),
        }
    }
}

#[async_trait::async_trait]
impl LocalModelStore for JsonLocalModelStore {
    async fn load(&self) -> Result<Vec<LocalModelEntry>, ProviderError> {
        let value = self
            .store
            .get("local_models")
            .await
            .map_err(|e| ProviderError::Fetch(e.to_string()))?;
        match value {
            Some(serde_json::Value::Array(arr)) => {
                serde_json::from_value::<Vec<LocalModelEntry>>(serde_json::Value::Array(arr))
                    .map_err(|e| ProviderError::Fetch(e.to_string()))
            }
            Some(_) | None => Ok(Vec::new()),
        }
    }

    async fn save(&self, entries: &[LocalModelEntry]) -> Result<(), ProviderError> {
        let value =
            serde_json::to_value(entries).map_err(|e| ProviderError::Fetch(e.to_string()))?;
        self.store
            .set("local_models", value)
            .await
            .map_err(|e| ProviderError::Fetch(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::store::JsonStore;
    use providers::LocalApi;
    use serde_json::json;
    use std::env::temp_dir;

    fn temp_path(name: &str) -> PathBuf {
        let mut path = temp_dir();
        path.push(format!(
            "alan-local-store-test-{}-{name}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[tokio::test]
    async fn load_missing_file_returns_empty() {
        let store = JsonLocalModelStore::new(temp_path("missing"));
        assert!(store.load().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn save_then_load_roundtrips() {
        let store = JsonLocalModelStore::new(temp_path("roundtrip"));
        let entries = vec![LocalModelEntry {
            model_id: "llama3".into(),
            url: "http://localhost:11434".into(),
            api: LocalApi::ChatCompletions,
            api_key: None,
        }];
        store.save(&entries).await.unwrap();
        assert_eq!(store.load().await.unwrap(), entries);
    }

    #[tokio::test]
    async fn load_rejects_malformed_entries() {
        let path = temp_path("corrupt");
        // A valid JSON array containing one well-formed and one malformed entry.
        JsonStore::new(path.clone())
            .set(
                "local_models",
                json!([
                    {"model_id": "ok", "url": "http://x", "api": "ChatCompletions", "api_key": null},
                    {"model_id": "bad"}
                ]),
            )
            .await
            .unwrap();
        let store = JsonLocalModelStore::new(path);
        assert!(store.load().await.is_err());
    }
}
