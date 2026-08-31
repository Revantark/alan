//! The `ALAN_*` layer.

use super::{SettingsLayer, ToolsLayer};
use llm::ReasoningEffort;

/// `ALAN_HOME`, `ALAN_SESSION` and `ALAN_LOG*` are absent on purpose: they
/// decide where settings live, or apply to one run, so they are not settings.
pub(super) fn env_layer() -> anyhow::Result<SettingsLayer> {
    let tools = ToolsLayer {
        web_search: from_env("ALAN_OPENROUTER_WEB_SEARCH", parse_bool, "a boolean")?,
        web_fetch: from_env("ALAN_OPENROUTER_WEB_FETCH", parse_bool, "a boolean")?,
    };

    Ok(SettingsLayer {
        model: env_value("ALAN_MODEL"),
        reasoning_effort: from_env(
            "ALAN_REASONING_EFFORT",
            parse_effort,
            "one of auto, none, minimal, low, medium, high, xhigh, max",
        )?,
        // An all-empty `ToolsLayer` folds the same as no layer at all.
        tools: Some(tools),
    })
}

fn from_env<T>(
    name: &str,
    parse: impl Fn(&str) -> Option<T>,
    expected: &str,
) -> anyhow::Result<Option<T>> {
    let Some(raw) = env_value(name) else {
        return Ok(None);
    };
    parse(&raw)
        .map(Some)
        .ok_or_else(|| anyhow::anyhow!("{name}: expected {expected}, got {raw:?}"))
}

fn env_value(name: &str) -> Option<String> {
    let value = std::env::var_os(name)?.to_string_lossy().trim().to_owned();
    (!value.is_empty()).then_some(value)
}

fn parse_bool(raw: &str) -> Option<bool> {
    match raw.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn parse_effort(raw: &str) -> Option<ReasoningEffort> {
    ReasoningEffort::parse(&raw.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unset_variable_is_not_an_error_but_a_bad_one_is() {
        assert!(matches!(
            from_env("ALAN_TEST_MISSING_VAR", parse_bool, "a boolean"),
            Ok(None)
        ));
        assert!(from_env("PATH", |_| None::<bool>, "a boolean").is_err());

        assert_eq!(parse_bool("nope"), None);
        assert_eq!(parse_bool("ON"), Some(true));
    }
}
