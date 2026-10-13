//! Providers added with `/provider add`: any server that speaks Chat
//! Completions. Each is a name and a base URL in `providers.json`. Its key,
//! if any, lives in `auth.json` under the same name so `/login` can change it.

use anyhow::{Context, Result};
use providers::{
    Credential, CredentialStore, CustomProvider, ModelInfo, ProviderId, ProviderRegistry,
    fetch_catalog,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::core::paths;
use crate::core::store::JsonStore;

const PROVIDERS_KEY: &str = "providers";

/// Built-in provider ids, plus `google`, which the providers crate has but
/// Alan does not register yet. A custom provider may not take one: its key
/// would overwrite the built-in provider's in `auth.json`.
const RESERVED_NAMES: [&str; 4] = ["openrouter", "deepseek", "zai", "google"];

/// A custom provider's name, and its id in `/models`, `auth.json`,
/// `settings.json` and `ALAN_PROVIDER`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProviderName(String);

impl ProviderName {
    pub fn parse(name: &str) -> Result<Self> {
        let name = name.trim();
        anyhow::ensure!(!name.is_empty(), "name is required");
        anyhow::ensure!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "name may only contain lowercase letters, digits and -"
        );
        anyhow::ensure!(
            !RESERVED_NAMES.contains(&name),
            "{name} is a built-in provider"
        );
        Ok(Self(name.to_owned()))
    }

    pub fn id(&self) -> ProviderId {
        ProviderId::new(&self.0)
    }
}

impl TryFrom<String> for ProviderName {
    type Error = anyhow::Error;

    fn try_from(name: String) -> Result<Self> {
        Self::parse(&name)
    }
}

impl From<ProviderName> for String {
    fn from(name: ProviderName) -> Self {
        name.0
    }
}

impl fmt::Display for ProviderName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomProviderEntry {
    pub name: ProviderName,
    pub url: String,
}

/// `providers.json`: the saved custom providers, in the order they were added.
pub struct CustomProviderStore {
    store: JsonStore,
}

impl CustomProviderStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            store: JsonStore::new(path),
        }
    }

    pub async fn load(&self) -> Result<Vec<CustomProviderEntry>> {
        decode(self.store.get(PROVIDERS_KEY).await?)
    }

    /// Save `entry`, replacing a saved provider with the same name.
    pub async fn put(&self, entry: CustomProviderEntry) -> Result<()> {
        self.store
            .update(PROVIDERS_KEY, move |value| {
                let mut entries = decode(value)?;
                match entries.iter_mut().find(|saved| saved.name == entry.name) {
                    Some(saved) => *saved = entry,
                    None => entries.push(entry),
                }
                Ok((Some(serde_json::to_value(entries)?), ()))
            })
            .await
    }

    pub async fn remove(&self, name: &ProviderName) -> Result<bool> {
        self.store
            .update(PROVIDERS_KEY, |value| {
                let mut entries = decode(value)?;
                let before = entries.len();
                entries.retain(|saved| saved.name != *name);
                let removed = entries.len() < before;
                let replacement = if entries.is_empty() {
                    None
                } else {
                    Some(serde_json::to_value(entries)?)
                };
                Ok((replacement, removed))
            })
            .await
    }
}

fn decode(value: Option<serde_json::Value>) -> Result<Vec<CustomProviderEntry>> {
    Ok(value
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default())
}

/// Default location of providers.json inside the Alan data directory.
pub fn default_providers_path() -> Result<PathBuf> {
    paths::data_file("providers.json")
}

/// Adds, edits and removes custom providers, keeping `providers.json`,
/// `auth.json` and the live registry in step.
pub struct CustomProviders {
    store: CustomProviderStore,
    registry: Arc<ProviderRegistry>,
    credentials: Arc<dyn CredentialStore>,
    /// The registered instances, typed, so an edit can update one in place.
    registered: Mutex<HashMap<ProviderName, Arc<CustomProvider>>>,
}

impl CustomProviders {
    pub fn new(
        store: CustomProviderStore,
        registry: Arc<ProviderRegistry>,
        credentials: Arc<dyn CredentialStore>,
    ) -> Self {
        Self {
            store,
            registry,
            credentials,
            registered: Mutex::new(HashMap::new()),
        }
    }

