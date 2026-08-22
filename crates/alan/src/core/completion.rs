//! File-path completion for the prompt editor.
//!
//! Typing `@` in the editor opens a completion popup listing files and
//! folders. A bare token (`@popup`) recursively searches the project and
//! matches anywhere in relative paths; a token with a slash (`@src/fo`)
//! lists `src/` filtered by the `fo` prefix. Directory scans run on the
//! blocking thread pool and deliver results through a channel drained by
//! [`CompletionController::poll`].

use super::Poll;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::watch::{self, Receiver, Sender};

/// Maximum candidates retained from one scan for later in-memory filtering.
const CANDIDATE_LIMIT: usize = 5_000;
/// Maximum candidates displayed after filtering.
const SCAN_LIMIT: usize = 250;
/// Maximum filesystem entries visited by one scan.
const VISIT_LIMIT: usize = 20_000;
/// Maximum recursive depth for a bare-token scan.
const MAX_SCAN_DEPTH: usize = 32;
/// Directories excluded from scans regardless of prefix.
const SKIPPED_DIRS: &[&str] = &[".git", "target", "node_modules"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// Path relative to the scanned root, e.g. `crates/agent/src/lib.rs`.
    pub path: String,
    pub is_dir: bool,
}

impl DirEntry {
    /// Final path segment, for prefix filtering.
    fn file_name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionStatus {
    Loading,
    Ready,
    Error(String),
}

/// Filtered completion candidates shown in the popup.
#[derive(Debug, Clone)]
pub struct CompletionState {
    pub items: Vec<DirEntry>,
    pub selected: usize,
    pub status: CompletionStatus,
}

impl Default for CompletionState {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            selected: 0,
            status: CompletionStatus::Loading,
        }
    }
}

/// Result of accepting the highlighted completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    /// Text replacing the `@token`, including the typed directory segments.
    pub replacement: String,
    /// Directories keep the popup open so the user can drill deeper.
    pub is_dir: bool,
}

type ScanResults = io::Result<Vec<DirEntry>>;

/// Token shared with a scan task. Bumping it asks the traversal to bail early.
/// `Arc<AtomicU64>` is the cheapest cancellation primitive available on the
/// blocking thread pool: no `JoinHandle` polling, no runtime blocking.
type ScanCancel = Arc<AtomicU64>;

pub struct CompletionController {
    state: CompletionState,
    open: bool,
    /// Workspace root used to resolve typed relative directories.
    root: PathBuf,
    /// Raw scan results for the current directory.
    entries: Vec<DirEntry>,
    /// Directory segment of the active token as typed ("" for the project root).
    dir_part: String,
    prefix: String,
    /// Staleness stamp bumped on every new scan request.
    generation: u64,
    /// Directory of the most recent scan request, for deduplication.
    last_scan_dir: Option<PathBuf>,
    /// Latest scan result. Stale results are drained before applying the newest.
    tx: Sender<Option<(u64, PathBuf, ScanResults)>>,
    rx: Receiver<Option<(u64, PathBuf, ScanResults)>>,
    /// Epoch token shared with a scan task so it can bail early.
    cancel_epoch: ScanCancel,
}

