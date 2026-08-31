//! One declaration per setting, read by the `/settings` list, validation, and
//! the project-scope guard.

use super::Settings;
use llm::ReasoningEffort;

/// `Bool` and `Enum` cycle on a keypress; the rest open a prompt.
pub enum Kind {
    Bool,
    Enum(&'static [&'static str]),
    Text,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Bool(bool),
    Text(String),
    Enum(&'static str),
}

pub struct SettingDef {
    /// Dotted path into the settings JSON object.
    pub key: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub kind: Kind,
    /// May a project's `.alan/settings.json` set this? `false` for anything
    /// that can execute code, change an endpoint, or touch credentials.
    pub project_safe: bool,
    pub read: fn(&Settings) -> Value,
}

/// Spelled by the enum itself, so a renamed level cannot desync from the list
/// the `/settings` row cycles through.
const EFFORTS: &[&str] = &[
    ReasoningEffort::Auto.as_str(),
    ReasoningEffort::None.as_str(),
    ReasoningEffort::Minimal.as_str(),
    ReasoningEffort::Low.as_str(),
    ReasoningEffort::Medium.as_str(),
    ReasoningEffort::High.as_str(),
    ReasoningEffort::XHigh.as_str(),
    ReasoningEffort::Max.as_str(),
];

pub const SETTINGS: &[SettingDef] = &[
    SettingDef {
        key: "model",
        label: "model",
        help: "Provider model id used for new sessions.",
        kind: Kind::Text,
        project_safe: true,
        read: |s| Value::Text(s.model.clone()),
    },
    SettingDef {
        key: "reasoning_effort",
        label: "reasoning effort",
        help: "Hidden thinking spent before answering. `auto` defers to the model; `none` disables it.",
        kind: Kind::Enum(EFFORTS),
        project_safe: true,
        read: |s| Value::Enum(s.reasoning_effort.as_str()),
    },
    SettingDef {
        key: "tools.web_search",
        label: "web search",
        help: "Offer the provider's web search tool.",
        kind: Kind::Bool,
        project_safe: true,
        read: |s| Value::Bool(s.tools.web_search),
    },
    SettingDef {
        key: "tools.web_fetch",
        label: "web fetch",
        help: "Offer the provider's web fetch tool.",
        kind: Kind::Bool,
        project_safe: true,
        read: |s| Value::Bool(s.tools.web_fetch),
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_unique() {
        let mut keys: Vec<_> = SETTINGS.iter().map(|def| def.key).collect();
        keys.sort_unstable();
        let count = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), count, "duplicate key in SETTINGS");
    }

    /// Or the UI could offer a value the resolver rejects.
    #[test]
    fn enum_options_all_parse() {
        for def in SETTINGS {
            let Kind::Enum(options) = def.kind else {
                continue;
            };
            for option in options {
                assert!(
                    ReasoningEffort::parse(option).is_some(),
                    "{}: option {option:?} does not parse",
                    def.key
                );
            }
        }
    }
}
