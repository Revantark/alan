//! Keeps the running agent in step with the settings files, and drives the
//! `/settings` list over them.
//!
//! Polls mtime rather than watching for events: editors save atomically, so an
//! ordinary `:w` arrives as remove-then-create and an event watcher would need
//! to watch the directory, filter and debounce to catch it. A renamed file has
//! a different mtime either way.

use super::{Layer, Layers, Settings, SettingsOverlay, SettingsState};
use crate::core::Poll;
use anyhow::Context;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

const CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// `Ok(true)` means the settings changed and need applying.
pub type Outcome = anyhow::Result<bool>;

/// Includes the paths themselves, so a project file appearing or being deleted
/// counts as a change.
type Fingerprint = Vec<(PathBuf, Option<SystemTime>)>;

pub struct SettingsController {
    alan_dir: PathBuf,
    current_dir: PathBuf,

    settings: Settings,
    /// Unfolded, so a single scope can be read and a key's origin found.
    layers: Layers,
    seen: Fingerprint,
    next_check: Instant,
    overlay: Option<SettingsOverlay>,
}

impl SettingsController {
    /// Loads the files itself, by the same route [`reload`](Self::reload) uses.
    pub fn new(alan_dir: &Path, current_dir: &Path) -> anyhow::Result<Self> {
        let layers = Layers::load(alan_dir, current_dir)?;
        let seen = fingerprint(alan_dir, current_dir);

        Ok(Self {
            alan_dir: alan_dir.to_path_buf(),
            current_dir: current_dir.to_path_buf(),
            settings: layers.resolve(),
            layers,
            seen,
            next_check: Instant::now() + CHECK_INTERVAL,
            overlay: None,
        })
    }

    /// Writes to the file and reloads, so a change from the UI and one from an
    /// editor take the same route.
    pub(super) fn write(&mut self, key: &'static str, value: Option<serde_json::Value>) -> Outcome {
        let Some(path) = self.target() else {
            return Ok(false);
        };
        super::write_key(&path, key, value)?;
        self.seen = fingerprint(&self.alan_dir, &self.current_dir);
        self.reload()
    }

    /// What a row in `scope` displays and edits — not the resolved value.
    fn as_of(&self, scope: Layer) -> Settings {
        self.layers.resolve_as_of(scope)
    }

    pub fn current(&self) -> &Settings {
        &self.settings
    }

    fn origin(&self, key: &str) -> Layer {
        self.layers.origin_layer(key)
    }

    /// The project file in force, if one exists.
    fn project_file(&self) -> Option<PathBuf> {
        super::project_settings(&self.current_dir, &super::global_path(&self.alan_dir))
    }

    /// Where a write in this scope lands: the project file if there is one,
    /// otherwise where it would be created.
    pub(super) fn path(&self, scope: Layer) -> PathBuf {
        match scope {
            Layer::Project => self
                .project_file()
                .unwrap_or_else(|| super::project_target(&self.current_dir)),
            _ => super::global_path(&self.alan_dir),
        }
    }

    /// `Ok(Poll::Changed)` means the caller should apply the new settings.
    pub fn poll(&mut self) -> anyhow::Result<Poll> {
        let now = Instant::now();
        if now < self.next_check {
            return Ok(Poll::Idle);
        }
        self.next_check = now + CHECK_INTERVAL;

        let current = fingerprint(&self.alan_dir, &self.current_dir);
        if current == self.seen {
            return Ok(Poll::Idle);
        }
        // Recorded before the read, so a broken file is reported once.
        self.seen = current;
        match self.reload()? {
            true => Ok(Poll::Changed),
            false => Ok(Poll::Idle),
        }
    }

    /// Keeps the previous values if anything is wrong: a live session should
    /// survive a typo made mid-edit.
    fn reload(&mut self) -> Outcome {
        let layers =
            Layers::load(&self.alan_dir, &self.current_dir).context("previous settings kept")?;
        let next = layers.resolve();
        self.layers = layers;
        if next == self.settings {
            return Ok(false);
        }
        self.settings = next;
        Ok(true)
    }

    /// Opens in project scope only when a project write has somewhere to go.
    pub fn open(&mut self) {
        let has_project =
            self.project_file().is_some() || super::repo_root(&self.current_dir).is_some();
        self.overlay = Some(SettingsOverlay::new(has_project));
    }

    pub fn close(&mut self) {
        self.overlay = None;
    }

    /// Where the open overlay writes. `None` when it is closed.
    fn target(&self) -> Option<PathBuf> {
        self.overlay.as_ref().map(|o| self.path(o.scope))
    }

    /// A prompt is open for the selected row.
    pub fn editing(&self) -> bool {
        self.overlay.as_ref().is_some_and(|o| o.editing)
    }

