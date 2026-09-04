//! State for the `/settings` list, and the value handed to `views` to draw it.
//!
//! Reads [`Layers`] rather than the store, so nothing here depends on how the
//! files are watched. Rendering lives in `views`.

use super::{Kind, Layer, Layers, SETTINGS, SettingDef, Settings, Value};
use std::path::PathBuf;

/// Whether editing this row does anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Marker {
    SetHere,
    /// From a lower layer; editing here takes over.
    Inherited(Layer),
    /// From a higher layer; editing here is stored but inert.
    Overridden(Layer),
}

pub struct Row {
    pub label: &'static str,
    pub help: &'static str,
    /// This scope's value, or the one it would inherit.
    pub value: String,
    pub marker: Marker,
    /// `Bool` and `Enum` cycle in place; the rest open a prompt.
    pub cycles: bool,
}

/// Everything the `/settings` overlay needs to draw itself, handed over as one
/// value so the view never reaches into the store.
pub struct SettingsState {
    pub scope: Layer,
    /// Where a write in this scope lands.
    pub path: PathBuf,
    /// `false` when that file has not been created yet.
    pub file_exists: bool,
    pub rows: Vec<Row>,
    pub selected: usize,
    /// A prompt is open for the selected row.
    pub editing: bool,
}

pub struct SettingsOverlay {
    pub(super) scope: Layer,
    pub(super) selected: usize,
    /// A prompt is open for this row. The text lives in the shared editor.
    pub(super) editing: bool,
    /// What a just-opened prompt should start from, taken once by the view.
    pub(super) seed: Option<String>,
}

impl SettingsOverlay {
    pub(super) fn new(has_project_scope: bool) -> Self {
        Self {
            scope: if has_project_scope {
                Layer::Project
            } else {
                Layer::Global
            },
            selected: 0,
            editing: false,
            seed: None,
        }
    }

    pub(super) fn def(&self) -> &'static SettingDef {
        &SETTINGS[self.selected.min(SETTINGS.len() - 1)]
    }

    pub(super) fn move_by(&mut self, delta: isize) {
        let count = SETTINGS.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(count) as usize;
    }

    pub(super) fn toggle_scope(&mut self) {
        self.scope = match self.scope {
            Layer::Project => Layer::Global,
            _ => Layer::Project,
        };
        self.editing = false;
    }

    pub(super) fn rows(&self, layers: &Layers) -> Vec<Row> {
        let in_scope = layers.resolve_as_of(self.scope);
        SETTINGS
            .iter()
            .map(|def| {
                let origin = layers.origin_layer(def.key);
                Row {
                    label: def.label,
                    help: def.help,
                    value: show((def.read)(&in_scope)),
                    marker: match origin.cmp(&self.scope) {
                        std::cmp::Ordering::Equal => Marker::SetHere,
                        std::cmp::Ordering::Less => Marker::Inherited(origin),
                        std::cmp::Ordering::Greater => Marker::Overridden(origin),
                    },
                    cycles: matches!(def.kind, Kind::Bool | Kind::Enum(_)),
                }
            })
            .collect()
    }

    pub(super) fn shown_value(&self, in_scope: &Settings) -> String {
        show((self.def().read)(in_scope))
    }

    /// `None` when the row needs a prompt rather than a cycle.
    pub(super) fn next_value(&self, current: &Settings) -> Option<serde_json::Value> {
        let def = self.def();
        match def.kind {
            Kind::Bool => match (def.read)(current) {
                Value::Bool(on) => Some(serde_json::Value::Bool(!on)),
                _ => None,
            },
            Kind::Enum(options) => {
                let current = show((def.read)(current));
                let index = options.iter().position(|option| *option == current)?;
                Some(serde_json::Value::String(
                    options[(index + 1) % options.len()].to_owned(),
                ))
            }
            _ => None,
        }
    }

    /// An empty string clears the key, as `Backspace` does.
    pub(super) fn parse_edit(text: &str) -> Option<serde_json::Value> {
        let text = text.trim();
        (!text.is_empty()).then(|| serde_json::Value::String(text.to_owned()))
    }
}

