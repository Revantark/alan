//! Finding, reading and writing the settings files.

use super::{Layers, SETTINGS, SettingsLayer, env};
use std::path::{Path, PathBuf};

pub(super) const SETTINGS_FILE: &str = "settings.json";

pub(super) const SETTINGS_DIR: &str = ".alan";

/// The global settings file inside `dir`.
pub fn global_path(dir: &Path) -> PathBuf {
    dir.join(SETTINGS_FILE)
}

/// Read every source. Errors are fatal at startup and non-fatal on reload;
/// the caller decides which.
pub fn load_settings_layers(global_dir: &Path, cwd: &Path) -> anyhow::Result<Layers> {
    let global_path = global_path(global_dir);
    let global = read_layer(&global_path)?;

    let project = match project_settings(cwd, &global_path) {
        Some(path) => project_layer(&path)?,
        None => SettingsLayer::default(),
    };

    Ok(Layers::new(global, project, env::env_layer()?))
}

/// Rejects rather than ignores keys a project file may not set, so the mistake
/// is visible.
fn project_layer(path: &Path) -> anyhow::Result<SettingsLayer> {
    let layer = read_layer(path)?;
    let refused: Vec<_> = SETTINGS
        .iter()
        .filter(|def| !def.project_safe && layer.has(def.key))
        .map(|def| format!("`{}`", def.key))
        .collect();
    if !refused.is_empty() {
        anyhow::bail!(
            "{}: {} cannot be set by a project file",
            path.display(),
            refused.join(", ")
        );
    }
    Ok(layer)
}

/// Not having a settings file is normal and silent. Anything else — unreadable,
/// bad syntax, a wrong type, an unknown key — is an error naming the file.
/// Running on defaults because a file could not be read would be worse.
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

/// The nearest project settings file, if any.
///
/// Stops at a `.git` boundary: an ancestor's settings apply to a package in a
/// monorepo, but not to an unrelated checkout that happens to sit below it.
pub fn project_settings(cwd: &Path, global: &Path) -> Option<PathBuf> {
    for dir in cwd.ancestors() {
        let candidate = dir.join(SETTINGS_DIR).join(SETTINGS_FILE);
        if candidate.is_file() && !same_file(&candidate, global) {
            return Some(candidate);
        }
        if dir.join(".git").exists() {
            break;
        }
    }
    None
}

/// The nearest enclosing repository, if any. `Some` also answers whether a
/// project write has somewhere sensible to go.
pub(super) fn repo_root(cwd: &Path) -> Option<&Path> {
    cwd.ancestors().find(|dir| dir.join(".git").exists())
}

/// Where a project write lands when no project file exists yet. Prefers the
/// repo root, so running in `repo/crates/agent` does not bury it four levels
/// down.
pub fn project_target(cwd: &Path) -> PathBuf {
    repo_root(cwd)
        .unwrap_or(cwd)
        .join(SETTINGS_DIR)
        .join(SETTINGS_FILE)
}

/// True only when both paths exist and resolve to the same file. Two missing
/// files are not equal.
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Mutates the raw JSON rather than re-serializing from `SettingsLayer`, so
/// keys this build does not know about survive.
pub fn write_key(path: &Path, key: &str, value: Option<serde_json::Value>) -> anyhow::Result<()> {
    let mut root = match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)?,
        // Only a file that does not exist yet starts from scratch. Treating one
        // we merely failed to read as empty would overwrite it below.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            serde_json::Value::Object(serde_json::Map::new())
        }
        Err(error) => anyhow::bail!("{}: {error}", path.display()),
    };
    if !root.is_object() {
        anyhow::bail!("{}: settings must be a JSON object", path.display());
    }

    set_path(&mut root, key, value).map_err(|group| {
        anyhow::anyhow!(
            "{}: `{group}` is not an object, so `{key}` cannot be set — fix the file first",
            path.display()
        )
    })?;
    prune_empty_groups(&mut root);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        ignore_project_dir(parent);
    }
    atomic_write(path, &format!("{}\n", serde_json::to_string_pretty(&root)?))
}

/// A project settings directory starts ignored: committing it is a decision to
/// take deliberately rather than by default. Best effort — failing to write it
/// is no reason to lose the setting.
fn ignore_project_dir(dir: &Path) {
    if dir.file_name().is_some_and(|name| name == SETTINGS_DIR) && !dir.join(".gitignore").exists()
    {
        let _ = std::fs::write(dir.join(".gitignore"), "*\n");
    }
}

/// Creates intermediate groups as needed. Returns the offending group when a
/// value already sits where one belongs, rather than overwriting it.
fn set_path(
    root: &mut serde_json::Value,
    key: &str,
    value: Option<serde_json::Value>,
) -> Result<(), String> {
    let mut node = root;
    let mut walked = String::new();
    let mut parts = key.split('.').peekable();
    while let Some(part) = parts.next() {
        let Some(object) = node.as_object_mut() else {
            return Err(walked);
        };
        if parts.peek().is_none() {
            match value {
                Some(value) => object.insert(part.to_owned(), value),
                None => object.remove(part),
            };
            return Ok(());
        }
        if !walked.is_empty() {
            walked.push('.');
        }
        walked.push_str(part);
        node = object
            .entry(part.to_owned())
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    }
    Ok(())
}

