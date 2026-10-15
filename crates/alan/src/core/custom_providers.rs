//! Providers added with `/provider add`: any server that speaks Chat
//! Completions. Each is a name, a base URL and an API standard in
//! `providers.json`. Its key, if any, lives in `auth.json` under the same
//! name so `/login` can change it.

use anyhow::Result;
use providers::{
    ApiId, Credential, CredentialStore, CustomProvider, ModelInfo, ProviderError, ProviderId,
    ProviderRegistry, fetch_catalog,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use crate::core::paths;
use crate::core::store::JsonStore;

const PROVIDERS_KEY: &str = "providers";

/// A custom provider's name, and its id in `/models`, `auth.json`,
/// `settings.json` and `ALAN_PROVIDER`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "String")]
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

impl fmt::Display for ProviderName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CustomProviderEntry {
    pub name: ProviderName,
    pub url: String,
    pub api: ApiId,
}

/// `providers.json`: the saved custom providers, in the order they were added.
///
/// Entries are edited as raw JSON, matched by name, so one Alan cannot read
/// (a hand edit, or an API standard from a newer Alan) is kept and does not
/// block saving the others.
pub struct CustomProviderStore {
    store: JsonStore,
}

impl CustomProviderStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            store: JsonStore::new(path),
        }
    }

    /// The entries Alan can read.
    pub async fn load(&self) -> Result<Vec<CustomProviderEntry>> {
        Ok(self.load_each().await?.into_iter().flatten().collect())
    }

    /// Each saved entry on its own, with the reason for any it cannot read.
    pub async fn load_each(&self) -> Result<Vec<serde_json::Result<CustomProviderEntry>>> {
        let values = raw_entries(self.store.get(PROVIDERS_KEY).await?)?;
        Ok(values.into_iter().map(serde_json::from_value).collect())
    }

    /// Save `entry`, replacing a saved provider with the same name.
    pub async fn put(&self, entry: CustomProviderEntry) -> Result<()> {
        let name = entry.name.to_string();
        let value = serde_json::to_value(entry)?;
        self.store
            .update(PROVIDERS_KEY, move |saved| {
                let mut values = raw_entries(saved)?;
                match values.iter_mut().find(|saved| has_name(saved, &name)) {
                    Some(saved) => *saved = value,
                    None => values.push(value),
                }
                Ok((Some(Value::Array(values)), ()))
            })
            .await
    }

    pub async fn remove(&self, name: &ProviderName) -> Result<()> {
        let name = name.to_string();
        self.store
            .update(PROVIDERS_KEY, move |saved| {
                let mut values = raw_entries(saved)?;
                values.retain(|saved| !has_name(saved, &name));
                Ok(((!values.is_empty()).then_some(Value::Array(values)), ()))
            })
            .await
    }
}

fn raw_entries(value: Option<Value>) -> Result<Vec<Value>> {
    Ok(value
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default())
}

