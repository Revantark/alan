//! Layers and how they fold.
//!
//! Every field in a [`SettingsLayer`] is optional, where `None` means the source
//! has no opinion rather than that it chose the default. Folding in precedence
//! order gives [`Settings`](super::Settings), which has no optionals.

use super::{DEFAULT_MODEL, Settings, Tools};
use llm::ReasoningEffort;
use serde::Deserialize;

/// Which source supplied a value. Ordered lowest-precedence first, so a value
/// coming from a *lower* layer than the one being edited can be taken over,
/// and one from a *higher* layer cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    Default,
    Global,
    Project,
    Env,
}

impl Layer {
    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Global => "global",
            Self::Project => "project",
            Self::Env => "env",
        }
    }
}

/// Every source in precedence order, unfolded so a single scope can be read.
#[derive(Debug, Clone)]
pub struct Layers(Vec<(Layer, SettingsLayer)>);

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SettingsLayer {
    pub model: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub tools: Option<ToolsLayer>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ToolsLayer {
    pub web_search: Option<bool>,
    pub web_fetch: Option<bool>,
}

impl SettingsLayer {
    /// Nested groups merge field-wise, so setting one key under `tools` does
    /// not erase the others.
    fn overlay(self, higher: SettingsLayer) -> SettingsLayer {
        SettingsLayer {
            model: higher.model.or(self.model),
            reasoning_effort: higher.reasoning_effort.or(self.reasoning_effort),
            tools: match (self.tools, higher.tools) {
                (Some(low), Some(high)) => Some(ToolsLayer {
                    web_search: high.web_search.or(low.web_search),
                    web_fetch: high.web_fetch.or(low.web_fetch),
                }),
                (low, high) => high.or(low),
            },
        }
    }

    pub(super) fn has(&self, key: &str) -> bool {
        match key {
            "model" => self.model.is_some(),
            "reasoning_effort" => self.reasoning_effort.is_some(),
            "tools.web_search" => self.tools.is_some_and(|t| t.web_search.is_some()),
            "tools.web_fetch" => self.tools.is_some_and(|t| t.web_fetch.is_some()),
            _ => false,
        }
    }
}

impl From<SettingsLayer> for Settings {
    fn from(layer: SettingsLayer) -> Self {
        let tools = layer.tools.unwrap_or_default();
        Self {
            model: layer.model.unwrap_or_else(|| DEFAULT_MODEL.to_owned()),
            reasoning_effort: layer.reasoning_effort.unwrap_or_default(),
            tools: Tools {
                web_search: tools.web_search.unwrap_or(false),
                web_fetch: tools.web_fetch.unwrap_or(false),
            },
        }
    }
}

impl Layers {
    /// Takes each source by name, so precedence is decided here rather than at
    /// every construction site.
    pub(super) fn new(global: SettingsLayer, project: SettingsLayer, env: SettingsLayer) -> Self {
        Self(vec![
            (Layer::Global, global),
            (Layer::Project, project),
            (Layer::Env, env),
        ])
    }

    /// The values in force: every layer folded, lowest precedence first.
    pub(super) fn resolve(&self) -> Settings {
        fold(self.0.iter())
    }

    /// What would apply in `scope` if nothing above it interfered.
    pub(super) fn resolve_as_of(&self, scope: Layer) -> Settings {
        fold(self.0.iter().filter(|(source, _)| *source <= scope))
    }

    /// Which layer supplied a key's effective value: the highest one with an
    /// opinion about it.
    pub(super) fn origin_of(&self, key: &str) -> Layer {
        self.0
            .iter()
            .rev()
            .find(|(_, layer)| layer.has(key))
            .map_or(Layer::Default, |(source, _)| *source)
    }
}

fn fold<'a>(layers: impl Iterator<Item = &'a (Layer, SettingsLayer)>) -> Settings {
    layers
        .fold(SettingsLayer::default(), |merged, (_, layer)| {
            merged.overlay(layer.clone())
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(json: &str) -> SettingsLayer {
        serde_json::from_str(json).expect("valid layer")
    }

    /// A source with no opinion about anything.
    fn silent() -> SettingsLayer {
        SettingsLayer::default()
    }

    #[test]
    fn defaults_apply_when_every_layer_is_silent() {
        let settings = Layers::new(silent(), silent(), silent()).resolve();
        assert_eq!(settings.model, DEFAULT_MODEL);
        assert_eq!(settings.reasoning_effort, ReasoningEffort::Auto);
        assert_eq!(settings.tools, Tools::default());
    }

    #[test]
    fn a_higher_layer_wins_per_field_not_per_layer() {
        let global = layer(r#"{"model":"opus","reasoning_effort":"high"}"#);
        let project = layer(r#"{"model":"haiku"}"#);

        let settings = Layers::new(global, project, silent()).resolve();
        assert_eq!(settings.model, "haiku");
        // Untouched by the higher layer, so the lower one still applies.
        assert_eq!(settings.reasoning_effort, ReasoningEffort::High);
    }

    /// Without a field-wise merge, a project setting one tool would silently
    /// switch the other back off.
    #[test]
    fn nested_groups_merge_rather_than_replace() {
        let global = layer(r#"{"tools":{"web_search":true,"web_fetch":true}}"#);
        let project = layer(r#"{"tools":{"web_search":false}}"#);

        let settings = Layers::new(global, project, silent()).resolve();
        assert!(!settings.tools.web_search);
        assert!(settings.tools.web_fetch, "web_fetch must survive");
    }

    /// The distinction the whole layer design exists for: silence is not the
    /// same as an explicit choice, even when the choice looks like a default.
    #[test]
    fn silence_and_an_explicit_none_resolve_differently() {
        let chosen = layer(r#"{"reasoning_effort":"none"}"#);

        let silence = Layers::new(silent(), silent(), silent()).resolve();
        let explicit = Layers::new(chosen, silent(), silent()).resolve();

        assert_eq!(silence.reasoning_effort, ReasoningEffort::Auto);
        assert_eq!(explicit.reasoning_effort, ReasoningEffort::None);
    }

    #[test]
    fn env_outranks_the_file() {
        let file = layer(r#"{"model":"from-file"}"#);
        let env = layer(r#"{"model":"from-env"}"#);

        let settings = Layers::new(file, silent(), env).resolve();
        assert_eq!(settings.model, "from-env");
    }
}