    /// Register every saved provider. Their models are fetched the next time
    /// `/models` opens.
    pub async fn load(&self) -> Result<()> {
        for entry in self.store.load().await? {
            self.register(&entry, Vec::new());
        }
        Ok(())
    }

    pub async fn entries(&self) -> Result<Vec<CustomProviderEntry>> {
        self.store.load().await
    }

    /// The saved key, to prefill the edit form.
    pub async fn key(&self, name: &ProviderName) -> Result<Option<String>> {
        Ok(self
            .credentials
            .read(&name.id())
            .await?
            .map(|Credential::ApiKey { key }| key))
    }

    /// Check `url` with a `/models` call, then save the provider and its key.
    /// Returns how many models the server lists. Nothing is saved when the
    /// check fails.
    pub async fn add(&self, name: ProviderName, url: &str, key: Option<String>) -> Result<usize> {
        anyhow::ensure!(
            self.registry.get(&name.id()).is_none(),
            "provider already exists: {name}"
        );
        self.save(name, url, key).await
    }

    /// [`add`](Self::add) for a saved provider. No key removes the saved one.
    pub async fn update(
        &self,
        name: ProviderName,
        url: &str,
        key: Option<String>,
    ) -> Result<usize> {
        self.save(name, url, key).await
    }

    /// Refuses the provider of the running model: its next request would go
    /// out with the key this deletes.
    pub async fn remove(&self, name: &ProviderName, active: &ProviderId) -> Result<()> {
        anyhow::ensure!(
            name.id() != *active,
            "{name} is in use; pick another model with /models first"
        );
        self.store.remove(name).await?;
        self.credentials.delete(&name.id()).await?;
        self.registered
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
        self.registry.remove(&name.id());
        Ok(())
    }

    async fn save(&self, name: ProviderName, url: &str, key: Option<String>) -> Result<usize> {
        let url = url.trim().trim_end_matches('/').to_owned();
        let models = fetch_catalog(&name.id(), &url, key.as_deref())
            .await
            .with_context(|| format!("checking {url}/models"))?;
        let count = models.len();
        match key {
            Some(key) => {
                self.credentials
                    .put(&name.id(), Credential::ApiKey { key })
                    .await?
            }
            None => self.credentials.delete(&name.id()).await?,
        }
        let entry = CustomProviderEntry { name, url };
        self.store.put(entry.clone()).await?;
        self.register(&entry, models);
        Ok(count)
    }

    fn register(&self, entry: &CustomProviderEntry, models: Vec<ModelInfo>) {
        let mut registered = self.registered.lock().unwrap_or_else(|e| e.into_inner());
        // A bound model keeps its provider's API client: replacing the
        // provider on an edit would leave the running model on the old URL.
        if let Some(provider) = registered.get(&entry.name) {
            provider.set_base_url(entry.url.clone());
            return;
        }
        let provider = Arc::new(CustomProvider::new(
            entry.name.id(),
            entry.url.clone(),
            Arc::clone(&self.credentials),
            models,
        ));
        registered.insert(entry.name.clone(), Arc::clone(&provider));
        self.registry.insert(provider);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::store::JsonStore;
    use providers::InMemoryCredentialStore;
    use serde_json::json;
    use std::env::temp_dir;

    const OK_TWO_MODELS: &str =
        "HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n{\"data\":[{\"id\":\"m1\"},{\"id\":\"m2\"}]}";

    fn store_path(name: &str) -> PathBuf {
        temp_dir().join(format!(
            "alan-custom-providers-test-{}-{name}.json",
            std::process::id()
        ))
    }

    fn temp_path(name: &str) -> PathBuf {
        let path = store_path(name);
        let _ = std::fs::remove_file(&path);
        path
    }

    fn cleanup(name: &str) {
        let path = store_path(name);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}.lock", path.display()));
    }

    fn name(value: &str) -> ProviderName {
        ProviderName::parse(value).unwrap()
    }

    fn service(
        file: &str,
    ) -> (
        CustomProviders,
        Arc<ProviderRegistry>,
        Arc<InMemoryCredentialStore>,
    ) {
        let registry = Arc::new(ProviderRegistry::new([]));
        let credentials = Arc::new(InMemoryCredentialStore::new());
        let providers = CustomProviders::new(
            CustomProviderStore::new(temp_path(file)),
            Arc::clone(&registry),
            credentials.clone(),
        );
        (providers, registry, credentials)
    }

