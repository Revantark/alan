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

use crate::core::paths;
use crate::core::permissions;
use crate::core::store::JsonStore;

const SETTINGS_KEY: &str = "settings";
use anyhow::Context as _;
use llm::ReasoningEffort;
use std::collections::BTreeMap;
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
    /// Name of the last profile applied in this process. Cleared by manual
    /// settings changes; profile definitions are stored separately.
    #[serde(default)]
    pub active_profile: Option<String>,
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
/// JsonStore, under a fixed key within the json object.
pub struct SettingsStore {
    store: JsonStore,
    key: String,
}

impl SettingsStore {
    /// Create a new SettingsStore backed by the given file path. Uses a fixed
    /// key within the json object to store the Settings.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            store: JsonStore::new(path),
            key: SETTINGS_KEY.to_string(),
        }
    }

    /// Load the Settings from the json store. Returns None if the key is missing.
    pub async fn load(&self) -> Result<Option<Settings>> {
        if let Some(value) = self.store.get(&self.key).await? {
            let settings: Settings = serde_json::from_value(value)?;
            Ok(Some(settings))
        } else {
            Ok(None)
        }
    }

    /// Save the provided Settings into the json store.
    pub async fn save(&self, settings: &Settings) -> Result<()> {
        let value = serde_json::to_value(settings)?;
        self.store.set(&self.key, value).await
    }

    /// Atomically read, modify, and write the stored settings while holding
    /// the store's file lock. The closure receives the current settings
    /// (defaults when the key is missing) and returns the replacement.
    pub async fn update<F>(&self, f: F) -> Result<()>
    where
        F: FnOnce(Settings) -> Settings,
    {
        self.store
            .update(&self.key, |value| {
                let settings: Settings = match value {
                    Some(value) => {
                        serde_json::from_value(value).context("failed to parse stored settings")?
                    }
                    None => Settings::default(),
                };
                let updated = f(settings);
                let value = serde_json::to_value(updated)?;
                Ok((Some(value), ()))
            })
            .await
    }
}

/// Default location of settings.json inside the Alan data directory.
pub fn default_settings_path() -> Result<PathBuf> {
    paths::data_file("settings.json")
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
        let mut changed = false;
        if let Some(v) = patch.model {
            changed |= self.model.as_ref() != Some(&v);
            self.model = Some(v);
        }
        if let Some(v) = patch.web_fetch {
            changed |= self.web_fetch != Some(v);
            self.web_fetch = Some(v);
        }
        if let Some(v) = patch.web_search {
            changed |= self.web_search != Some(v);
            self.web_search = Some(v);
        }
        if let Some(v) = patch.reasoning {
            changed |= self.reasoning != Some(v);
            self.reasoning = Some(v);
        }
        if let Some(v) = patch.provider_orders {
            for (model, order) in v {
                changed |= self.provider_orders.get(&model) != Some(&order);
                self.provider_orders.insert(model, order);
            }
        }
        if let Some(v) = patch.provider {
            changed |= self.provider.as_ref() != Some(&v);
            self.provider = Some(v);
        }
        if let Some(v) = patch.tool_policy {
            changed |= self.tool_policy != Some(v);
            self.tool_policy = Some(v);
        }
        if changed {
            self.active_profile = None;
        }
    }
}

pub async fn get_settings() -> anyhow::Result<Settings> {
    let store = SettingsStore::new(default_settings_path()?);
    Ok(store.load().await?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("alan-profiles-{}-{label}.json", std::process::id()))
    }

    #[test]
    fn settings_deserializes_without_new_profile_marker() {
        let settings: Settings = serde_json::from_value(serde_json::json!({
            "model": "test-model",
            "provider": "openrouter"
        }))
        .unwrap();
        assert_eq!(settings.model.as_deref(), Some("test-model"));
        assert_eq!(settings.active_profile, None);
    }

    #[test]
    fn settings_patch_clears_active_profile_only_on_a_change() {
        let mut settings = Settings {
            model: Some("model-a".to_owned()),
            active_profile: Some("gpt".to_owned()),
            ..Settings::default()
        };
        settings.apply_patch(PatchSettings {
            model: Some("model-a".to_owned()),
            ..PatchSettings::default()
        });
        assert_eq!(settings.active_profile.as_deref(), Some("gpt"));

        settings.apply_patch(PatchSettings {
            reasoning: Some(ReasoningEffort::High),
            ..PatchSettings::default()
        });
        assert_eq!(settings.active_profile, None);
    }

    #[tokio::test]
    async fn settings_store_update_read_modifies_and_writes_atomically() {
        let path = test_store_path("settings-update");
        let store = SettingsStore::new(&path);

        // Missing file: update sees defaults and writes the result.
        store
            .update(|mut settings| {
                assert_eq!(settings.model, None);
                assert_eq!(settings.active_profile, None);
                settings.model = Some("m1".to_owned());
                settings
            })
            .await
            .unwrap();

        // Existing file: update sees the previous value, not defaults.
        store
            .update(|mut settings| {
                assert_eq!(settings.model.as_deref(), Some("m1"));
                settings.reasoning = Some(ReasoningEffort::High);
                settings
            })
            .await
            .unwrap();

        let reloaded = SettingsStore::new(&path);
        let settings = reloaded.load().await.unwrap().unwrap();
        assert_eq!(settings.model.as_deref(), Some("m1"));
        assert_eq!(settings.reasoning, Some(ReasoningEffort::High));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}.lock", path.display()));
    }
}