fn has_name(entry: &Value, name: &str) -> bool {
    entry.get("name").and_then(Value::as_str) == Some(name)
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
        }
    }

    /// Register every saved provider Alan can use; their models are fetched
    /// the next time `/models` opens. Entries it cannot read or bind are
    /// skipped. Returns a notice for each entry removed because a built-in now
    /// takes its name.
    pub async fn load(&self) -> Result<Vec<String>> {
        let mut notices = Vec::new();
        for entry in self.store.load_each().await? {
            let Ok(entry) = entry.inspect_err(|error| {
                tracing::warn!(%error, "skipping an unreadable entry in providers.json");
            }) else {
                continue;
            };
            // The name, and its key slot in `auth.json`, now belong to the
            // built-in, so only the entry goes.
            if self.registry.is_builtin(&entry.name.id()) {
                self.store.remove(&entry.name).await?;
                notices.push(format!(
                    "custom provider {} ({}) was removed: a built-in provider now uses that name; add it again under another name",
                    entry.name, entry.url
                ));
                continue;
            }
            if let Err(error) = self.register(&entry, Vec::new()) {
                tracing::warn!(%error, provider = %entry.name, "skipping a custom provider");
            }
        }
        Ok(notices)
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
            .and_then(|credential| match credential {
                Credential::ApiKey { key } => Some(key),
                // `/provider` only saves API keys; sign-in tokens belong to built-ins.
                Credential::OAuth { .. } => None,
            }))
    }

    /// Check `url` with a `/models` call, then save the provider and its key.
    /// Returns how many models the server lists. Nothing is saved when the
    /// check fails.
    pub async fn add(
        &self,
        name: ProviderName,
        api: ApiId,
        url: &str,
        key: Option<String>,
    ) -> Result<usize> {
        // A built-in's key in `auth.json` sits under the same name.
        anyhow::ensure!(
            !self.registry.is_builtin(&name.id()),
            "{name} is a built-in provider; pick another name, e.g. {name}-2"
        );
        anyhow::ensure!(
            self.registry.custom(&name.id()).is_none(),
            "{name} already exists; change it with /provider edit"
        );
        let (entry, models) = self.check(name, api, url, key.as_deref()).await?;
        self.store.put(entry.clone()).await?;
        if let Err(error) = self.save_key(&entry.name, key).await {
            if let Err(undo) = self.store.remove(&entry.name).await {
                return Err(error.context(format!(
                    "{} is still in providers.json: {undo:#}",
                    entry.name
                )));
            }
            return Err(error);
        }
        let count = models.len();
        self.register(&entry, models)?;
        Ok(count)
    }

    /// [`add`](Self::add) for a saved provider. No key removes the saved one.
    pub async fn update(
        &self,
        name: ProviderName,
        api: ApiId,
        url: &str,
        key: Option<String>,
    ) -> Result<usize> {
        let (entry, models) = self.check(name, api, url, key.as_deref()).await?;
        self.store.put(entry.clone()).await?;
        self.save_key(&entry.name, key).await?;
        let count = models.len();
        self.register(&entry, models)?;
        Ok(count)
    }

    pub async fn remove(&self, name: &ProviderName) -> Result<()> {
        self.store.remove(name).await?;
        self.credentials.delete(&name.id()).await?;
        self.registry.remove_custom(&name.id());
        Ok(())
    }

    async fn check(
        &self,
        name: ProviderName,
        api: ApiId,
        url: &str,
        key: Option<&str>,
    ) -> Result<(CustomProviderEntry, Vec<ModelInfo>)> {
        let url = url.trim().trim_end_matches('/').to_owned();
        // The reason already names the URL; the error's own prefix would
        // only repeat that the check failed.
        let models =
            fetch_catalog(&name.id(), &api, &url, key)
                .await
                .map_err(|error| match error {
                    ProviderError::Fetch(reason) => anyhow::anyhow!(reason),
                    other => other.into(),
                })?;
        Ok((CustomProviderEntry { name, url, api }, models))
    }

    async fn save_key(&self, name: &ProviderName, key: Option<String>) -> Result<()> {
        match key {
            Some(key) => {
                self.credentials
                    .put(&name.id(), Credential::ApiKey { key })
                    .await?
            }
            None => self.credentials.delete(&name.id()).await?,
        }
        Ok(())
    }

    fn register(&self, entry: &CustomProviderEntry, models: Vec<ModelInfo>) -> Result<()> {
        // A bound model keeps its provider's API client: replacing the
        // provider on an edit would leave the running model on the old URL.
        if let Some(provider) = self.registry.custom(&entry.name.id()) {
            provider.set_base_url(entry.url.clone());
            return Ok(());
        }
        self.registry.add_custom(Arc::new(CustomProvider::new(
            entry.name.id(),
            entry.api.clone(),
            entry.url.clone(),
            Arc::clone(&self.credentials),
            models,
        )?));
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::core::store::JsonStore;
    use providers::{CredentialError, CredentialInfo, InMemoryCredentialStore};
    use serde_json::json;
    use std::env::temp_dir;

    const OK_TWO_MODELS: &str =
        "HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n{\"data\":[{\"id\":\"m1\"},{\"id\":\"m2\"}]}";

    fn temp_path(name: &str) -> PathBuf {
        let path = temp_dir().join(format!(
            "alan-custom-providers-test-{}-{name}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn cleanup(name: &str) {
        let path = temp_path(name);
        let _ = std::fs::remove_file(format!("{}.lock", path.display()));
    }

    fn name(value: &str) -> ProviderName {
        ProviderName::parse(value).unwrap()
    }

    pub(crate) fn provider(id: &str, models: &[&str]) -> Arc<CustomProvider> {
        let models = models
            .iter()
            .map(|model| ModelInfo::new(model, ProviderId::new(id)))
            .collect();
        Arc::new(
            CustomProvider::new(
                ProviderId::new(id),
                ApiId::ChatCompletions,
                "http://localhost:1/v1",
                Arc::new(InMemoryCredentialStore::new()),
                models,
            )
            .unwrap(),
        )
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

    // Replies `response` to `count` connections, hands back each request.
    async fn serve(
        response: &'static str,
        count: usize,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
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
                socket.write_all(response.as_bytes()).await.unwrap();
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
    fn provider_name_rejects_bad_characters() {
        for bad in ["", "Grok", "my groq", "groq!"] {
            assert!(ProviderName::parse(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[tokio::test]
    async fn add_refuses_a_builtin_name_before_any_request() {
        let registry = Arc::new(ProviderRegistry::new([
            provider("deepseek", &[]) as Arc<dyn providers::Provider>
        ]));
        let providers = CustomProviders::new(
            CustomProviderStore::new(temp_path("builtin")),
            registry,
            Arc::new(InMemoryCredentialStore::new()),
        );

        // Port 1 is closed: reaching the network would say "could not connect".
        let error = providers
            .add(
                name("deepseek"),
                ApiId::ChatCompletions,
                "http://127.0.0.1:1/v1",
                None,
            )
            .await
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "deepseek is a built-in provider; pick another name, e.g. deepseek-2"
        );
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
                    api: ApiId::ChatCompletions,
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
    async fn store_keeps_entries_it_cannot_read_and_still_saves_others() {
        let path = temp_path("unreadable");
        JsonStore::new(path.clone())
            .set(
                PROVIDERS_KEY,
                json!([{"name": "Bad Name", "url": "http://x", "api": "ChatCompletions"}]),
            )
            .await
            .unwrap();
        let store = CustomProviderStore::new(path.clone());

        store
            .put(CustomProviderEntry {
                name: name("grok"),
                url: "http://one".into(),
                api: ApiId::ChatCompletions,
            })
            .await
            .unwrap();
        store.remove(&name("grok")).await.unwrap();
        store
            .put(CustomProviderEntry {
                name: name("groq"),
                url: "http://two".into(),
                api: ApiId::ChatCompletions,
            })
            .await
            .unwrap();

        let names: Vec<_> = store
            .load()
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.name.to_string())
            .collect();
        assert_eq!(names, ["groq"]);
        let raw = JsonStore::new(path)
            .get(PROVIDERS_KEY)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(raw.as_array().unwrap().len(), 2, "{raw}");
        cleanup("unreadable");
    }

    #[tokio::test]
    async fn add_checks_then_saves_entry_key_and_provider() {
        let (providers, registry, _credentials) = service("add");
        let url = serve(OK_TWO_MODELS, 1).await.0;

        let count = providers
            .add(
                name("grok"),
                ApiId::ChatCompletions,
                &url,
                Some("sk".into()),
            )
            .await
            .unwrap();

        assert_eq!(count, 2);
        let entries = providers.entries().await.unwrap();
        assert_eq!(entries[0].url, url.trim_end_matches('/'));
        assert_eq!(entries[0].api, ApiId::ChatCompletions);
        assert_eq!(
            providers.key(&name("grok")).await.unwrap().as_deref(),
            Some("sk")
        );
        let provider = registry.get(&name("grok").id()).unwrap();
        assert_eq!(provider.models().len(), 2);
        cleanup("add");
    }

    #[tokio::test]
    async fn add_saves_nothing_when_the_check_fails() {
        let (providers, registry, credentials) = service("add-fails");
        let url = serve(
            "HTTP/1.1 401 Unauthorized\r\nconnection: close\r\n\r\n{}",
            1,
        )
        .await
        .0;

        let error = providers
            .add(
                name("grok"),
                ApiId::ChatCompletions,
                &url,
                Some("bad".into()),
            )
            .await
            .unwrap_err();

        assert_eq!(
            format!("{error:#}"),
            "the API key was rejected (HTTP 401 Unauthorized)"
        );
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
        let (providers, registry, _credentials) = service("taken");
        registry.add_custom(provider("taken", &[]));

        // Port 1 is closed: reaching the network would say "could not connect".
        let error = providers
            .add(
                name("taken"),
                ApiId::ChatCompletions,
                "http://127.0.0.1:1/v1",
                None,
            )
            .await
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "taken already exists; change it with /provider edit"
        );
    }

    #[tokio::test]
    async fn add_leaves_no_key_when_providers_json_cannot_be_written() {
        // A directory where the file should be: every read and write fails.
        let path = temp_path("unwritable");
        std::fs::create_dir_all(&path).unwrap();
        let credentials = Arc::new(InMemoryCredentialStore::new());
        let providers = CustomProviders::new(
            CustomProviderStore::new(path.clone()),
            Arc::new(ProviderRegistry::new([])),
            credentials.clone(),
        );
        let url = serve(OK_TWO_MODELS, 1).await.0;

        let result = providers
            .add(
                name("grok"),
                ApiId::ChatCompletions,
                &url,
                Some("sk".into()),
            )
            .await;

        assert!(result.is_err());
        assert!(
            credentials
                .read(&name("grok").id())
                .await
                .unwrap()
                .is_none()
        );
        let _ = std::fs::remove_dir_all(&path);
        cleanup("unwritable");
    }

    #[tokio::test]
    async fn add_takes_the_entry_back_when_the_key_cannot_be_saved() {
        let (providers_json, credentials) = (temp_path("key-fails"), Arc::new(FailingKeys));
        let registry = Arc::new(ProviderRegistry::new([]));
        let providers = CustomProviders::new(
            CustomProviderStore::new(providers_json),
            Arc::clone(&registry),
            credentials,
        );
        let url = serve(OK_TWO_MODELS, 1).await.0;

        let result = providers
            .add(
                name("grok"),
                ApiId::ChatCompletions,
                &url,
                Some("sk".into()),
            )
            .await;

        assert!(result.is_err());
        assert!(providers.entries().await.unwrap().is_empty());
        assert!(registry.custom(&name("grok").id()).is_none());
        cleanup("key-fails");
    }

    /// A key store whose writes always fail.
    struct FailingKeys;

    #[async_trait::async_trait]
    impl CredentialStore for FailingKeys {
        async fn read(&self, _: &ProviderId) -> Result<Option<Credential>, CredentialError> {
            Ok(None)
        }

        async fn list(&self) -> Result<Vec<CredentialInfo>, CredentialError> {
            Ok(Vec::new())
        }

        async fn put(&self, _: &ProviderId, _: Credential) -> Result<(), CredentialError> {
            Err(CredentialError::LockBusy)
        }

        async fn delete(&self, _: &ProviderId) -> Result<(), CredentialError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn update_without_a_key_deletes_the_saved_one() {
        let (providers, _registry, credentials) = service("update");
        credentials
            .put(&name("grok").id(), Credential::ApiKey { key: "old".into() })
            .await
            .unwrap();
        let url = serve(OK_TWO_MODELS, 1).await.0;

        providers
            .update(name("grok"), ApiId::ChatCompletions, &url, None)
            .await
            .unwrap();

        assert!(providers.key(&name("grok")).await.unwrap().is_none());
        cleanup("update");
    }

    #[tokio::test]
    async fn update_moves_models_already_bound_to_the_new_url() {
        let (providers, registry, _credentials) = service("edit-bound");
        let old_url = serve(OK_TWO_MODELS, 1).await.0;
        providers
            .add(name("grok"), ApiId::ChatCompletions, &old_url, None)
            .await
            .unwrap();
        let provider = registry.get(&name("grok").id()).unwrap();
        let model = providers::bind_model(provider.as_ref(), "m1", Default::default()).unwrap();

        // The new server answers the edit's check, then the chat request.
        let (new_url, server) = serve(OK_TWO_MODELS, 2).await;
        providers
            .update(name("grok"), ApiId::ChatCompletions, &new_url, None)
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
        let url = serve(OK_TWO_MODELS, 1).await.0;
        providers
            .add(
                name("grok"),
                ApiId::ChatCompletions,
                &url,
                Some("sk".into()),
            )
            .await
            .unwrap();

        providers.remove(&name("grok")).await.unwrap();

        assert!(providers.entries().await.unwrap().is_empty());
        assert!(providers.key(&name("grok")).await.unwrap().is_none());
        assert!(registry.get(&name("grok").id()).is_none());
        cleanup("remove-all");
    }

    #[tokio::test]
    async fn load_skips_entries_it_cannot_use_and_keeps_the_rest() {
        let path = temp_path("load-skips");
        JsonStore::new(path.clone())
            .set(
                PROVIDERS_KEY,
                json!([
                    {"name": "grok", "url": "http://localhost:1/v1", "api": "ChatCompletions"},
                    {"name": "Bad Name", "url": "http://x", "api": "ChatCompletions"},
                    {"name": "newer", "url": "http://x", "api": "SomeFutureStandard"},
                    {"name": "claude", "url": "http://x", "api": "AnthropicMessages"}
                ]),
            )
            .await
            .unwrap();
        let registry = Arc::new(ProviderRegistry::new([]));
        let providers = CustomProviders::new(
            CustomProviderStore::new(path),
            Arc::clone(&registry),
            Arc::new(InMemoryCredentialStore::new()),
        );

        let notices = providers.load().await.unwrap();

        assert!(notices.is_empty(), "{notices:?}");
        let ids: Vec<_> = registry.providers().iter().map(|p| p.id().0).collect();
        assert_eq!(ids, ["grok"]);
        // Models come with the first `/models`, not at startup.
        assert!(
            registry
                .get(&name("grok").id())
                .unwrap()
                .models()
                .is_empty()
        );
        cleanup("load-skips");
    }

    #[tokio::test]
    async fn load_drops_an_entry_a_builtin_now_names_and_keeps_the_builtins_key() {
        let path = temp_path("load-clash");
        let store = CustomProviderStore::new(path.clone());
        for n in ["openai", "grok"] {
            store
                .put(CustomProviderEntry {
                    name: name(n),
                    url: "http://localhost:1/v1".into(),
                    api: ApiId::ChatCompletions,
                })
                .await
                .unwrap();
        }
        let credentials = Arc::new(InMemoryCredentialStore::new());
        credentials
            .put(
                &name("openai").id(),
                Credential::ApiKey { key: "sk".into() },
            )
            .await
            .unwrap();
        let registry = Arc::new(ProviderRegistry::new([
            provider("openai", &[]) as Arc<dyn providers::Provider>
        ]));
        let providers = CustomProviders::new(
            CustomProviderStore::new(path),
            Arc::clone(&registry),
            credentials.clone(),
        );

        let notices = providers.load().await.unwrap();

        assert_eq!(
            notices,
            [
                "custom provider openai (http://localhost:1/v1) was removed: a built-in provider now uses that name; add it again under another name"
            ]
        );
        assert!(registry.custom(&name("openai").id()).is_none());
        assert!(registry.custom(&name("grok").id()).is_some());
        let names: Vec<_> = providers
            .entries()
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.name.to_string())
            .collect();
        assert_eq!(names, ["grok"]);
        // The key under that name now belongs to the built-in.
        assert!(
            credentials
                .read(&name("openai").id())
                .await
                .unwrap()
                .is_some()
        );
        cleanup("load-clash");
    }
}
