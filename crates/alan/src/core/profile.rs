//! Named profiles: snapshots of settings that are applied together.
//!
//! Profiles are persisted in a separate JSON file at
//! the Alan data directory so that the catalog can grow independently
//! from the frequently changed settings.

use anyhow::Result;
use llm::ReasoningEffort;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::core::paths;
use crate::core::store::JsonStore;

/// A named snapshot of the settings that profiles switch together.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Profile {
    pub provider: String,
    pub model: String,
    pub reasoning: ReasoningEffort,
    pub web_fetch: bool,
    pub web_search: bool,
}

const PROFILES_KEY: &str = "profiles";

/// Atomically persisted named profiles, kept separate from frequently changed
/// settings so the catalog can grow independently.
pub struct ProfileStore {
    store: JsonStore,
}

impl ProfileStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            store: JsonStore::new(path),
        }
    }

    pub async fn load(&self) -> Result<BTreeMap<String, Profile>> {
        decode_profiles(self.store.get(PROFILES_KEY).await?)
    }

    pub async fn save_new(&self, name: &str, profile: Profile) -> Result<()> {
        let name = validate_profile_name(name)?;

        self.update(|profiles| {
            anyhow::ensure!(
                !profiles.contains_key(&name),
                "profile already exists: {name}"
            );
            profiles.insert(name.to_owned(), profile);

            Ok(true)
        })
        .await?;

        Ok(())
    }

    pub async fn delete(&self, name: &str) -> Result<bool> {
        let name = validate_profile_name(name)?;

        self.update(|profiles| Ok(profiles.remove(&name).is_some()))
            .await
    }

    /// Mutate the stored map, reporting the closure's outcome. The key is
    /// dropped entirely once the map is empty, so the file never carries an
    /// empty object.
    async fn update(
        &self,
        f: impl FnOnce(&mut BTreeMap<String, Profile>) -> Result<bool>,
    ) -> Result<bool> {
        self.store
            .update(PROFILES_KEY, move |value| {
                let mut profiles = decode_profiles(value)?;
                let outcome = f(&mut profiles)?;

                let replacement = if profiles.is_empty() {
                    None
                } else {
                    Some(serde_json::to_value(profiles)?)
                };

                Ok((replacement, outcome))
            })
            .await
    }
}

fn decode_profiles(value: Option<serde_json::Value>) -> Result<BTreeMap<String, Profile>> {
    Ok(value
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default())
}

fn validate_profile_name(name: &str) -> Result<String> {
    let name = name.trim();
    anyhow::ensure!(!name.is_empty(), "profile name must not be empty");
    Ok(name.to_owned())
}

/// Default location of profiles.json inside the Alan data directory.
pub fn default_profiles_path() -> Result<PathBuf> {
    paths::data_file("profiles.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_profile(model: &str) -> Profile {
        Profile {
            provider: "openrouter".to_owned(),
            model: model.to_owned(),
            reasoning: ReasoningEffort::High,
            web_fetch: true,
            web_search: false,
        }
    }

    fn profile_store_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("alan-profiles-{}-{label}.json", std::process::id()))
    }

    #[tokio::test]
    async fn profile_store_saves_and_deletes_profiles() {
        let path = profile_store_path("crud");
        let store = ProfileStore::new(&path);

        store
            .save_new("GPT", test_profile("gpt-model"))
            .await
            .unwrap();
        assert!(
            store
                .save_new("GPT", test_profile("duplicate"))
                .await
                .is_err()
        );
        assert_eq!(
            store.load().await.unwrap()["GPT"],
            test_profile("gpt-model")
        );

        assert!(store.delete("GPT").await.unwrap());
        assert!(!store.delete("GPT").await.unwrap());
        assert!(store.load().await.unwrap().is_empty());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}.lock", path.display()));
    }

    /// Both mutators reject blank names, and names are trimmed before use.
    #[tokio::test]
    async fn mutators_validate_the_profile_name() {
        let path = profile_store_path("validate");
        let store = ProfileStore::new(&path);

        assert!(store.save_new("  ", test_profile("m")).await.is_err());
        assert!(store.delete("   ").await.is_err());
        assert!(store.load().await.unwrap().is_empty());

        store
            .save_new("  spaced  ", test_profile("m"))
            .await
            .unwrap();
        assert!(store.load().await.unwrap().contains_key("spaced"));
        assert!(store.delete("spaced").await.unwrap());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}.lock", path.display()));
    }
}
