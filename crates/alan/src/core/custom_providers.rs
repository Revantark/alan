//! Providers added with `/provider add`: any server that speaks Chat
//! Completions. Each is a name, a base URL and an API standard in
//! `providers.json`. Its key, if any, lives in `auth.json` under the same
//! name so `/login` can change it.

use anyhow::Result;
use providers::{
    ApiId, Credential, CredentialStore, CustomProvider, ModelInfo, Provider, ProviderError,
    ProviderId, ProviderRegistry, fetch_catalog,
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

/// The saved custom providers: entries in `providers.json`, in the order
/// they were added, and their keys in `auth.json`.
///
/// Entries are edited as raw JSON, matched by name, so one Alan cannot read
/// (a hand edit, or an API standard from a newer Alan) is kept and does not
/// block saving the others.
pub struct CustomProviderStore {
    store: JsonStore,
    credentials: Arc<dyn CredentialStore>,
}

impl CustomProviderStore {
    pub fn new(path: impl Into<PathBuf>, credentials: Arc<dyn CredentialStore>) -> Self {
        Self {
            store: JsonStore::new(path),
            credentials,
        }
    }

    /// The entries Alan can read.
    pub async fn entries(&self) -> Result<Vec<CustomProviderEntry>> {
        Ok(self.load_each().await?.into_iter().flatten().collect())
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

    /// Save a new provider and its key, or neither.
    pub async fn add(&self, entry: &CustomProviderEntry, key: Option<String>) -> Result<()> {
        self.save_provider(entry, key, None).await
    }

    /// Save a changed provider and its key, or neither. No key removes the
    /// saved one.
    pub async fn update(&self, entry: &CustomProviderEntry, key: Option<String>) -> Result<()> {
        let previous = self.find(&entry.name).await?.ok_or_else(|| {
            anyhow::anyhow!("{} is not saved; add it with /provider add", entry.name)
        })?;
        self.save_provider(entry, key, Some(previous)).await
    }

    pub async fn remove(&self, name: &ProviderName) -> Result<()> {
        self.credentials.delete(&name.id()).await?;
        self.remove_entry(name).await
    }

    /// The provider to register for `entry`; it reads its key from this
    /// store's `auth.json` on every request.
    pub fn provider(
        &self,
        entry: &CustomProviderEntry,
        models: Vec<ModelInfo>,
    ) -> Result<Arc<dyn Provider>> {
        Ok(Arc::new(CustomProvider::new(
            entry.name.id(),
            entry.api.clone(),
            entry.url.clone(),
            Arc::clone(&self.credentials),
            models,
        )?))
    }

    /// Write `entry`, then its key. Without `previous` the entry is new and
    /// its name must be free. A refused key puts `previous` back, or takes the
    /// new entry out.
    async fn save_provider(
        &self,
        entry: &CustomProviderEntry,
        key: Option<String>,
        previous: Option<CustomProviderEntry>,
    ) -> Result<()> {
        match &previous {
            Some(_) => self.put(entry).await?,
            None => self.insert(entry).await?,
        }
        let Err(error) = self.save_key(&entry.name, key).await else {
            return Ok(());
        };

        let undo = match &previous {
            Some(previous) => self.put(previous).await,
            None => self.remove_entry(&entry.name).await,
        };

        Err(match undo {
            Ok(()) => error,
            Err(undo) => error.context(format!(
                "{} is still in providers.json: {undo:#}",
                entry.name
            )),
        })
    }

    async fn find(&self, name: &ProviderName) -> Result<Option<CustomProviderEntry>> {
        Ok(self
            .entries()
            .await?
            .into_iter()
            .find(|saved| saved.name.id() == name.id()))
    }

    /// Whether any entry uses `name`, even one this Alan cannot read.
    async fn contains(&self, name: &ProviderName) -> Result<bool> {
        let values = raw_entries(self.store.get(PROVIDERS_KEY).await?)?;
        Ok(values
            .iter()
            .any(|saved| has_name(saved, &name.to_string())))
    }

    /// Each saved entry on its own, with the reason for any it cannot read.
    async fn load_each(&self) -> Result<Vec<serde_json::Result<CustomProviderEntry>>> {
        let values = raw_entries(self.store.get(PROVIDERS_KEY).await?)?;
        Ok(values.into_iter().map(serde_json::from_value).collect())
    }

    /// Save `entry`, replacing a saved provider with the same name.
    async fn put(&self, entry: &CustomProviderEntry) -> Result<()> {
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

    /// Save `entry` under a name no entry uses. The check and the write share
    /// one lock, so another Alan cannot take the name in between.
    async fn insert(&self, entry: &CustomProviderEntry) -> Result<()> {
        let name = entry.name.clone();
        let value = serde_json::to_value(entry)?;
        self.store
            .update(PROVIDERS_KEY, move |saved| {
                let mut values = raw_entries(saved)?;
                if values
                    .iter()
                    .any(|saved| has_name(saved, &name.to_string()))
                {
                    return Err(already_exists(&name));
                }

                values.push(value);
                Ok((Some(Value::Array(values)), ()))
            })
            .await
    }

    async fn remove_entry(&self, name: &ProviderName) -> Result<()> {
        let name = name.to_string();
        self.store
            .update(PROVIDERS_KEY, move |saved| {
                let mut values = raw_entries(saved)?;
                values.retain(|saved| !has_name(saved, &name));
                Ok(((!values.is_empty()).then_some(Value::Array(values)), ()))
            })
            .await
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

fn already_exists(name: &ProviderName) -> anyhow::Error {
    anyhow::anyhow!("{name} already exists; change it with /provider edit")
}

/// Default location of providers.json inside the Alan data directory.
pub fn default_providers_path() -> Result<PathBuf> {
    paths::data_file("providers.json")
}

/// Register every saved provider Alan can use; their models are fetched the
/// next time `/models` opens. Entries it cannot read or bind are skipped.
/// Call it once the built-ins are registered: returns a notice for each entry
/// removed because a built-in now takes its name.
pub async fn load(store: &CustomProviderStore, registry: &ProviderRegistry) -> Result<Vec<String>> {
    let mut notices = Vec::new();
    for entry in store.load_each().await? {
        let Ok(entry) = entry.inspect_err(|error| {
            tracing::warn!(%error, "skipping an unreadable entry in providers.json");
        }) else {
            continue;
        };
        // The name, and its key slot in `auth.json`, now belong to the
        // built-in, so only the entry goes.
        if registry.get(&entry.name.id()).is_some() {
            store.remove_entry(&entry.name).await?;
            notices.push(format!(
                "custom provider {} ({}) was removed: a built-in provider now uses that name; add it again under another name",
                entry.name, entry.url
            ));
            continue;
        }
        match store.provider(&entry, Vec::new()) {
            Ok(provider) => registry.insert(provider),
            Err(error) => {
                tracing::warn!(%error, provider = %entry.name, "skipping a custom provider")
            }
        }
    }
    Ok(notices)
}

/// Refuse a name `/provider add` cannot take, before any request.
pub async fn ensure_new_name(
    store: &CustomProviderStore,
    registry: &ProviderRegistry,
    name: &ProviderName,
) -> Result<()> {
    if store.contains(name).await? {
        return Err(already_exists(name));
    }
    // A built-in's key in `auth.json` sits under the same name.
    anyhow::ensure!(
        registry.get(&name.id()).is_none(),
        "{name} is a built-in provider; pick another name, e.g. {name}-2"
    );
    Ok(())
}

/// Check `url` with a `/models` call. Returns the entry to save and the
/// models the server lists.
pub async fn check(
    name: ProviderName,
    api: ApiId,
    url: &str,
    key: Option<&str>,
) -> Result<(CustomProviderEntry, Vec<ModelInfo>)> {
    let url = url.trim().trim_end_matches('/').to_owned();
    // The reason already names the URL; the error's own prefix would only
    // repeat that the check failed.
    let models = fetch_catalog(&name.id(), &api, &url, key)
        .await
        .map_err(|error| match error {
            ProviderError::Fetch(reason) => anyhow::anyhow!(reason),
            other => other.into(),
        })?;
    Ok((CustomProviderEntry { name, url, api }, models))
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

    fn entry(value: &str, url: &str) -> CustomProviderEntry {
        CustomProviderEntry {
            name: name(value),
            url: url.into(),
            api: ApiId::ChatCompletions,
        }
    }

    pub(crate) fn provider(id: &str, models: &[&str]) -> Arc<dyn Provider> {
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

    fn store(file: &str) -> (CustomProviderStore, Arc<InMemoryCredentialStore>) {
        let credentials = Arc::new(InMemoryCredentialStore::new());
        let store = CustomProviderStore::new(temp_path(file), credentials.clone());
        (store, credentials)
    }

    async fn names(store: &CustomProviderStore) -> Vec<String> {
        store
            .entries()
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.name.to_string())
            .collect()
    }

    // Replies `response` to one connection, hands back the request.
    async fn serve(response: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut buffer).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&buffer[..read]);
            }
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap()
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
    async fn ensure_new_name_refuses_saved_and_builtin_names() {
        let (store, _credentials) = store("names");
        store.add(&entry("grok", "http://x"), None).await.unwrap();
        let registry = ProviderRegistry::new([provider("deepseek", &[])]);

        let saved = ensure_new_name(&store, &registry, &name("grok"))
            .await
            .unwrap_err();
        let builtin = ensure_new_name(&store, &registry, &name("deepseek"))
            .await
            .unwrap_err();

        assert_eq!(
            saved.to_string(),
            "grok already exists; change it with /provider edit"
        );
        assert_eq!(
            builtin.to_string(),
            "deepseek is a built-in provider; pick another name, e.g. deepseek-2"
        );
        assert!(
            ensure_new_name(&store, &registry, &name("groq"))
                .await
                .is_ok()
        );
        cleanup("names");
    }

    #[tokio::test]
    async fn add_refuses_a_saved_name_and_update_a_missing_one() {
        let (store, _credentials) = store("wrong-name");
        store.add(&entry("grok", "http://one"), None).await.unwrap();

        let added = store.add(&entry("grok", "http://two"), None).await;
        let updated = store.update(&entry("groq", "http://two"), None).await;

        assert_eq!(
            added.unwrap_err().to_string(),
            "grok already exists; change it with /provider edit"
        );
        assert_eq!(
            updated.unwrap_err().to_string(),
            "groq is not saved; add it with /provider add"
        );
        assert_eq!(store.entries().await.unwrap()[0].url, "http://one");
        cleanup("wrong-name");
    }

    #[tokio::test]
    async fn add_refuses_a_name_only_a_newer_alan_can_read() {
        let path = temp_path("newer-name");
        let newer = json!({"name": "grok", "url": "http://x", "api": "SomeFutureStandard"});
        JsonStore::new(path.clone())
            .set(PROVIDERS_KEY, json!([newer.clone()]))
            .await
            .unwrap();
        let store = CustomProviderStore::new(path.clone(), Arc::new(FailingKeys));

        let error = store
            .add(&entry("grok", "http://one"), Some("sk".into()))
            .await
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "grok already exists; change it with /provider edit"
        );
        let raw = JsonStore::new(path)
            .get(PROVIDERS_KEY)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(raw, json!([newer]));
        cleanup("newer-name");
    }

    #[tokio::test]
    async fn check_trims_the_url_and_lists_the_models() {
        let url = serve(OK_TWO_MODELS).await.0;

        let (entry, models) = check(name("grok"), ApiId::ChatCompletions, &url, None)
            .await
            .unwrap();

        assert_eq!(entry.url, url.trim_end_matches('/'));
        assert_eq!(models.len(), 2);
    }

    #[tokio::test]
    async fn check_reports_only_the_reason() {
        let url = serve("HTTP/1.1 401 Unauthorized\r\nconnection: close\r\n\r\n{}")
            .await
            .0;

        let error = check(name("grok"), ApiId::ChatCompletions, &url, Some("bad"))
            .await
            .unwrap_err();

        assert_eq!(
            format!("{error:#}"),
            "the API key was rejected (HTTP 401 Unauthorized)"
        );
    }

    #[tokio::test]
    async fn put_replaces_by_name_and_keeps_order() {
        let (store, _credentials) = store("put");
        for (n, url) in [
            ("a", "http://one"),
            ("b", "http://two"),
            ("a", "http://three"),
        ] {
            store.put(&entry(n, url)).await.unwrap();
        }
        let urls: Vec<_> = store
            .entries()
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
        let store =
            CustomProviderStore::new(path.clone(), Arc::new(InMemoryCredentialStore::new()));

        store.add(&entry("grok", "http://one"), None).await.unwrap();
        store.remove(&name("grok")).await.unwrap();
        store.add(&entry("groq", "http://two"), None).await.unwrap();

        assert_eq!(names(&store).await, ["groq"]);
        let raw = JsonStore::new(path)
            .get(PROVIDERS_KEY)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(raw.as_array().unwrap().len(), 2, "{raw}");
        cleanup("unreadable");
    }

    #[tokio::test]
    async fn add_saves_the_entry_and_key_and_remove_deletes_both() {
        let (store, _credentials) = store("add-remove");

        store
            .add(&entry("grok", "http://x/v1"), Some("sk".into()))
            .await
            .unwrap();
        assert_eq!(names(&store).await, ["grok"]);
        assert_eq!(
            store.key(&name("grok")).await.unwrap().as_deref(),
            Some("sk")
        );

        store.remove(&name("grok")).await.unwrap();
        assert!(names(&store).await.is_empty());
        assert!(store.key(&name("grok")).await.unwrap().is_none());
        cleanup("add-remove");
    }

    #[tokio::test]
    async fn add_leaves_no_key_when_providers_json_cannot_be_written() {
        // A directory where the file should be: every read and write fails.
        let path = temp_path("unwritable");
        std::fs::create_dir_all(&path).unwrap();
        let credentials = Arc::new(InMemoryCredentialStore::new());
        let store = CustomProviderStore::new(path.clone(), credentials.clone());

        let result = store
            .add(&entry("grok", "http://x"), Some("sk".into()))
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
        let store = CustomProviderStore::new(temp_path("key-fails"), Arc::new(FailingKeys));

        let result = store
            .add(&entry("grok", "http://x"), Some("sk".into()))
            .await;

        assert!(result.is_err());
        assert!(names(&store).await.is_empty());
        cleanup("key-fails");
    }

    #[tokio::test]
    async fn update_puts_the_old_entry_back_when_the_key_cannot_be_saved() {
        let store = CustomProviderStore::new(temp_path("edit-key-fails"), Arc::new(FailingKeys));
        store.put(&entry("grok", "http://one")).await.unwrap();

        let result = store
            .update(&entry("grok", "http://two"), Some("sk".into()))
            .await;

        assert!(result.is_err());
        assert_eq!(store.entries().await.unwrap()[0].url, "http://one");
        cleanup("edit-key-fails");
    }

    #[tokio::test]
    async fn remove_keeps_the_entry_when_the_key_cannot_be_deleted() {
        let store = CustomProviderStore::new(temp_path("delete-fails"), Arc::new(FailingKeys));
        store.put(&entry("grok", "http://one")).await.unwrap();

        assert!(store.remove(&name("grok")).await.is_err());
        assert_eq!(names(&store).await, ["grok"]);
        cleanup("delete-fails");
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
            Err(CredentialError::LockBusy)
        }
    }

    #[tokio::test]
    async fn update_without_a_key_deletes_the_saved_one() {
        let (store, _credentials) = store("update");
        store
            .add(&entry("grok", "http://one"), Some("old".into()))
            .await
            .unwrap();

        store
            .update(&entry("grok", "http://two"), None)
            .await
            .unwrap();

        assert!(store.key(&name("grok")).await.unwrap().is_none());
        assert_eq!(store.entries().await.unwrap()[0].url, "http://two");
        cleanup("update");
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
        let registry = ProviderRegistry::new([]);
        let store = CustomProviderStore::new(path, Arc::new(InMemoryCredentialStore::new()));

        let notices = load(&store, &registry).await.unwrap();

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
        let (store, credentials) = store("load-clash");
        for n in ["openai", "grok"] {
            store.put(&entry(n, "http://localhost:1/v1")).await.unwrap();
        }
        credentials
            .put(
                &name("openai").id(),
                Credential::ApiKey { key: "sk".into() },
            )
            .await
            .unwrap();
        let registry = ProviderRegistry::new([provider("openai", &["gpt"])]);

        let notices = load(&store, &registry).await.unwrap();

        assert_eq!(
            notices,
            [
                "custom provider openai (http://localhost:1/v1) was removed: a built-in provider now uses that name; add it again under another name"
            ]
        );
        assert_eq!(names(&store).await, ["grok"]);
        // The built-in keeps its place and its key.
        let openai = registry.get(&name("openai").id()).unwrap();
        assert_eq!(openai.models()[0].id, "gpt");
        assert!(registry.get(&name("grok").id()).is_some());
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