impl CompletionController {
    pub fn new() -> Self {
        let root = std::env::current_dir()
            .ok()
            .and_then(|path| path.canonicalize().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        Self::with_root(root)
    }

    pub(crate) fn with_root(root: PathBuf) -> Self {
        let (tx, rx) = watch::channel(None);
        Self {
            state: CompletionState::default(),
            open: false,
            root,
            entries: Vec::new(),
            dir_part: String::new(),
            prefix: String::new(),
            generation: 0,
            last_scan_dir: None,
            tx,
            rx,
            cancel_epoch: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Track the token currently typed after `@`.
    ///
    /// A bare token recursively searches the workspace; a token containing
    /// `/` lists that directory. The scan is independent of the filename
    /// prefix so later keystrokes can refilter the same result safely.
    pub fn update(&mut self, token: &str) {
        let (dir_part, prefix) = match token.rsplit_once('/') {
            Some((dir, prefix)) => (dir, prefix),
            None => ("", token),
        };
        let scan_key = if dir_part.is_empty() {
            PathBuf::from(".")
        } else {
            PathBuf::from(dir_part)
        };
        let Some(scan_dir) = self.resolve_relative_dir(dir_part) else {
            self.cancel_scan();
            self.open = true;
            self.entries.clear();
            self.state = CompletionState {
                status: CompletionStatus::Error("path is outside the workspace".into()),
                ..CompletionState::default()
            };
            self.dir_part = dir_part.to_owned();
            self.prefix = prefix.to_owned();
            self.last_scan_dir = None;
            return;
        };

        self.prefix = prefix.to_owned();
        self.dir_part = dir_part.to_owned();
        if self.last_scan_dir.as_ref() != Some(&scan_key) {
            self.generation += 1;
            self.last_scan_dir = Some(scan_key.clone());
            self.entries.clear();
            self.state = CompletionState {
                status: CompletionStatus::Loading,
                ..CompletionState::default()
            };
            self.spawn_scan(scan_dir, scan_key, self.generation, dir_part.is_empty());
        }
        self.open = true;
        self.refilter();
    }

    pub fn poll(&mut self) -> Poll {
        if !self.rx.has_changed().unwrap_or(false) {
            return Poll::Idle;
        }
        let results = {
            let result = self.rx.borrow_and_update();
            let Some((generation, key, results)) = result.as_ref() else {
                return Poll::Idle;
            };
            if *generation != self.generation || self.last_scan_dir.as_ref() != Some(key) {
                return Poll::Idle;
            }
            match results {
                Ok(entries) => Ok(entries.clone()),
                Err(error) => Err(io::Error::new(error.kind(), error.to_string())),
            }
        };
        self.state.status = match results {
            Ok(entries) => {
                self.entries = entries;
                self.refilter();
                CompletionStatus::Ready
            }
            Err(error) => {
                self.entries.clear();
                self.state.items.clear();
                CompletionStatus::Error(completion_error(&error))
            }
        };
        Poll::Changed
    }

    /// Invalidate any in-flight traversal. The blocking task observes this
    /// epoch and exits cooperatively at its next filesystem boundary.
    fn cancel_scan(&mut self) {
        self.cancel_epoch.fetch_add(1, Ordering::Release);
    }

    fn resolve_relative_dir(&self, dir_part: &str) -> Option<PathBuf> {
        let relative = Path::new(dir_part);
        if relative.is_absolute() {
            return None;
        }
        let mut normalized = PathBuf::new();
        for component in relative.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(segment) => normalized.push(segment),
                Component::ParentDir => {
                    if !normalized.pop() {
                        return None;
                    }
                }
                Component::RootDir | Component::Prefix(_) => return None,
            }
        }

        let candidate = self.root.join(normalized);
        let mut existing = candidate.as_path();
        loop {
            if let Ok(canonical) = existing.canonicalize() {
                return canonical.starts_with(&self.root).then_some(candidate);
            }
            existing = existing.parent()?;
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.state.items.is_empty() {
            return;
        }
        let max = self.state.items.len() - 1;
        let next = (self.state.selected as isize + delta).clamp(0, max as isize);
        self.state.selected = next as usize;
    }

    /// Accept the highlighted entry. Files close the popup; directories keep
    /// it open so the next token update drills into them.
    pub fn accept(&mut self) -> Option<Accepted> {
        if !matches!(self.state.status, CompletionStatus::Ready) {
            return None;
        }
        let entry = self.state.items.get(self.state.selected)?.clone();

        let mut replacement = String::new();
        if !self.dir_part.is_empty() {
            replacement.push_str(&self.dir_part);
            replacement.push('/');
        }
        replacement.push_str(&entry.path);
        if entry.is_dir {
            replacement.push('/');
        } else {
            self.dismiss();
        }

        Some(Accepted {
            replacement,
            is_dir: entry.is_dir,
        })
    }

    pub fn dismiss(&mut self) {
        self.open = false;
        self.entries.clear();
        self.state = CompletionState::default();
        self.dir_part.clear();
        self.prefix.clear();
        self.last_scan_dir = None;
        self.cancel_scan();
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn has_items(&self) -> bool {
        !self.state.items.is_empty()
    }

    pub fn state(&self) -> Option<&CompletionState> {
        self.open.then_some(&self.state)
    }

    fn spawn_scan(&mut self, dir: PathBuf, key: PathBuf, generation: u64, recursive: bool) {
        self.cancel_scan();
        let my_epoch = self.cancel_epoch.load(Ordering::Acquire);
        let cancel = self.cancel_epoch.clone();
        let tx = self.tx.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn_blocking(move || {
                let results = scan_dir_with(&dir, recursive, &cancel, my_epoch);
                let _ = tx.send(Some((generation, key, results)));
            });
        } else {
            let results = scan_dir_with(&dir, recursive, &cancel, my_epoch);
            let _ = tx.send(Some((generation, key, results)));
        }
    }

    fn refilter(&mut self) {
        let prefix = self.prefix.to_lowercase();
        self.state.items = self
            .entries
            .iter()
            .filter(|entry| matches_prefix(&entry.path, entry.file_name(), &prefix))
            .take(SCAN_LIMIT)
            .cloned()
            .collect();
        if self.state.selected >= self.state.items.len() {
            self.state.selected = self.state.items.len().saturating_sub(1);
        }
    }

    #[cfg(test)]
    pub(crate) fn inject_items(&mut self, items: Vec<DirEntry>) {
        self.entries = items;
        self.open = true;
        self.state.status = CompletionStatus::Ready;
        self.refilter();
    }
}

/// List `dir`'s entries. The optional prefix is used only by test helpers;
/// production scans collect a bounded candidate set and refilter it in memory.
#[cfg(test)]
fn scan_dir(dir: &Path, recursive: bool, _prefix: &str) -> ScanResults {
    let cancel: ScanCancel = Arc::new(AtomicU64::new(1));
    scan_dir_with(dir, recursive, &cancel, 1)
}

/// Like [`scan_dir`], but cooperative: the traversal bails as soon as
/// `cancel` no longer equals `my_epoch`.
fn scan_dir_with(dir: &Path, recursive: bool, cancel: &ScanCancel, my_epoch: u64) -> ScanResults {
    let mut builder = ignore::WalkBuilder::new(dir);
    builder
        // `ignore` handles hidden files and .ignore/.gitignore files. Keep
        // these application-level exclusions in addition to those filters.
        .standard_filters(true)
        .follow_links(false)
        .min_depth(Some(1))
        .max_depth(Some(if recursive { MAX_SCAN_DEPTH } else { 1 }))
        .filter_entry(|entry| entry.depth() == 0 || !is_skipped_name(entry.file_name()));

    let mut entries = Vec::new();
    for (visited, result) in builder.build().enumerate() {
        if cancel.load(Ordering::Acquire) != my_epoch || visited >= VISIT_LIMIT {
            break;
        }
        let entry = result.map_err(ignore_error)?;

        let Some(file_type) = entry.file_type() else {
            continue;
        };
        let Some(relative) = entry.path().strip_prefix(dir).ok() else {
            continue;
        };
        entries.push(DirEntry {
            path: relative_path(relative),
            is_dir: file_type.is_dir(),
        });
    }
    entries.sort_by(sort_entries);
    entries.truncate(CANDIDATE_LIMIT);
    Ok(entries)
}

fn is_skipped_name(name: &std::ffi::OsStr) -> bool {
    SKIPPED_DIRS
        .iter()
        .any(|skipped| name == std::ffi::OsStr::new(skipped))
}

fn relative_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn ignore_error(error: ignore::Error) -> io::Error {
    let kind = error
        .io_error()
        .map_or(io::ErrorKind::Other, io::Error::kind);
    io::Error::new(kind, error.to_string())
}

fn completion_error(error: &io::Error) -> String {
    match error.kind() {
        io::ErrorKind::NotFound => "directory not found".into(),
        _ => error.to_string(),
    }
}

fn sort_entries(a: &DirEntry, b: &DirEntry) -> std::cmp::Ordering {
    a.path
        .matches('/')
        .count()
        .cmp(&b.path.matches('/').count())
        .then_with(|| b.is_dir.cmp(&a.is_dir))
        .then_with(|| a.path.cmp(&b.path))
}

/// Whether an entry should be included given the active prefix.
///
/// Mirrors [`CompletionController::refilter`]: an empty prefix accepts
/// everything; otherwise the path must contain the prefix or its final
/// segment must start with it.
fn matches_prefix(rel_path: &str, file_name: &str, prefix: &str) -> bool {
    prefix.is_empty()
        || rel_path.to_lowercase().contains(prefix)
        || file_name.to_lowercase().starts_with(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn unique_temp_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("alan-completion-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn scan_dir_sorts_dirs_first_and_skips_junk() {
        let root = unique_temp_dir("scan");
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir(root.join("target")).unwrap();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("zeta.txt"), "").unwrap();
        fs::write(root.join("alpha.txt"), "").unwrap();

        let entries = scan_dir(&root, true, "").unwrap();

        assert_eq!(
            entries,
            vec![
                DirEntry {
                    path: "src".into(),
                    is_dir: true
                },
                DirEntry {
                    path: "alpha.txt".into(),
                    is_dir: false
                },
                DirEntry {
                    path: "zeta.txt".into(),
                    is_dir: false
                },
            ]
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_directory_shows_minimal_error() {
        let root = unique_temp_dir("missing-controller");
        let error = scan_dir(&root.join("nope"), false, "").unwrap_err();

        assert_eq!(completion_error(&error), "directory not found");

        let _ = fs::remove_dir_all(root);
    }

    fn entry(path: &str, is_dir: bool) -> DirEntry {
        DirEntry {
            path: path.into(),
            is_dir,
        }
    }

    #[test]
    fn update_filters_by_prefix_case_insensitively() {
        let mut controller = CompletionController::new();
        // update() primes the prefix ("someth") and clears results pending a
        // scan; inject stands in for the scan delivering.
        controller.update("someth");
        controller.inject_items(vec![
            entry("Something.txt", false),
            entry("other.md", false),
        ]);
        let state = controller.state().unwrap();
        assert_eq!(state.items.len(), 1);
        assert_eq!(state.items[0].path, "Something.txt");

        // Empty prefix shows everything again.
        controller.update("");
        assert_eq!(controller.state().unwrap().items.len(), 2);
    }

    #[test]
    fn selection_clamps_when_filter_shrinks() {
        let mut controller = CompletionController::new();
        controller.inject_items(vec![entry("a.txt", false), entry("b.txt", false)]);
        controller.move_selection(5);
        assert_eq!(controller.state().unwrap().selected, 1);

        controller.update("b");
        assert_eq!(controller.state().unwrap().selected, 0);

        controller.move_selection(-5);
        assert_eq!(controller.state().unwrap().selected, 0);
    }

    #[test]
    fn accept_file_closes_and_dir_stays_open() {
        let mut controller = CompletionController::new();
        controller.inject_items(vec![entry("src", true), entry("main.rs", false)]);

        let accepted = controller.accept().unwrap();
        assert_eq!(
            accepted,
            Accepted {
                replacement: "src/".into(),
                is_dir: true
            }
        );
        assert!(controller.is_open());

        controller.move_selection(1);
        let accepted = controller.accept().unwrap();
        assert_eq!(
            accepted,
            Accepted {
                replacement: "main.rs".into(),
                is_dir: false
            }
        );
        assert!(!controller.is_open());
    }

    #[test]
    fn accept_prefixes_typed_directory_segment() {
        let mut controller = CompletionController::new();
        // Token "@crates/al": directory segment "crates", prefix "al".
        controller.update("crates/al");
        // The real scan of crates/ fails (missing directory); inject stands
        // in for the scan delivering.
        controller.inject_items(vec![entry("alan.rs", false)]);

        let accepted = controller.accept().unwrap();
        assert_eq!(accepted.replacement, "crates/alan.rs");
    }

    #[test]
    fn poll_drops_stale_generations() {
        let mut controller = CompletionController::new();
        controller.update("docs/");
        let fresh_generation = controller.generation;
        let docs = PathBuf::from("docs");

        controller
            .tx
            .send(Some((
                fresh_generation - 1,
                docs.clone(),
                Ok(vec![entry("stale.txt", false)]),
            )))
            .unwrap();
        controller
            .tx
            .send(Some((
                fresh_generation,
                docs.clone(),
                Ok(vec![entry("fresh.txt", false)]),
            )))
            .unwrap();
        assert_eq!(controller.poll(), Poll::Changed);

        let names: Vec<_> = controller
            .state()
            .unwrap()
            .items
            .iter()
            .map(|e| e.path.clone())
            .collect();
        assert_eq!(names, vec!["fresh.txt"]);
        assert_eq!(controller.poll(), Poll::Idle);
    }

    #[test]
    fn poll_drops_results_for_other_directories() {
        let mut controller = CompletionController::new();
        controller.update("docs/");
        let generation = controller.generation;

        // The synchronous test path queues the real scan result immediately.
        // Drain it so this test exercises only the deliberately wrong result.
        let _ = controller.poll();
        controller
            .tx
            .send(Some((
                generation,
                PathBuf::from("src"),
                Ok(vec![entry("wrong.rs", false)]),
            )))
            .unwrap();
        assert_eq!(controller.poll(), Poll::Idle);
        assert!(controller.state().unwrap().items.is_empty());
    }

    /// Regression: parent and absolute paths cannot escape the workspace.
    #[test]
    fn rejects_paths_outside_workspace() {
        let root = unique_temp_dir("root");
        let controller = CompletionController::with_root(root);
        assert!(controller.resolve_relative_dir("../").is_none());
        assert!(controller.resolve_relative_dir("../../tmp").is_none());
        assert!(controller.resolve_relative_dir("/tmp").is_none());
    }
    #[test]
    fn rescan_only_when_directory_changes() {
        let mut controller = CompletionController::new();
        controller.update("src/f");
        let first_generation = controller.generation;

        controller.update("src/fo");
        assert_eq!(controller.generation, first_generation);

        controller.update("docs/");
        assert_eq!(controller.generation, first_generation + 1);
    }

    #[test]
    fn dismiss_resets_scan_dedup() {
        let mut controller = CompletionController::new();
        controller.update("src/");
        let generation = controller.generation;

        controller.dismiss();
        controller.update("src/");
        assert_eq!(controller.generation, generation + 1);
    }

    #[test]
    fn bare_token_matches_substring_of_nested_paths() {
        let mut controller = CompletionController::new();
        controller.update("popup");
        controller.inject_items(vec![
            entry("Cargo.toml", false),
            entry("crates/agent", true),
            entry("crates/agent/src/agent.rs", false),
            entry("crates/alan/src/views/components/popup.rs", false),
            entry("crates/alan/src/views/components/header.rs", false),
        ]);

        let names: Vec<_> = controller
            .state()
            .unwrap()
            .items
            .iter()
            .map(|e| e.path.clone())
            .collect();
        assert_eq!(names, vec!["crates/alan/src/views/components/popup.rs"]);
    }

    #[test]
    fn bare_token_shows_directory_and_its_contents() {
        let mut controller = CompletionController::new();
        controller.update("agent");
        controller.inject_items(vec![
            entry("Cargo.toml", false),
            entry("crates/agent", true),
            entry("crates/agent/src", true),
            entry("crates/agent/src/agent.rs", false),
            entry("crates/alan/src/main.rs", false),
        ]);

        let names: Vec<_> = controller
            .state()
            .unwrap()
            .items
            .iter()
            .map(|e| e.path.clone())
            .collect();
        assert_eq!(
            names,
            vec![
                "crates/agent",
                "crates/agent/src",
                "crates/agent/src/agent.rs",
            ]
        );
    }

    #[test]
    fn bare_token_filename_prefix_match_ranks_with_substring() {
        let mut controller = CompletionController::new();
        controller.update("main");
        controller.inject_items(vec![
            entry("domain.rs", false),
            entry("crates/alan/src/main.rs", false),
        ]);

        // Substring matching is intentional ("domain.rs" contains "main");
        // ordering follows the scan's shallow-first sort.
        let names: Vec<_> = controller
            .state()
            .unwrap()
            .items
            .iter()
            .map(|e| e.path.clone())
            .collect();
        assert_eq!(names, vec!["domain.rs", "crates/alan/src/main.rs"]);
    }

    #[test]
    fn recursive_scan_lists_nested_entries_with_relative_paths() {
        let root = unique_temp_dir("recursive");
        fs::create_dir_all(root.join("crates/alan/src/views")).unwrap();
        fs::create_dir_all(root.join("crates/agent/src")).unwrap();
        fs::write(root.join("crates/alan/src/views/popup.rs"), "").unwrap();
        fs::write(root.join("crates/alan/src/main.rs"), "").unwrap();
        fs::write(root.join("crates/agent/src/lib.rs"), "").unwrap();
        fs::create_dir(root.join("target")).unwrap();

        let entries = scan_dir(&root, true, "").unwrap();
        let paths: Vec<_> = entries.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"crates/alan/src/views"));
        assert!(paths.contains(&"crates/alan/src/views/popup.rs"));
        assert!(paths.contains(&"crates/agent/src/lib.rs"));
        // Junk dirs are excluded everywhere in the tree.
        assert!(!paths.iter().any(|p| p.contains("target")));
        // All paths are relative to the scan root.
        assert!(paths.iter().all(|p| !p.starts_with("./")));

        // A single-level scan of a subdirectory stays flat.
        let flat = scan_dir(&root.join("crates/alan/src"), false, "").unwrap();
        let flat_paths: Vec<_> = flat.iter().map(|e| e.path.as_str()).collect();
        assert!(flat_paths.contains(&"main.rs"));
        assert!(flat_paths.contains(&"views"));
        assert!(!flat_paths.iter().any(|p| p.contains('/')));

        let _ = fs::remove_dir_all(&root);
    }

    /// Regression: the scan retains a bounded candidate set that can be
    /// refiltered for prefixes typed after the scan starts.
    #[test]
    fn scan_truncates_at_scan_limit_before_filtering() {
        let root = unique_temp_dir("trunc");
        // More than SCAN_LIMIT non-matching files to exhaust the old cap.
        let count = SCAN_LIMIT + 50;
        for i in 0..count {
            fs::write(root.join(format!("filler_{i:04}.txt")), "").unwrap();
        }
        // A file whose name matches the bare-token query `popup`, placed at
        // the end so it would be truncated under the old implementation.
        fs::write(root.join("popup_target.rs"), "").unwrap();

        let entries = scan_dir(&root, true, "popup").unwrap();
        assert!(entries.iter().any(|e| e.path == "popup_target.rs"));
        // The result set is bounded by the candidate budget.
        assert!(entries.len() <= CANDIDATE_LIMIT);

        let _ = fs::remove_dir_all(&root);
    }

    /// Regression: a scan whose epoch has been superseded bails instead of
    /// producing results. `spawn_scan` bumps the shared epoch on every new
    /// request and passes the per-task epoch to `scan_dir_with`; this keeps
    /// a superseded traversal from finishing and delivering a stale result.
    #[test]
    fn scan_bails_when_epoch_superseded() {
        let root = unique_temp_dir("cancel");
        fs::create_dir_all(root.join("deeply/nested/path")).unwrap();
        fs::write(root.join("deeply/nested/path/match.rs"), "").unwrap();
        fs::write(root.join("keep.rs"), "").unwrap();

        // A token already bumped past this task's epoch: the traversal sees
        // a stale epoch at its first directory and returns immediately.
        let stale: ScanCancel = Arc::new(AtomicU64::new(2));
        let entries = scan_dir_with(&root, true, &stale, 1).unwrap();
        assert!(entries.is_empty());

        // A matching epoch still scans normally.
        let fresh: ScanCancel = Arc::new(AtomicU64::new(1));
        let entries = scan_dir_with(&root, true, &fresh, 1).unwrap();
        assert!(entries.iter().any(|e| e.path == "keep.rs"));
        assert!(
            entries
                .iter()
                .any(|e| e.path == "deeply/nested/path/match.rs")
        );

        let _ = fs::remove_dir_all(&root);
    }
}