    // one-shot server: replies `response`.
    async fn serve_once(response: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut buffer).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&buffer[..read]);
            }
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        format!("http://{address}/v1/")
    }

    // Replies `OK_TWO_MODELS` to `count` connections, hands back each request.
    async fn serve_many(count: usize) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for _ in 0..count {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let read = socket.read(&mut buffer).await.unwrap();
                    assert!(read > 0);
                    request.extend_from_slice(&buffer[..read]);
                }
                socket.write_all(OK_TWO_MODELS.as_bytes()).await.unwrap();
                requests.push(String::from_utf8(request).unwrap());
            }
            requests
        });
        (format!("http://{address}/v1/"), server)
    }

    #[test]
    fn provider_name_accepts_lowercase_digits_and_dashes() {
        assert_eq!(name("  my-groq2 ").to_string(), "my-groq2");
    }

    #[test]
    fn provider_name_rejects_bad_and_reserved_names() {
        for bad in [
            "",
            "Grok",
            "my groq",
            "groq!",
            "openrouter",
            "deepseek",
            "zai",
            "google",
        ] {
            assert!(ProviderName::parse(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[tokio::test]
    async fn store_put_replaces_by_name_and_keeps_order() {
        let store = CustomProviderStore::new(temp_path("put"));
        for (n, url) in [
            ("a", "http://one"),
            ("b", "http://two"),
            ("a", "http://three"),
        ] {
            store
                .put(CustomProviderEntry {
                    name: name(n),
                    url: url.into(),
                })
                .await
                .unwrap();
        }
        let urls: Vec<_> = store
            .load()
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.url)
            .collect();
        assert_eq!(urls, ["http://three", "http://two"]);
        cleanup("put");
    }

    #[tokio::test]
    async fn store_remove_reports_whether_an_entry_went() {
        let store = CustomProviderStore::new(temp_path("remove"));
        store
            .put(CustomProviderEntry {
                name: name("a"),
                url: "http://one".into(),
            })
            .await
            .unwrap();
        assert!(store.remove(&name("a")).await.unwrap());
        assert!(!store.remove(&name("a")).await.unwrap());
        assert!(store.load().await.unwrap().is_empty());
        cleanup("remove");
    }

    #[tokio::test]
    async fn store_load_rejects_an_invalid_name_in_the_file() {
        let path = temp_path("corrupt");
        JsonStore::new(path.clone())
            .set(
                PROVIDERS_KEY,
                json!([{"name": "Bad Name", "url": "http://x"}]),
            )
            .await
            .unwrap();
        assert!(CustomProviderStore::new(path).load().await.is_err());
        cleanup("corrupt");
    }

    #[tokio::test]
    async fn add_checks_then_saves_entry_key_and_provider() {
        let (providers, registry, credentials) = service("add");
        let url = serve_once(OK_TWO_MODELS).await;

        let count = providers
            .add(name("grok"), &url, Some("sk".into()))
            .await
            .unwrap();

        assert_eq!(count, 2);
        let entries = providers.entries().await.unwrap();
        assert_eq!(entries[0].url, url.trim_end_matches('/'));
        assert_eq!(
            providers.key(&name("grok")).await.unwrap().as_deref(),
            Some("sk")
        );
        assert!(
            credentials
                .read(&name("grok").id())
                .await
                .unwrap()
                .is_some()
        );
        let provider = registry.get(&name("grok").id()).unwrap();
        assert_eq!(provider.models().len(), 2);
        cleanup("add");
    }

    #[tokio::test]
    async fn add_saves_nothing_when_the_check_fails() {
        let (providers, registry, credentials) = service("add-fails");
        let url = serve_once("HTTP/1.1 401 Unauthorized\r\nconnection: close\r\n\r\n{}").await;

        let error = providers
            .add(name("grok"), &url, Some("bad".into()))
            .await
            .unwrap_err();

        assert!(format!("{error:#}").contains("401"), "{error:#}");
        assert!(providers.entries().await.unwrap().is_empty());
        assert!(
            credentials
                .read(&name("grok").id())
                .await
                .unwrap()
                .is_none()
        );
        assert!(registry.get(&name("grok").id()).is_none());
        cleanup("add-fails");
    }

    #[tokio::test]
    async fn add_refuses_a_taken_name_before_any_request() {
        let (providers, registry, credentials) = service("taken");
        registry.insert(Arc::new(CustomProvider::new(
            name("taken").id(),
            "http://localhost:1/v1",
            credentials,
            Vec::new(),
        )));

        // Port 1 is closed: reaching the network would say "could not connect".
        let error = providers
            .add(name("taken"), "http://127.0.0.1:1/v1", None)
            .await
            .unwrap_err();

        assert_eq!(error.to_string(), "provider already exists: taken");
    }

    #[tokio::test]
    async fn update_without_a_key_deletes_the_saved_one() {
        let (providers, _registry, credentials) = service("update");
        credentials
            .put(&name("grok").id(), Credential::ApiKey { key: "old".into() })
            .await
            .unwrap();
        let url = serve_once(OK_TWO_MODELS).await;

        providers.update(name("grok"), &url, None).await.unwrap();

        assert!(providers.key(&name("grok")).await.unwrap().is_none());
        cleanup("update");
    }

    #[tokio::test]
    async fn update_moves_models_already_bound_to_the_new_url() {
        let (providers, registry, _credentials) = service("edit-bound");
        let old_url = serve_once(OK_TWO_MODELS).await;
        providers.add(name("grok"), &old_url, None).await.unwrap();
        let provider = registry.get(&name("grok").id()).unwrap();
        let model = providers::bind_model(provider.as_ref(), "m1", Default::default()).unwrap();

        // The new server answers the edit's check, then the chat request.
        let (new_url, server) = serve_many(2).await;
        providers
            .update(name("grok"), &new_url, None)
            .await
            .unwrap();
        let messages = [llm::Message::user("hi")];
        let _ = model
            .stream(llm::CompletionInput {
                messages: &messages,
                tools: &[],
                options: &Default::default(),
                extensions: &Default::default(),
            })
            .await;

        let requests = tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .expect("the chat request never reached the new URL")
            .unwrap();
        assert!(
            requests[1].starts_with("POST /v1/chat/completions"),
            "{requests:?}"
        );
        cleanup("edit-bound");
    }

    #[tokio::test]
    async fn remove_deletes_entry_key_and_provider() {
        let (providers, registry, _credentials) = service("remove-all");
        let url = serve_once(OK_TWO_MODELS).await;
        providers
            .add(name("grok"), &url, Some("sk".into()))
            .await
            .unwrap();

        providers
            .remove(&name("grok"), &ProviderId::new("openrouter"))
            .await
            .unwrap();

        assert!(providers.entries().await.unwrap().is_empty());
        assert!(providers.key(&name("grok")).await.unwrap().is_none());
        assert!(registry.get(&name("grok").id()).is_none());
        cleanup("remove-all");
    }

    #[tokio::test]
    async fn remove_refuses_the_provider_in_use() {
        let (providers, _registry, _credentials) = service("remove-active");
        let url = serve_once(OK_TWO_MODELS).await;
        providers.add(name("grok"), &url, None).await.unwrap();

        let error = providers
            .remove(&name("grok"), &name("grok").id())
            .await
            .unwrap_err();

        assert!(error.to_string().contains("in use"), "{error:#}");
        assert_eq!(providers.entries().await.unwrap().len(), 1);
        cleanup("remove-active");
    }

    #[tokio::test]
    async fn load_registers_saved_providers_without_fetching() {
        let path = temp_path("load");
        CustomProviderStore::new(path.clone())
            .put(CustomProviderEntry {
                name: name("grok"),
                url: "http://localhost:1/v1".into(),
            })
            .await
            .unwrap();
        let registry = Arc::new(ProviderRegistry::new([]));
        let providers = CustomProviders::new(
            CustomProviderStore::new(path),
            Arc::clone(&registry),
            Arc::new(InMemoryCredentialStore::new()),
        );

        providers.load().await.unwrap();

        let provider = registry.get(&name("grok").id()).unwrap();
        assert!(provider.models().is_empty());
        cleanup("load");
    }
}
