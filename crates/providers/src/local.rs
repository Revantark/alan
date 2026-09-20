use crate::store::LocalModelStore;
use crate::{
    ApiId, ApiKeyAuth, AuthResolver, ModelInfo, ModelOptions, NoAuth, Provider, ProviderError,
    ProviderId, ServerToolInfo,
};
use async_trait::async_trait;
use llm::{ChatCompletionsApi, HttpClient, LlmApi};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, RwLock};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum LocalApi {
    ChatCompletions,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalModelEntry {
    pub model_id: String,
    pub url: String,
    pub api: LocalApi,
    pub api_key: Option<String>,
}

pub struct LocalProvider {
    store: Arc<dyn LocalModelStore>,
    entries: RwLock<Vec<LocalModelEntry>>,
    models: RwLock<Vec<ModelInfo>>,
}

impl LocalProvider {
    pub fn new(store: Arc<dyn LocalModelStore>) -> Self {
        Self {
            store,
            entries: RwLock::new(Vec::new()),
            models: RwLock::new(Vec::new()),
        }
    }

    pub async fn load(&self) -> Result<(), ProviderError> {
        let entries = self.store.load().await?;
        let models = Self::rebuild(&entries);
        *self.entries.write().unwrap_or_else(|e| e.into_inner()) = entries;
        *self.models.write().unwrap_or_else(|e| e.into_inner()) = models;
        Ok(())
    }

    pub async fn add_model(&self, entry: LocalModelEntry) -> Result<(), ProviderError> {
        {
            let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
            if entries
                .iter()
                .any(|existing| existing.model_id == entry.model_id)
            {
                return Err(ProviderError::Fetch(format!(
                    "local model already exists: {}",
                    entry.model_id
                )));
            }
            entries.push(entry);
            let models = Self::rebuild(&entries);
            *self.models.write().unwrap_or_else(|e| e.into_inner()) = models;
        }
        self.persist().await
    }

    pub async fn remove_model(&self, model_id: &str) -> Result<bool, ProviderError> {
        let removed;
        {
            let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
            let len_before = entries.len();
            entries.retain(|e| e.model_id != model_id);
            removed = entries.len() < len_before;
            let models = Self::rebuild(&entries);
            *self.models.write().unwrap_or_else(|e| e.into_inner()) = models;
        }
        if removed {
            self.persist().await?;
        }
        Ok(removed)
    }

    pub async fn update_model(&self, entry: LocalModelEntry) -> Result<(), ProviderError> {
        {
            let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
            let Some(existing) = entries.iter_mut().find(|e| e.model_id == entry.model_id) else {
                return Err(ProviderError::ModelNotFound(entry.model_id));
            };
            *existing = entry;
            let models = Self::rebuild(&entries);
            *self.models.write().unwrap_or_else(|e| e.into_inner()) = models;
        }
        self.persist().await
    }

    pub fn find_entry(&self, model_id: &str) -> Option<LocalModelEntry> {
        self.entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|e| e.model_id == model_id)
            .cloned()
    }

    fn rebuild(entries: &[LocalModelEntry]) -> Vec<ModelInfo> {
        entries
            .iter()
            .map(|e| ModelInfo {
                provider: ProviderId::new("local"),
                id: e.model_id.clone(),
                name: e.model_id.clone(),
                pricing: None,
                context_length: None,
            })
            .collect()
    }

    async fn persist(&self) -> Result<(), ProviderError> {
        let entries = self
            .entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        self.store.save(&entries).await
    }
}

#[async_trait]
impl Provider for LocalProvider {
    fn id(&self) -> ProviderId {
        ProviderId::new("local")
    }