fn show(value: Value) -> String {
    match value {
        Value::Bool(true) => "on".into(),
        Value::Bool(false) => "off".into(),
        Value::Text(text) => text,
        Value::Enum(name) => name.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layers(global: &str, project: Option<&str>) -> (Layers, tempdirs::Dirs) {
        let dirs = tempdirs::Dirs::new();
        dirs.write_global(global);
        if let Some(project) = project {
            dirs.write_project(project);
        }
        let layers = Layers::load(&dirs.home, &dirs.project).expect("valid");
        (layers, dirs)
    }

    fn overlay_at(scope: Layer, key: &str) -> SettingsOverlay {
        SettingsOverlay {
            scope,
            selected: SETTINGS.iter().position(|def| def.key == key).expect("key"),
            editing: false,
            seed: None,
        }
    }

    mod tempdirs {
        use crate::core::settings::storage::{SETTINGS_DIR, SETTINGS_FILE};
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicU64, Ordering};

        pub struct Dirs {
            root: PathBuf,
            pub home: PathBuf,
            pub project: PathBuf,
        }

        /// So a test leaves the filesystem as it found it, panic or not.
        impl Drop for Dirs {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.root);
            }
        }

        impl Dirs {
            pub fn new() -> Self {
                // Counted: every test here builds one, and two sharing a name
                // would delete each other's directory.
                static NEXT: AtomicU64 = AtomicU64::new(0);
                let root = std::env::temp_dir().join(format!(
                    "alan-overlay-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                let _ = std::fs::remove_dir_all(&root);
                let home = root.join("home");
                let project = root.join("project");
                std::fs::create_dir_all(&home).unwrap();
                std::fs::create_dir_all(project.join(SETTINGS_DIR)).unwrap();
                std::fs::create_dir_all(project.join(".git")).unwrap();
                Self {
                    root,
                    home,
                    project,
                }
            }
            pub fn write_global(&self, text: &str) {
                std::fs::write(self.home.join(SETTINGS_FILE), text).unwrap();
            }
            pub fn write_project(&self, text: &str) {
                std::fs::write(self.project.join(SETTINGS_DIR).join(SETTINGS_FILE), text).unwrap();
            }
        }
    }

    fn row<'a>(rows: &'a [Row], label: &str) -> &'a Row {
        rows.iter().find(|row| row.label == label).expect("row")
    }

    #[test]
    fn markers_distinguish_owned_inherited_and_overridden() {
        let (layers, _dirs) = layers(
            r#"{"model":"from-global","tools":{"web_fetch":true}}"#,
            Some(r#"{"tools":{"web_search":true}}"#),
        );
        let mut overlay = overlay_at(Layer::Project, "model");
        let rows = overlay.rows(&layers);

        assert_eq!(row(&rows, "web search").marker, Marker::SetHere);
        assert_eq!(
            row(&rows, "model").marker,
            Marker::Inherited(Layer::Global),
            "a lower layer set it, so editing here would take over"
        );

        // From global scope the project file is now the one imposing a value.
        overlay.toggle_scope();
        let rows = overlay.rows(&layers);
        assert_eq!(row(&rows, "model").marker, Marker::SetHere);
        assert_eq!(
            row(&rows, "web search").marker,
            Marker::Overridden(Layer::Project),
            "editing here is stored but inert"
        );
    }

    #[test]
    fn a_row_shows_its_own_scopes_value_not_the_resolved_one() {
        let (layers, _dirs) = layers(
            r#"{"reasoning_effort":"low"}"#,
            Some(r#"{"reasoning_effort":"max"}"#),
        );
        let mut overlay = overlay_at(Layer::Global, "reasoning_effort");

        let rows = overlay.rows(&layers);
        let effort = row(&rows, "reasoning effort");
        assert_eq!(
            effort.value, "low",
            "global's own value, not the resolved max"
        );
        assert_eq!(effort.marker, Marker::Overridden(Layer::Project));

        // And cycling starts from what the row showed.
        assert_eq!(
            overlay.next_value(&layers.resolve_as_of(Layer::Global)),
            Some(serde_json::json!("medium")),
            "cycles from low, not from max"
        );

        overlay.toggle_scope();
        assert_eq!(row(&overlay.rows(&layers), "reasoning effort").value, "max");
    }

    #[test]
    fn enter_cycles_bools_and_enums_but_not_text() {
        let (layers, _dirs) = layers(r#"{"reasoning_effort":"low"}"#, None);
        let current = layers.resolve();

        let overlay = overlay_at(Layer::Global, "tools.web_search");
        assert_eq!(
            overlay.next_value(&current),
            Some(serde_json::Value::Bool(true))
        );

        let overlay = overlay_at(Layer::Global, "reasoning_effort");
        assert_eq!(
            overlay.next_value(&current),
            Some(serde_json::json!("medium")),
            "cycles to the next option, not a toggle"
        );

        let overlay = overlay_at(Layer::Global, "model");
        assert_eq!(
            overlay.next_value(&current),
            None,
            "text rows need a prompt"
        );
    }

    #[test]
    fn an_empty_edit_clears_the_key_rather_than_setting_an_empty_value() {
        assert_eq!(SettingsOverlay::parse_edit("   "), None);
        assert_eq!(
            SettingsOverlay::parse_edit("opus"),
            Some(serde_json::json!("opus"))
        );
    }

    #[test]
    fn selection_wraps_in_both_directions() {
        let mut overlay = SettingsOverlay::new(false);
        overlay.move_by(-1);
        assert_eq!(overlay.selected, SETTINGS.len() - 1);
        overlay.move_by(1);
        assert_eq!(overlay.selected, 0);
    }
}
