//! State for the `/settings` list. Rendering lives in `views`.

use super::{Kind, Layer, Outcome, SETTINGS, SettingDef, Settings, SettingsController, Value};
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

pub struct SettingsOverlay {
    pub scope: Layer,
    pub selected: usize,
    /// A prompt is open for this row. The text lives in the shared editor.
    pub editing: bool,
    /// What a just-opened prompt should start from, taken once by the view.
    pub seed: Option<String>,
}

impl SettingsOverlay {
    /// Opens in project scope only when a project write has somewhere to go.
    fn new(settings: &SettingsController) -> Self {
        let has_project =
            settings.project_file().is_some() || super::repo_root(&settings.cwd).is_some();
        Self {
            scope: if has_project {
                Layer::Project
            } else {
                Layer::Global
            },
            selected: 0,
            editing: false,
            seed: None,
        }
    }

    fn def(&self) -> &'static SettingDef {
        &SETTINGS[self.selected.min(SETTINGS.len() - 1)]
    }

    fn move_by(&mut self, delta: isize) {
        let count = SETTINGS.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(count) as usize;
    }

    fn toggle_scope(&mut self) {
        self.scope = match self.scope {
            Layer::Project => Layer::Global,
            _ => Layer::Project,
        };
        self.editing = false;
    }

    fn rows(&self, settings: &SettingsController) -> Vec<Row> {
        let in_scope = settings.as_of(self.scope);
        SETTINGS
            .iter()
            .map(|def| {
                let origin = settings.origin(def.key);
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

    fn shown_value(&self, in_scope: &Settings) -> String {
        show((self.def().read)(in_scope))
    }

    /// `None` when the row needs a prompt rather than a cycle.
    fn next_value(&self, current: &Settings) -> Option<serde_json::Value> {
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
    fn parse_edit(text: &str) -> Option<serde_json::Value> {
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

/// Driving the `/settings` list. Kept beside [`SettingsOverlay`] rather than
/// with the store, whose job is files and layers.
impl SettingsController {
    pub fn overlay(&self) -> Option<&SettingsOverlay> {
        self.overlay.as_ref()
    }

    pub fn open(&mut self) {
        self.overlay = Some(SettingsOverlay::new(self));
    }

    pub fn close(&mut self) {
        self.overlay = None;
    }

    /// Where the open overlay writes, and whether that file exists yet.
    pub fn target(&self) -> Option<PathBuf> {
        self.overlay.as_ref().map(|o| self.path(o.scope))
    }

    /// A prompt is open for the selected row.
    pub fn editing(&self) -> bool {
        self.overlay.as_ref().is_some_and(|o| o.editing)
    }

    pub fn rows(&self) -> Vec<Row> {
        self.overlay
            .as_ref()
            .map(|overlay| overlay.rows(self))
            .unwrap_or_default()
    }

    pub fn move_selection(&mut self, delta: isize) {
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.move_by(delta);
        }
    }

    pub fn toggle_scope(&mut self) {
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.toggle_scope();
        }
    }

    /// Abandon a row's prompt, leaving the list open.
    pub fn cancel_edit(&mut self) {
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.editing = false;
        }
    }

    /// Take the value a just-opened prompt should start from.
    pub fn take_seed(&mut self) -> Option<String> {
        self.overlay.as_mut()?.seed.take()
    }

    /// Cycles the row, or opens a prompt when it needs typing. Both read the
    /// scope's value, so what you cycle from is what the row shows.
    pub fn activate(&mut self) -> Outcome {
        let Some(overlay) = self.overlay.as_ref() else {
            return Ok(false);
        };
        let in_scope = self.as_of(overlay.scope);
        let key = overlay.def().key;
        match overlay.next_value(&in_scope) {
            Some(value) => self.write(key, Some(value)),
            None => {
                let seed = overlay.shown_value(&in_scope);
                if let Some(overlay) = self.overlay.as_mut() {
                    overlay.seed = Some(seed);
                    overlay.editing = true;
                }
                Ok(false)
            }
        }
    }

    /// Only a row this scope owns can be cleared.
    pub fn clear(&mut self) -> Outcome {
        let Some(overlay) = self.overlay.as_ref() else {
            return Ok(false);
        };
        let key = overlay.def().key;
        if self.origin(key) != overlay.scope {
            return Ok(false);
        }
        self.write(key, None)
    }

    pub fn submit_edit(&mut self, text: &str) -> Outcome {
        let Some(overlay) = self.overlay.as_ref() else {
            return Ok(false);
        };
        let key = overlay.def().key;
        let value = SettingsOverlay::parse_edit(text);
        if let Some(overlay) = self.overlay.as_mut() {
            overlay.editing = false;
        }
        self.write(key, value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn controller(global: &str, project: Option<&str>) -> (SettingsController, tempdirs::Dirs) {
        let dirs = tempdirs::Dirs::new();
        dirs.write_global(global);
        if let Some(project) = project {
            dirs.write_project(project);
        }
        let controller = SettingsController::new(&dirs.home, &dirs.project).expect("valid");
        (controller, dirs)
    }

    mod tempdirs {
        use crate::core::settings::files::{SETTINGS_DIR, SETTINGS_FILE};
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
        let (settings, _dirs) = controller(
            r#"{"model":"from-global","tools":{"web_fetch":true}}"#,
            Some(r#"{"tools":{"web_search":true}}"#),
        );
        let mut overlay = SettingsOverlay {
            scope: Layer::Project,
            selected: 0,
            editing: false,
            seed: None,
        };
        let rows = overlay.rows(&settings);

        assert_eq!(row(&rows, "web search").marker, Marker::SetHere);
        assert_eq!(
            row(&rows, "model").marker,
            Marker::Inherited(Layer::Global),
            "a lower layer set it, so editing here would take over"
        );

        // From global scope the project file is now the one imposing a value.
        overlay.toggle_scope();
        let rows = overlay.rows(&settings);
        assert_eq!(row(&rows, "model").marker, Marker::SetHere);
        assert_eq!(
            row(&rows, "web search").marker,
            Marker::Overridden(Layer::Project),
            "editing here is stored but inert"
        );
    }

    #[test]
    fn a_row_shows_its_own_scopes_value_not_the_resolved_one() {
        let (settings, _dirs) = controller(
            r#"{"reasoning_effort":"low"}"#,
            Some(r#"{"reasoning_effort":"max"}"#),
        );
        let mut overlay = SettingsOverlay {
            scope: Layer::Global,
            selected: SETTINGS
                .iter()
                .position(|d| d.key == "reasoning_effort")
                .unwrap(),
            editing: false,
            seed: None,
        };

        let rows = overlay.rows(&settings);
        let effort = row(&rows, "reasoning effort");
        assert_eq!(
            effort.value, "low",
            "global's own value, not the resolved max"
        );
        assert_eq!(effort.marker, Marker::Overridden(Layer::Project));

        // And cycling starts from what the row showed.
        assert_eq!(
            overlay.next_value(&settings.as_of(Layer::Global)),
            Some(serde_json::json!("medium")),
            "cycles from low, not from max"
        );

        overlay.toggle_scope();
        assert_eq!(
            row(&overlay.rows(&settings), "reasoning effort").value,
            "max"
        );
    }

    #[test]
    fn enter_cycles_bools_and_enums_but_not_text() {
        let (settings, _dirs) = controller(r#"{"reasoning_effort":"low"}"#, None);
        let mut overlay = SettingsOverlay {
            scope: Layer::Global,
            selected: 0,
            editing: false,
            seed: None,
        };

        overlay.selected = SETTINGS
            .iter()
            .position(|d| d.key == "tools.web_search")
            .unwrap();
        assert_eq!(
            overlay.next_value(settings.current()),
            Some(serde_json::Value::Bool(true))
        );

        overlay.selected = SETTINGS
            .iter()
            .position(|d| d.key == "reasoning_effort")
            .unwrap();
        assert_eq!(
            overlay.next_value(settings.current()),
            Some(serde_json::json!("medium")),
            "cycles to the next option, not a toggle"
        );

        overlay.selected = SETTINGS.iter().position(|d| d.key == "model").unwrap();
        assert_eq!(
            overlay.next_value(settings.current()),
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
        let mut overlay = SettingsOverlay {
            scope: Layer::Global,
            selected: 0,
            editing: false,
            seed: None,
        };
        overlay.move_by(-1);
        assert_eq!(overlay.selected, SETTINGS.len() - 1);
        overlay.move_by(1);
        assert_eq!(overlay.selected, 0);
    }
}