    fn models(&self) -> Vec<ModelInfo> {
        self.models
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn server_tools(&self) -> &[ServerToolInfo] {
        &[]
    }

    fn auth_methods(&self) -> Vec<crate::auth::AuthMethod> {
        vec![]
    }

    fn apis(&self) -> &HashMap<ApiId, Arc<dyn LlmApi>> {
        // Local models bind per-entry via `bind_local_model`, which reads the
        // URL from the chosen `LocalModelEntry`. This provider-level entry is a
        // placeholder so the catalog advertises chat-completions support.
        static APIS: LazyLock<HashMap<ApiId, Arc<dyn LlmApi>>> = LazyLock::new(|| {
            HashMap::from([(
                ApiId::ChatCompletions,
                Arc::new(ChatCompletionsApi::new("", Arc::new(HttpClient::new())))
                    as Arc<dyn LlmApi>,
            )])
        });
        &APIS
    }

    fn auth(&self) -> Arc<dyn AuthResolver> {
        Arc::new(NoAuth)
    }

    async fn validate_auth(
        &self,
        _auth_result: &crate::auth::AuthResult,
    ) -> Result<(), crate::auth::AuthError> {
        Ok(())
    }

    async fn fetch_models(&self) -> Result<(), ProviderError> {
        self.load().await
    }
}

/// Bind a [`crate::Model`] to a local model entry, using the entry's URL and
/// optional API key. Local models are chat-completions compatible.
pub fn bind_local_model(
    entry: &LocalModelEntry,
    options: ModelOptions,
) -> Result<crate::Model, ProviderError> {
    let api: Arc<dyn LlmApi> = Arc::new(ChatCompletionsApi::new(
        &entry.url,
        Arc::new(HttpClient::new()),
    ));
    let auth: Arc<dyn AuthResolver> = match entry.api_key.as_deref() {
        Some(key) if !key.is_empty() => Arc::new(ApiKeyAuth::new(key)),
        _ => Arc::new(NoAuth),
    };
    let info = ModelInfo {
        provider: ProviderId::new("local"),
        id: entry.model_id.clone(),
        name: entry.model_id.clone(),
        pricing: None,
        context_length: None,
    };
    Ok(crate::Model::new_with_options(info, api, auth, options))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::LocalModelStore;

    fn make_entry(model_id: &str) -> LocalModelEntry {
        LocalModelEntry {
            model_id: model_id.into(),
            url: format!("http://localhost:8080/{model_id}"),
            api: LocalApi::ChatCompletions,
            api_key: None,
        }
    }

    #[derive(Default)]
    struct MockStore {
        entries: RwLock<Vec<LocalModelEntry>>,
    }

    impl MockStore {
        fn shared() -> Arc<dyn LocalModelStore> {
            Arc::new(Self::default())
        }
    }

    #[async_trait::async_trait]
    impl LocalModelStore for MockStore {
        async fn load(&self) -> Result<Vec<LocalModelEntry>, ProviderError> {
            Ok(self
                .entries
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone())
        }

        async fn save(&self, entries: &[LocalModelEntry]) -> Result<(), ProviderError> {
            *self.entries.write().unwrap_or_else(|e| e.into_inner()) = entries.to_vec();
            Ok(())
        }
    }

    #[tokio::test]
    async fn load_missing_file_returns_empty() {
        let store = MockStore::shared();
        let provider = LocalProvider::new(store);
        provider.load().await.unwrap();
        assert!(provider.models().is_empty());
    }

    #[tokio::test]
    async fn add_model_appears_in_catalog() {
        let store = MockStore::shared();
        let provider = LocalProvider::new(Arc::clone(&store));
        provider.add_model(make_entry("llama3")).await.unwrap();
        let models = provider.models();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "llama3");
        assert_eq!(models[0].provider, ProviderId::new("local"));
        // Verify persistence via store
        let persisted = store.load().await.unwrap();
        assert_eq!(persisted.len(), 1);
    }

    #[tokio::test]
    async fn add_multiple_models() {
        let store = MockStore::shared();
        let provider = LocalProvider::new(Arc::clone(&store));
        provider.add_model(make_entry("a")).await.unwrap();
        provider.add_model(make_entry("b")).await.unwrap();
        assert_eq!(provider.models().len(), 2);
        let persisted = store.load().await.unwrap();
        assert_eq!(persisted.len(), 2);
    }

