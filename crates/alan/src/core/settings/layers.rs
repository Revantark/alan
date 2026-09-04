//! What each source said, and the order they apply in.
//!
//! Every field here is optional, where `None` means the source has no opinion
//! rather than that it chose the default. Applying them in precedence order
//! over [`Settings::default`] gives the values in force.

use super::Settings;
use llm::ReasoningEffort;
use serde::Deserialize;
use std::path::Path;

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

/// Which source supplied a value. Ordered lowest-precedence first.
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

/// Each source kept unfolded, so a single scope can be read back and a key's
/// origin found after the fold has already happened.
#[derive(Debug, Clone)]
pub struct Layers {
    global: SettingsLayer,
    project: SettingsLayer,
    env: SettingsLayer,
}

impl Layers {
    /// Reads every source. Errors are fatal at startup and non-fatal on
    /// reload; the caller decides which.
    pub(super) fn load(alan_dir: &Path, current_dir: &Path) -> anyhow::Result<Self> {
        let global_path = super::global_path(alan_dir);

        let project = match super::project_settings(current_dir, &global_path) {
            Some(path) => read_layer(&path)?,
            None => SettingsLayer::default(),
        };

        Ok(Self {
            global: read_layer(&global_path)?,
            project,
            env: env_layer()?,
        })
    }

    /// The values in force: every source applied over the defaults.
    pub(super) fn resolve(&self) -> Settings {
        self.resolve_as_of(Layer::Env)
    }

    /// What would apply in `scope` if nothing above it interfered — the same
    /// fold, stopped early.
    pub(super) fn resolve_as_of(&self, scope: Layer) -> Settings {
        let mut settings = Settings::default();

        if scope >= Layer::Global {
            settings = settings.layer(&self.global);
        }
        if scope >= Layer::Project {
            settings = settings.layer(&self.project);
        }
        if scope >= Layer::Env {
            settings = settings.layer(&self.env);
        }

        settings
    }

    /// Which layer supplied a key's effective value: the highest one with an
    /// opinion about it.
    pub(super) fn origin_layer(&self, key: &str) -> Layer {
        if self.env.has(key) {
            Layer::Env
        } else if self.project.has(key) {
            Layer::Project
        } else if self.global.has(key) {
            Layer::Global
        } else {
            Layer::Default
        }
    }
}

fn read_layer(path: &Path) -> anyhow::Result<SettingsLayer> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SettingsLayer::default());
        }
        Err(error) => anyhow::bail!("{}: {error}", path.display()),
    };
    serde_json::from_str(&text).map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))
}

/// `ALAN_HOME`, `ALAN_SESSION` and `ALAN_LOG*` are absent on purpose: they
/// decide where settings live, or apply to one run, so they are not settings.
fn env_layer() -> anyhow::Result<SettingsLayer> {
    Ok(SettingsLayer {
        model: read_var("ALAN_MODEL"),
        reasoning_effort: parse_var(
            "ALAN_REASONING_EFFORT",
            ReasoningEffort::parse,
            "one of auto, none, minimal, low, medium, high, xhigh, max",
        )?,
        tools: Some(ToolsLayer {
            web_search: parse_var("ALAN_OPENROUTER_WEB_SEARCH", parse_bool, "a boolean")?,
            web_fetch: parse_var("ALAN_OPENROUTER_WEB_FETCH", parse_bool, "a boolean")?,
        }),
    })
}

/// An unset or blank variable is no opinion.
fn parse_var<T>(
    name: &str,
    parser: impl Fn(&str) -> Option<T>,
    expected: &str,
) -> anyhow::Result<Option<T>> {
    let Some(raw) = read_var(name) else {
        return Ok(None);
    };

    parser(&raw.to_ascii_lowercase())
        .map(Some)
        .ok_or_else(|| anyhow::anyhow!("{name}: expected {expected}, got {raw:?}"))
}

fn read_var(name: &str) -> Option<String> {
    let value = std::env::var_os(name)?.to_string_lossy().trim().to_owned();
    (!value.is_empty()).then_some(value)
}

