//! Settings management for Alan.
//!
//! Settings are persisted in a JSON file at `ALAN_HOME/.alan/settings.json`.
//! Environment variables override stored settings with the following priority:
//!   env > settings.json > default (None)
//!
//! Supported environment variables:
//!   - ALAN_MODEL
//!   - ALAN_OPENROUTER_WEB_FETCH
//!   - ALAN_OPENROUTER_WEB_SEARCH
//!   - ALAN_REASONING_EFFORT
//!   - ALAN_OR_MODEL_PROVIDER
//!   - ALAN_PROVIDER

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::core::permissions;
use crate::core::store::JsonStore;
use llm::ReasoningEffort;
use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::path::PathBuf;

/// Public Settings representing current configuration. All fields are optional so
/// that we can distinguish between a field that has not been provided by the
/// environment/UI (None) and one that has been overridden (Some).
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Settings {
    pub model: Option<String>,
    pub web_fetch: Option<bool>,
    pub web_search: Option<bool>,
    pub reasoning: Option<ReasoningEffort>,
    /// Provider order scoped per model id. Absent entry means default
    /// (OpenRouter routing decides).
    #[serde(default)]
    pub provider_orders: BTreeMap<String, Vec<String>>,
    pub provider: Option<String>,
    /// Tool-permission policy persisted across runs. Absent means the
    /// default (Strict).
    #[serde(default)]
    pub tool_policy: Option<permissions::Policy>,
}

/// A patch type used to update only selected fields of Settings without replacing
/// the entire object.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct PatchSettings {
    pub model: Option<String>,
    pub web_fetch: Option<bool>,
    pub web_search: Option<bool>,
    pub reasoning: Option<ReasoningEffort>,
    /// Per-model provider order overrides, merged per key on apply.
    pub provider_orders: Option<BTreeMap<String, Vec<String>>>,
    pub provider: Option<String>,
    pub tool_policy: Option<permissions::Policy>,
}

/// SettingsStore provides load/save access to application settings stored in a
/// JsonStore. It is generic over the concrete Settings type, allowing the same
/// storage mechanism to be reused for different settings shapes.
pub struct SettingsStore<S> {
    store: JsonStore,
    key: String,
    _phantom: PhantomData<S>,
}

impl<S> SettingsStore<S> {
    /// Create a new SettingsStore backed by the given file path. Uses a fixed
    /// key within the json object to store the Settings.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            store: JsonStore::new(path),
            key: "settings".to_string(),
            _phantom: PhantomData,
        }
    }

    /// Load the Settings from the json store. Returns None if the key is missing.
    pub async fn load(&self) -> Result<Option<S>>
    where
        S: for<'de> Deserialize<'de>,
    {
        if let Some(value) = self.store.get(&self.key).await? {
            let settings: S = serde_json::from_value(value)?;
            Ok(Some(settings))
        } else {
            Ok(None)
        }
    }

    /// Save the provided Settings into the json store.
    pub async fn save(&self, settings: &S) -> Result<()>
    where
        S: Serialize,
    {
        let value = serde_json::to_value(settings)?;
        self.store.set(&self.key, value).await
    }
}

/// Default location of settings.json: `$ALAN_HOME/.alan/settings.json`
/// (falling back to `$HOME/.alan`).
pub fn default_settings_path() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("ALAN_HOME")
        .or_else(|| std::env::var_os("HOME"))
        .ok_or_else(|| anyhow::anyhow!("cannot determine Alan home directory"))?;
    Ok(PathBuf::from(home).join(".alan").join("settings.json"))
}

/// The fallback model used when no model is stored or provided via env.
pub const DEFAULT_MODEL: &str = "openai/gpt-4o-mini";
pub const DEFAULT_PROVIDER: &str = "openrouter";

impl Settings {
    /// Settings used on first run (and to seed a missing settings.json):
    /// the hardcoded fallback model and a low reasoning effort.
    pub fn with_defaults() -> Self {
        Self {
            model: Some(DEFAULT_MODEL.into()),
            reasoning: Some(ReasoningEffort::Low),
            provider: Some(DEFAULT_PROVIDER.into()),
            ..Self::default()
        }
    }
}

impl Settings {
    /// The provider order configured for `model`, empty when unset.
    pub fn provider_order(&self, model: &str) -> Vec<String> {
        self.provider_orders.get(model).cloned().unwrap_or_default()
    }
}

/// Apply a PatchSettings to mutate only specified fields.
impl Settings {
    pub fn apply_patch(&mut self, patch: PatchSettings) {
        if let Some(v) = patch.model {
            self.model = Some(v);
        }
        if let Some(v) = patch.web_fetch {
            self.web_fetch = Some(v);
        }
        if let Some(v) = patch.web_search {
            self.web_search = Some(v);
        }
        if let Some(v) = patch.reasoning {
            self.reasoning = Some(v);
        }
        if let Some(v) = patch.provider_orders {
            for (model, order) in v {
                self.provider_orders.insert(model, order);
            }
        }
        if let Some(v) = patch.provider {
            self.provider = Some(v);
        }
        if let Some(v) = patch.tool_policy {
            self.tool_policy = Some(v);
        }
    }
}

pub async fn get_settings() -> anyhow::Result<Settings> {
    let store = SettingsStore::<Settings>::new(default_settings_path()?);
    Ok(store.load().await?.unwrap_or_default())
}