    #[tokio::test]
    async fn remove_model() {
        let store = MockStore::shared();
        let provider = LocalProvider::new(Arc::clone(&store));
        provider.add_model(make_entry("a")).await.unwrap();
        provider.add_model(make_entry("b")).await.unwrap();
        let removed = provider.remove_model("a").await.unwrap();
        assert!(removed);
        assert_eq!(provider.models().len(), 1);
        assert_eq!(provider.models()[0].id, "b");
        let persisted = store.load().await.unwrap();
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].model_id, "b");
    }

    #[tokio::test]
    async fn remove_nonexistent_returns_false() {
        let store = MockStore::shared();
        let provider = LocalProvider::new(store);
        assert!(!provider.remove_model("nope").await.unwrap());
    }

    #[tokio::test]
    async fn update_model() {
        let store = MockStore::shared();
        let provider = LocalProvider::new(Arc::clone(&store));
        provider.add_model(make_entry("a")).await.unwrap();
        let updated = LocalModelEntry {
            model_id: "a".into(),
            url: "http://new-url:8080/v1".into(),
            api: LocalApi::ChatCompletions,
            api_key: Some("key".into()),
        };
        provider.update_model(updated).await.unwrap();
        let found = provider.find_entry("a").unwrap();
        assert_eq!(found.url, "http://new-url:8080/v1");
        let persisted = store.load().await.unwrap();
        assert_eq!(persisted[0].url, "http://new-url:8080/v1");
    }

    #[tokio::test]
    async fn find_entry_returns_none_for_missing() {
        let store = MockStore::shared();
        let provider = LocalProvider::new(store);
        assert!(provider.find_entry("nope").is_none());
    }

    #[tokio::test]
    async fn find_entry_returns_correct_entry() {
        let store = MockStore::shared();
        let provider = LocalProvider::new(Arc::clone(&store));
        provider.add_model(make_entry("x")).await.unwrap();
        let entry = provider.find_entry("x").unwrap();
        assert_eq!(entry.model_id, "x");
    }

    #[tokio::test]
    async fn persists_to_disk_and_reloads() {
        let store = MockStore::shared();
        {
            let provider = LocalProvider::new(Arc::clone(&store));
            provider.add_model(make_entry("a")).await.unwrap();
        }
        let provider = LocalProvider::new(Arc::clone(&store));
        provider.load().await.unwrap();
        assert_eq!(provider.models().len(), 1);
        assert_eq!(provider.models()[0].id, "a");
    }

    #[tokio::test]
    async fn remove_persists_to_disk() {
        let store = MockStore::shared();
        {
            let provider = LocalProvider::new(Arc::clone(&store));
            provider.add_model(make_entry("a")).await.unwrap();
            provider.add_model(make_entry("b")).await.unwrap();
            provider.remove_model("a").await.unwrap();
        }
        let provider = LocalProvider::new(Arc::clone(&store));
        provider.load().await.unwrap();
        assert_eq!(provider.models().len(), 1);
        assert_eq!(provider.models()[0].id, "b");
    }

    #[tokio::test]
    async fn update_persists_to_disk() {
        let store = MockStore::shared();
        {
            let provider = LocalProvider::new(Arc::clone(&store));
            provider.add_model(make_entry("a")).await.unwrap();
            let updated = LocalModelEntry {
                model_id: "a".into(),
                url: "http://updated:9090".into(),
                api: LocalApi::ChatCompletions,
                api_key: None,
            };
            provider.update_model(updated).await.unwrap();
        }
        let provider = LocalProvider::new(Arc::clone(&store));
        provider.load().await.unwrap();
        let entry = provider.find_entry("a").unwrap();
        assert_eq!(entry.url, "http://updated:9090");
        assert_eq!(entry.api, LocalApi::ChatCompletions);
    }

    #[tokio::test]
    async fn fetch_models_populates_catalog() {
        let store = MockStore::shared();
        {
            let provider = LocalProvider::new(Arc::clone(&store));
            provider.add_model(make_entry("x")).await.unwrap();
        }
        let provider = LocalProvider::new(Arc::clone(&store));
        provider.fetch_models().await.unwrap();
        assert_eq!(provider.models().len(), 1);
        assert_eq!(provider.models()[0].id, "x");
    }

    #[tokio::test]
    async fn bind_chat_completions() {
        let entry = LocalModelEntry {
            model_id: "llama3".into(),
            url: "http://localhost:11434/v1".into(),
            api: LocalApi::ChatCompletions,
            api_key: None,
        };
        let model = bind_local_model(&entry, ModelOptions::default()).unwrap();
        assert_eq!(model.info().provider, ProviderId::new("local"));
        assert_eq!(model.info().id, "llama3");
    }

    #[tokio::test]
    async fn bind_uses_entry_url() {
        let entry = LocalModelEntry {
            model_id: "gemini-local".into(),
            url: "http://localhost:8080".into(),
            api: LocalApi::ChatCompletions,
            api_key: None,
        };
        let model = bind_local_model(&entry, ModelOptions::default()).unwrap();
        assert_eq!(model.info().id, "gemini-local");
        assert_eq!(model.info().provider, ProviderId::new("local"));
    }

    #[tokio::test]
    async fn provider_id_is_local() {
        let store = MockStore::shared();
        let provider = LocalProvider::new(store);
        assert_eq!(provider.id(), ProviderId::new("local"));
    }

    #[tokio::test]
    async fn empty_provider_has_no_models() {
        let store = MockStore::shared();
        let provider = LocalProvider::new(store);
        assert!(provider.models().is_empty());
    }
}