    /// `None` when the overlay is closed. One value rather than a handle on
    /// the store, so `views` cannot reach past what it needs to draw.
    pub fn state(&self) -> Option<SettingsState> {
        let overlay = self.overlay.as_ref()?;
        let path = self.path(overlay.scope);
        Some(SettingsState {
            scope: overlay.scope,
            file_exists: path.is_file(),
            path,
            rows: overlay.rows(&self.layers),
            selected: overlay.selected,
            editing: overlay.editing,
        })
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

fn fingerprint(alan_dir: &Path, current_dir: &Path) -> Fingerprint {
    let global = super::global_path(alan_dir);
    let mut paths = vec![global.clone()];
    if let Some(project) = super::project_settings(current_dir, &global) {
        paths.push(project);
    }
    paths
        .into_iter()
        .map(|path| {
            let modified = std::fs::metadata(&path)
                .ok()
                .and_then(|m| m.modified().ok());
            (path, modified)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::settings::storage::{SETTINGS_DIR, SETTINGS_FILE};

    struct Scratch {
        root: PathBuf,
        home: PathBuf,
        project: PathBuf,
    }

    /// So a test leaves the filesystem as it found it, panic or not.
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("alan-reload-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let home = root.join("home");
            let project = root.join("project");
            std::fs::create_dir_all(&home).expect("home");
            std::fs::create_dir_all(project.join(SETTINGS_DIR)).expect("project");
            // A repo boundary, so the walk cannot escape the temp dir.
            std::fs::create_dir_all(project.join(".git")).expect("git");
            Self {
                root,
                home,
                project,
            }
        }

        fn write_global(&self, contents: &str) {
            write(&self.home.join(SETTINGS_FILE), contents);
        }

        fn write_project(&self, contents: &str) {
            write(
                &self.project.join(SETTINGS_DIR).join(SETTINGS_FILE),
                contents,
            );
        }

        fn controller(&self) -> SettingsController {
            SettingsController::new(&self.home, &self.project).expect("valid settings")
        }
    }

    fn write(path: &Path, contents: &str) {
        std::fs::write(path, contents).expect("write");
        // Some filesystems have mtime resolution coarse enough that two writes
        // in the same instant look identical.
        let later = SystemTime::now() + Duration::from_secs(1);
        let _ = std::fs::File::open(path).and_then(|file| file.set_modified(later));
    }

    /// Bypasses the throttle so tests need not sleep.
    fn poll_now(controller: &mut SettingsController) -> anyhow::Result<Poll> {
        controller.next_check = Instant::now();
        controller.poll()
    }

    #[test]
    fn an_unchanged_file_is_not_reloaded() {
        let scratch = Scratch::new("unchanged");
        scratch.write_global(r#"{"model":"opus"}"#);
        let mut controller = scratch.controller();

        assert_eq!(poll_now(&mut controller).unwrap(), Poll::Idle);
        assert_eq!(poll_now(&mut controller).unwrap(), Poll::Idle);
    }

    #[test]
    fn editing_the_file_applies_without_a_command() {
        let scratch = Scratch::new("edited");
        scratch.write_global(r#"{"model":"first"}"#);
        let mut controller = scratch.controller();
        assert_eq!(controller.current().model, "first");

        scratch.write_global(r#"{"model":"second"}"#);

        assert_eq!(poll_now(&mut controller).unwrap(), Poll::Changed);
        assert_eq!(controller.current().model, "second");
    }

    /// Killing a session with history in it because someone typo'd mid-edit
    /// would be worse than ignoring the edit.
    #[test]
    fn a_broken_edit_keeps_the_previous_settings() {
        let scratch = Scratch::new("broken");
        scratch.write_global(r#"{"model":"good"}"#);
        let mut controller = scratch.controller();

        scratch.write_global("{ broken");
        let error = poll_now(&mut controller)
            .expect_err("a broken file must report")
            .to_string();

        assert!(error.contains("previous settings kept"), "{error}");
        assert_eq!(controller.current().model, "good", "old values stay");
    }

    #[test]
    fn a_broken_file_is_reported_once_not_every_check() {
        let scratch = Scratch::new("broken-once");
        scratch.write_global(r#"{"model":"good"}"#);
        let mut controller = scratch.controller();

        scratch.write_global("{ broken");
        assert!(poll_now(&mut controller).is_err());

        assert_eq!(
            poll_now(&mut controller).unwrap(),
            Poll::Idle,
            "no second complaint until the file changes again"
        );
    }

    #[test]
    fn an_identical_rewrite_is_not_a_change() {
        let scratch = Scratch::new("identical");
        scratch.write_global(r#"{"model":"same"}"#);
        let mut controller = scratch.controller();

        scratch.write_global(r#"{"model":"same"}"#);
        assert_eq!(poll_now(&mut controller).unwrap(), Poll::Idle);
    }

    /// No file's mtime moved — the project file simply began to exist.
    #[test]
    fn a_project_file_appearing_is_noticed() {
        let scratch = Scratch::new("appeared");
        scratch.write_global(r#"{"model":"global"}"#);
        let mut controller = scratch.controller();
        assert_eq!(controller.current().model, "global");

        scratch.write_project(r#"{"model":"project"}"#);

        assert_eq!(poll_now(&mut controller).unwrap(), Poll::Changed);
        assert_eq!(controller.current().model, "project");
        assert_eq!(controller.origin("model"), Layer::Project);
    }

    #[test]
    fn the_project_file_outranks_the_global_one_per_field() {
        let scratch = Scratch::new("layered");
        scratch.write_global(r#"{"model":"global","tools":{"web_search":true}}"#);
        scratch.write_project(r#"{"model":"project"}"#);
        let controller = scratch.controller();

        assert_eq!(controller.current().model, "project");
        assert_eq!(controller.origin("model"), Layer::Project);
        // Untouched by the project file, so the global one still applies.
        assert!(controller.current().tools.web_search);
        assert_eq!(controller.origin("tools.web_search"), Layer::Global);
        assert_eq!(controller.origin("tools.web_fetch"), Layer::Default);
    }
}
