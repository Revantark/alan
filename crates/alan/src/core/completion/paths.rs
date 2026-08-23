//! File-path completion.
//!
//! Typing `@` offers files and folders from one in-memory index of the
//! workspace, in which directories carry a trailing `/`. Scans run on the
//! blocking thread pool and land through a channel drained by
//! [`Paths::poll`], so the index is served stale rather than waited on.

use super::{
    Backend, CompletionBackend, CompletionItem, CompletionRequest, CompletionResult,
    CompletionStatus, ranked_items,
};
use crate::core::Poll;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::watch::{self, Receiver, Sender};

/// Maximum entries retained by one scan.
const CANDIDATE_LIMIT: usize = 5_000;
/// Maximum filesystem entries visited by one scan.
const VISIT_LIMIT: usize = 20_000;
/// Maximum recursive depth.
const MAX_SCAN_DEPTH: usize = 32;
/// Directories excluded from scans regardless of prefix.
const SKIPPED_DIRS: &[&str] = &[".git", "target", "node_modules"];

type ScanResults = io::Result<Vec<String>>;

/// Token shared with a scan task. Bumping it asks the traversal to bail early.
/// `Arc<AtomicU64>` is the cheapest cancellation primitive available on the
/// blocking thread pool: no `JoinHandle` polling, no runtime blocking.
type ScanCancel = Arc<AtomicU64>;

pub struct Paths {
    /// The whole workspace. Directories end in `/`.
    index: Vec<String>,
    status: CompletionStatus,
    root: PathBuf,
    /// Staleness stamp bumped on every new scan request.
    generation: u64,
    /// Epoch token shared with a scan task so it can bail early.
    cancel_epoch: ScanCancel,
    tx: Sender<Option<(u64, ScanResults)>>,
    rx: Receiver<Option<(u64, ScanResults)>>,
}