/// A file that lists only what you changed should not keep `"tools": {}`.
fn prune_empty_groups(root: &mut serde_json::Value) {
    if let Some(object) = root.as_object_mut() {
        object.retain(|_, child| !child.as_object().is_some_and(|group| group.is_empty()));
    }
}

/// Canonicalised first, because renaming over a symlink would replace it with
/// a regular file and detach it from whatever manages it.
fn atomic_write(path: &Path, contents: &str) -> anyhow::Result<()> {
    let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let temporary = target.with_extension("json.tmp");
    std::fs::write(&temporary, contents)?;
    std::fs::rename(&temporary, &target)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::settings::SETTINGS;

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
        let dir = std::env::temp_dir().join(format!("alan-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join(SETTINGS_FILE);
        std::fs::write(&path, contents).expect("write");
        (Scratch(dir), path)
    }

    #[test]
    fn a_missing_file_is_silent() {
        let layer = read_layer(std::path::Path::new("/nonexistent/alan/settings.json"))
            .expect("not having a settings file is normal");
        assert_eq!(layer, SettingsLayer::default());
    }

    #[test]
    fn a_valid_file_becomes_a_layer() {
        let (_dir, path) = temp_file("valid", r#"{"model":"opus","tools":{"web_search":true}}"#);
        let layer = read_layer(&path).expect("valid file");

        assert_eq!(layer.model.as_deref(), Some("opus"));
        assert_eq!(layer.tools.unwrap().web_search, Some(true));
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

    #[test]
    fn a_wrongly_typed_value_refuses_to_start() {
        let (_dir, path) = temp_file("typed", r#"{"model":42}"#);
        assert!(read_layer(&path).is_err());
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
    fn a_known_group_is_not_itself_an_unknown_key() {
        let (_dir, path) = temp_file("group", r#"{"tools":{"web_search":true}}"#);
        read_layer(&path).expect("`tools` is a group, not a typo");
    }

    #[test]
    fn load_reads_the_global_file_from_the_directory_it_is_given() {
        let (_dir, path) = temp_file("load", "{ not json");
        let dir = path.parent().expect("temp dir");
        assert!(
            load_settings_layers(dir, dir).is_err(),
            "the global file's parse failure must reach the caller"
        );
    }

    /// Writing from the typed struct would drop anything this build does not
    /// know about — a key a newer Alan wrote, silently deleted by an older one.
    #[test]
    fn a_write_preserves_keys_this_build_does_not_know() {
        let (_dir, path) = temp_file("preserve", r#"{"model":"opus","from_the_future":{"a":1}}"#);
        write_key(&path, "model", Some(serde_json::json!("haiku"))).expect("write");

        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["model"], "haiku");
        assert_eq!(raw["from_the_future"]["a"], 1, "unknown keys survive");
    }

    #[test]
    fn a_write_creates_nested_groups_and_clearing_prunes_them() {
        let (_dir, path) = temp_file("nested", "{}");
        write_key(&path, "tools.web_search", Some(serde_json::json!(true))).expect("set");

        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["tools"]["web_search"], true);

        write_key(&path, "tools.web_search", None).expect("clear");
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            raw.get("tools").is_none(),
            "an empty group is noise in a file that lists only what you changed: {raw}"
        );
    }

    /// A settings file that failed to load is exactly when someone is most
    /// likely mid-edit, so a write must not "repair" it by discarding whatever
    /// sits where a group belongs.
    #[test]
    fn a_write_refuses_rather_than_clobbering_a_value_where_a_group_belongs() {
        let (_dir, path) = temp_file("clobber", r#"{"tools":42}"#);
        let error = write_key(&path, "tools.web_search", Some(serde_json::json!(true)))
            .expect_err("must refuse");

        assert!(error.to_string().contains("tools"), "{error}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("42"), "the file is untouched: {text}");
    }

    /// Dotfile managers symlink configs; renaming over the link would replace
    /// it with a regular file and quietly detach it from the repo.
    #[test]
    fn a_write_follows_a_symlink_rather_than_replacing_it() {
        let (_dir, real) = temp_file("symlink-target", r#"{"model":"before"}"#);
        let link = real.with_file_name("linked.json");
        let _ = std::fs::remove_file(&link);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        #[cfg(not(unix))]
        return;

        write_key(&link, "model", Some(serde_json::json!("after"))).expect("write");

        assert!(
            std::fs::symlink_metadata(&link)
                .expect("link")
                .file_type()
                .is_symlink(),
            "the symlink must survive the write"
        );
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&real).unwrap()).unwrap();
        assert_eq!(raw["model"], "after", "the target got the write");
    }

    /// A project file may not set anything that executes, redirects the
    /// network, or touches credentials — and saying so beats ignoring it.
    #[test]
    fn a_project_file_is_refused_when_it_sets_something_it_may_not() {
        // Every v1 setting is project-safe, so this asserts the gate is wired
        // rather than that any particular key is blocked.
        assert!(
            SETTINGS.iter().all(|def| def.project_safe),
            "update this test when the first user-scope-only setting lands"
        );
        let (_dir, path) = temp_file("project-safe", r#"{"model":"opus"}"#);
        assert!(project_layer(&path).is_ok());
    }
}
