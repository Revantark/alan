//! Layered settings: defaults, global file, project file, env — highest wins.
//!
//! Every field in a [`SettingsLayer`] is optional, where `None` means the
//! source has no opinion rather than that it chose the default. [`Settings`] is
//! the folded result and has no optionals.

mod controller;
mod env;
mod files;
mod layers;
mod overlay;
mod schema;

pub use controller::{Outcome, SettingsController};
use files::{
    global_path, load_settings_layers, project_settings, project_target, repo_root, write_key,
};
pub use layers::Layer;
use layers::{Layers, SettingsLayer, ToolsLayer};
pub use overlay::{Marker, SettingsOverlay};
use schema::{Kind, SETTINGS, SettingDef, Value};

use llm::{ReasoningEffort, ServerTool};
use providers::{Model, ModelOptions, ProviderRegistry};

const DEFAULT_MODEL: &str = "openai/gpt-4o-mini";

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub model: String,
    pub reasoning_effort: ReasoningEffort,
    pub tools: Tools,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tools {
    pub web_search: bool,
    pub web_fetch: bool,
}

impl Default for Settings {
    fn default() -> Self {
        SettingsLayer::default().into()
    }
}

/// Used by startup and by live rebinding, so the two cannot disagree.
pub fn bind(providers: &ProviderRegistry, settings: &Settings) -> anyhow::Result<Model> {
    let provider = providers
        .providers()
        .first()
        .ok_or_else(|| anyhow::anyhow!("no provider configured"))?;

    let options = ModelOptions {
        server_tools: provider
            .server_tools()
            .iter()
            .filter(|tool| match tool.id.as_str() {
                "openrouter:web_fetch" => settings.tools.web_fetch,
                "openrouter:web_search" => settings.tools.web_search,
                _ => false,
            })
            .map(|tool| ServerTool {
                kind: tool.id.clone(),
            })
            .collect(),
        reasoning_effort: settings.reasoning_effort,
    };

    Ok(provider.bind_with_options(&settings.model, options)?)
}