impl Paths {
    pub fn new() -> Self {
        let root = std::env::current_dir()
            .ok()
            .and_then(|path| path.canonicalize().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let mut paths = Self::empty(root);
        paths.refresh();
        paths
    }

    fn empty(root: PathBuf) -> Self {
        let (tx, rx) = watch::channel(None);
        Self {
            index: Vec::new(),
            status: CompletionStatus::Loading,
            root,
            generation: 0,
            cancel_epoch: Arc::new(AtomicU64::new(1)),
            tx,
            rx,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_index(index: Vec<String>) -> Self {
        let mut paths = Self::empty(PathBuf::from("."));
        paths.index = index;
        paths.status = CompletionStatus::Ready;
        paths
    }
}

impl CompletionBackend for Paths {
    fn complete(&self, request: &CompletionRequest) -> Option<CompletionResult> {
        let range = at_token(&request.line, request.cursor)?;
        let pattern = &request.line[range.clone()];
        Some(CompletionResult {
            backend: Backend::FilePath,
            range,
            status: self.status.clone(),
            items: ranked_items(pattern, &self.index, |_, path| CompletionItem {
                display: path.to_owned(),
                replacement: path.to_owned(),
                description: None,
                stay_open: path.ends_with('/'),
            }),
        })
    }

    /// The previous index stays visible until the new one lands.
    fn refresh(&mut self) {
        self.generation += 1;
        self.cancel_epoch.fetch_add(1, Ordering::Release);
        if self.index.is_empty() {
            self.status = CompletionStatus::Loading;
        }

        let generation = self.generation;
        let epoch = self.cancel_epoch.load(Ordering::Acquire);
        let cancel = self.cancel_epoch.clone();
        let root = self.root.clone();
        let tx = self.tx.clone();
        let scan = move || {
            let results = scan_dir(&root, &cancel, epoch);
            let _ = tx.send(Some((generation, results)));
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn_blocking(scan);
            }
            Err(_) => scan(),
        }
    }

    fn poll(&mut self) -> Poll {
        if !self.rx.has_changed().unwrap_or(false) {
            return Poll::Idle;
        }
        let results = {
            let received = self.rx.borrow_and_update();
            let Some((generation, results)) = received.as_ref() else {
                return Poll::Idle;
            };
            if *generation != self.generation {
                return Poll::Idle;
            }
            match results {
                Ok(index) => Ok(index.clone()),
                Err(error) => Err(io::Error::new(error.kind(), error.to_string())),
            }
        };
        match results {
            Ok(index) => {
                self.index = index;
                self.status = CompletionStatus::Ready;
            }
            Err(error) => self.status = CompletionStatus::Error(completion_error(&error)),
        }
        Poll::Changed
    }
}

/// Bytes of the `@` token under `cursor`, excluding the `@` so it survives
/// the replacement. A mention can sit anywhere in a prompt.
fn at_token(line: &str, cursor: usize) -> Option<std::ops::Range<usize>> {
    let cursor = cursor.min(line.len());
    let start = line[..cursor]
        .char_indices()
        .rev()
        .take_while(|&(_, character)| !character.is_whitespace())
        .map(|(index, _)| index)
        .last()?;
    let end = line[cursor..]
        .find(char::is_whitespace)
        .map_or(line.len(), |index| cursor + index);
    line[start..end].starts_with('@').then_some(start + 1..end)
}

/// Walk `root`, returning workspace-relative paths with `/` on directories.
///
/// Cooperative: the traversal bails as soon as `cancel` no longer equals
/// `epoch`.
fn scan_dir(root: &Path, cancel: &ScanCancel, epoch: u64) -> ScanResults {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        // `ignore` handles hidden files and .ignore/.gitignore files. Keep
        // these application-level exclusions in addition to those filters.
        .standard_filters(true)
        .follow_links(false)
        .min_depth(Some(1))
        .max_depth(Some(MAX_SCAN_DEPTH))
        .filter_entry(|entry| entry.depth() == 0 || !is_skipped_name(entry.file_name()));

    let mut index = Vec::new();
    for (visited, result) in builder.build().enumerate() {
        if cancel.load(Ordering::Acquire) != epoch || visited >= VISIT_LIMIT {
            break;
        }
        let entry = result.map_err(ignore_error)?;

        let Some(file_type) = entry.file_type() else {
            continue;
        };
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        let mut path = relative_path(relative);
        if file_type.is_dir() {
            path.push('/');
        }
        index.push(path);
    }
    index.sort_by(|a, b| sort_paths(a, b));
    index.truncate(CANDIDATE_LIMIT);
    Ok(index)
}

/// The order shown before anything is typed. Any pattern overrides it.
fn sort_paths(a: &str, b: &str) -> std::cmp::Ordering {
    fn depth(path: &str) -> usize {
        path.trim_end_matches('/').matches('/').count()
    }
    depth(a)
        .cmp(&depth(b))
        .then_with(|| b.ends_with('/').cmp(&a.ends_with('/')))
        .then_with(|| a.cmp(b))
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

    fn scan(root: &Path) -> ScanResults {
        scan_dir(root, &Arc::new(AtomicU64::new(1)), 1)
    }

    #[test]
    fn scan_sorts_dirs_first_and_skips_junk() {
        let root = unique_temp_dir("scan");
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir(root.join("target")).unwrap();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("zeta.txt"), "").unwrap();
        fs::write(root.join("alpha.txt"), "").unwrap();

        assert_eq!(scan(&root).unwrap(), ["src/", "alpha.txt", "zeta.txt"]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_lists_nested_entries_with_relative_paths() {
        let root = unique_temp_dir("recursive");
        fs::create_dir_all(root.join("crates/alan/src/views")).unwrap();
        fs::create_dir_all(root.join("crates/agent/src")).unwrap();
        fs::write(root.join("crates/alan/src/views/popup.rs"), "").unwrap();
        fs::write(root.join("crates/agent/src/lib.rs"), "").unwrap();
        fs::create_dir(root.join("target")).unwrap();

        let index = scan(&root).unwrap();

        assert!(index.contains(&"crates/alan/src/views/".to_owned()));
        assert!(index.contains(&"crates/alan/src/views/popup.rs".to_owned()));
        assert!(index.contains(&"crates/agent/src/lib.rs".to_owned()));
        // Junk dirs are excluded everywhere in the tree.
        assert!(!index.iter().any(|path| path.contains("target")));
        // All paths are relative to the scan root.
        assert!(!index.iter().any(|path| path.starts_with("./")));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_directory_shows_minimal_error() {
        let root = unique_temp_dir("missing");
        let error = scan(&root.join("nope")).unwrap_err();

        assert_eq!(completion_error(&error), "directory not found");

        let _ = fs::remove_dir_all(root);
    }

    /// A superseded traversal cannot deliver: it bails at the next entry.
    #[test]
    fn scan_bails_when_epoch_superseded() {
        let root = unique_temp_dir("cancel");
        fs::create_dir_all(root.join("deeply/nested")).unwrap();
        fs::write(root.join("keep.rs"), "").unwrap();

        let stale: ScanCancel = Arc::new(AtomicU64::new(2));
        assert!(scan_dir(&root, &stale, 1).unwrap().is_empty());

        assert!(scan(&root).unwrap().contains(&"keep.rs".to_owned()));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn poll_drops_stale_generations() {
        let mut paths = Paths::empty(PathBuf::from("."));
        paths.generation = 7;

        paths
            .tx
            .send(Some((6, Ok(vec!["stale.txt".into()]))))
            .unwrap();
        paths
            .tx
            .send(Some((7, Ok(vec!["fresh.txt".into()]))))
            .unwrap();

        assert_eq!(paths.poll(), Poll::Changed);
        assert_eq!(paths.index, ["fresh.txt"]);
        assert_eq!(paths.poll(), Poll::Idle);
    }

    #[test]
    fn refresh_keeps_serving_the_previous_index() {
        let mut paths = Paths::with_index(vec!["src/main.rs".into()]);
        paths.refresh();

        assert_eq!(paths.index, ["src/main.rs"]);
        assert_eq!(paths.status, CompletionStatus::Ready);
    }

    fn request(line: &str, cursor: usize) -> CompletionRequest {
        CompletionRequest {
            line: line.to_owned(),
            cursor,
        }
    }

    #[test]
    fn answers_only_for_an_at_token() {
        let paths = Paths::with_index(Vec::new());

        // The `@` sits at byte 8 and is deliberately outside the range.
        let result = paths.complete(&request("explain @mai", 12)).unwrap();
        assert_eq!(result.range, 9..12);

        assert!(paths.complete(&request("explain this", 12)).is_none());
        assert!(paths.complete(&request("", 0)).is_none());
    }

    #[test]
    fn directories_are_marked_by_a_trailing_slash() {
        let paths = Paths::with_index(vec!["src/".into(), "main.rs".into()]);
        let items = paths.complete(&request("@", 1)).unwrap().items;

        assert_eq!(items[0].display, "src/");
        assert!(items[0].stay_open);
        assert!(!items[1].stay_open);
    }

    #[test]
    fn the_whole_token_is_replaced_from_inside_it() {
        let paths = Paths::with_index(vec!["foobar".into()]);
        let result = paths.complete(&request("@foo", 3)).unwrap();

        assert_eq!(result.range, 1..4);
        assert_eq!(result.items[0].replacement, "foobar");
    }
}