fn parse_bool(raw: &str) -> Option<bool> {
    match raw {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::storage::SETTINGS_FILE;
    use super::super::{DEFAULT_MODEL, Tools};
    use super::*;
    use std::path::PathBuf;

    fn layer(json: &str) -> SettingsLayer {
        serde_json::from_str(json).expect("valid layer")
    }

    /// A source with no opinion about anything.
    fn silent() -> SettingsLayer {
        SettingsLayer::default()
    }

    /// Removes the directory on drop, so a test leaves the filesystem as it
    /// found it even when it panics part-way through.
    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The directory is returned alongside the path: dropping it deletes both.
    fn temp_file(name: &str, contents: &str) -> (Scratch, PathBuf) {
        let dir = std::env::temp_dir().join(format!("alan-layer-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join(SETTINGS_FILE);
        std::fs::write(&path, contents).expect("write");
        (Scratch(dir), path)
    }

    #[test]
    fn defaults_apply_when_every_layer_is_silent() {
        let settings = Layers {
            global: silent(),
            project: silent(),
            env: silent(),
        }
        .resolve();
        assert_eq!(settings.model, DEFAULT_MODEL);
        assert_eq!(settings.reasoning_effort, ReasoningEffort::Auto);
        assert_eq!(settings.tools, Tools::default());
    }

    #[test]
    fn a_higher_layer_wins_per_field_not_per_layer() {
        let global = layer(r#"{"model":"opus","reasoning_effort":"high"}"#);
        let project = layer(r#"{"model":"haiku"}"#);

        let settings = Layers {
            global,
            project,
            env: silent(),
        }
        .resolve();
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

        let settings = Layers {
            global,
            project,
            env: silent(),
        }
        .resolve();
        assert!(!settings.tools.web_search);
        assert!(settings.tools.web_fetch, "web_fetch must survive");
    }

    /// The distinction the whole layer design exists for: silence is not the
    /// same as an explicit choice, even when the choice looks like a default.
    #[test]
    fn silence_and_an_explicit_none_resolve_differently() {
        let chosen = layer(r#"{"reasoning_effort":"none"}"#);

        let silence = Layers {
            global: silent(),
            project: silent(),
            env: silent(),
        }
        .resolve();
        let explicit = Layers {
            global: chosen,
            project: silent(),
            env: silent(),
        }
        .resolve();

        assert_eq!(silence.reasoning_effort, ReasoningEffort::Auto);
        assert_eq!(explicit.reasoning_effort, ReasoningEffort::None);
    }

    #[test]
    fn env_outranks_the_file() {
        let file = layer(r#"{"model":"from-file"}"#);
        let env = layer(r#"{"model":"from-env"}"#);

        let settings = Layers {
            global: file,
            project: silent(),
            env,
        }
        .resolve();
        assert_eq!(settings.model, "from-env");
    }

    #[test]
    fn a_missing_file_is_silent() {
        let read = read_layer(Path::new("/nonexistent/alan/settings.json"))
            .expect("not having a settings file is normal");
        assert_eq!(read, SettingsLayer::default());
    }

    #[test]
    fn a_valid_file_becomes_a_layer() {
        let (_dir, path) = temp_file("valid", r#"{"model":"opus","tools":{"web_search":true}}"#);
        let read = read_layer(&path).expect("valid file");

        assert_eq!(read.model.as_deref(), Some("opus"));
        assert_eq!(read.tools.unwrap().web_search, Some(true));
    }

    /// Starting with silently different settings is worse than not starting:
    /// you would see the default model and wonder why the file did nothing.
    #[test]
    fn malformed_json_refuses_to_start() {
        let (_dir, path) = temp_file("malformed", "{ this is not json");
        let error = read_layer(&path).expect_err("malformed config must be fatal");
        assert!(
            error.to_string().contains("settings.json"),
            "the message must name the file: {error}"
        );
    }

    /// A typo means the setting you wrote is not applied, so it is an error.
    /// The message names the valid keys, including inside a nested group.
    #[test]
    fn an_unknown_key_refuses_to_start_and_names_the_valid_ones() {
        let (_dir, path) = temp_file("unknown", r#"{"model":"opus","reasoning_efort":"high"}"#);
        let error = read_layer(&path)
            .expect_err("a typo must be fatal")
            .to_string();
        assert!(error.contains("reasoning_efort"), "{error}");
        assert!(
            error.contains("reasoning_effort"),
            "names the real key: {error}"
        );

        let (_dir, path) = temp_file("unknown-nested", r#"{"tools":{"web_serch":true}}"#);
        let error = read_layer(&path)
            .expect_err("a nested typo must be fatal")
            .to_string();
        assert!(error.contains("web_serch"), "{error}");
        assert!(error.contains("web_search"), "names the real key: {error}");
    }

    #[test]
    fn an_unset_variable_is_not_an_error_but_a_bad_one_is() {
        assert!(matches!(
            parse_var("ALAN_TEST_MISSING_VAR", parse_bool, "a boolean"),
            Ok(None)
        ));
        assert!(parse_var("PATH", |_| None::<bool>, "a boolean").is_err());
    }

    /// Values arrive case folded, so a parser matching lowercase is enough.
    #[test]
    fn a_set_variable_is_case_insensitive() {
        unsafe { std::env::set_var("ALAN_TEST_CASE", "ON") };
        assert_eq!(
            parse_var("ALAN_TEST_CASE", parse_bool, "a boolean").unwrap(),
            Some(true)
        );
        unsafe { std::env::remove_var("ALAN_TEST_CASE") };
    }
}
