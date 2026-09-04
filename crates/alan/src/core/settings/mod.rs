//! Layered settings: defaults, global file, project file, env — highest wins.
//!
//! Every field in a [`SettingsLayer`] is optional, where `None` means the
//! source has no opinion rather than that it chose the default. [`Settings`] is
//! the folded result and has no optionals.

mod controller;
mod layers;
mod overlay;
mod schema;
mod storage;

pub use controller::{Outcome, SettingsController};
pub use layers::Layer;
use layers::{Layers, SettingsLayer};
use overlay::SettingsOverlay;
pub use overlay::{Marker, SettingsState};
use schema::{Kind, SETTINGS, SettingDef, Value};
use storage::{global_path, project_settings, project_target, repo_root, write_key};

use llm::{ReasoningEffort, ServerTool};
use providers::{Model, ModelOptions, ProviderRegistry};

const DEFAULT_MODEL: &str = "openai/gpt-4o-mini";

/// The values in force, with every source folded in.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub tools: Tools,
    pub model: String,
    pub reasoning_effort: ReasoningEffort,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tools {
    pub web_search: bool,
    pub web_fetch: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            tools: Tools::default(),
            model: DEFAULT_MODEL.to_owned(),
            reasoning_effort: ReasoningEffort::default(),
        }
    }
}

impl Settings {
    fn layer(mut self, higher: &SettingsLayer) -> Self {
        if let Some(model) = &higher.model {
            self.model = model.clone();
        }

        if let Some(effort) = higher.reasoning_effort {
            self.reasoning_effort = effort;
        }

        if let Some(tools) = higher.tools {
            if let Some(on) = tools.web_search {
                self.tools.web_search = on;
            }
            if let Some(on) = tools.web_fetch {
                self.tools.web_fetch = on;
            }
        }

        self
    }

    /// Turns the values in force into something that can serve a request.
    /// Used by startup and by live rebinding, so the two cannot disagree.
    pub fn bind(&self, providers: &ProviderRegistry) -> anyhow::Result<Model> {
        let provider = providers
            .providers()
            .first()
            .ok_or_else(|| anyhow::anyhow!("no provider configured"))?;

        let options = ModelOptions {
            server_tools: provider
                .server_tools()
                .iter()
                .filter(|tool| match tool.id.as_str() {
                    "openrouter:web_fetch" => self.tools.web_fetch,
                    "openrouter:web_search" => self.tools.web_search,
                    _ => false,
                })
                .map(|tool| ServerTool {
                    kind: tool.id.clone(),
                })
                .collect(),
            reasoning_effort: self.reasoning_effort,
        };

        Ok(provider.bind_with_options(&self.model, options)?)
    }
}
