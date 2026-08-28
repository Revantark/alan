//! File-path completion.
//!
//! Typing `@` offers files and folders from one in-memory index of the
//! workspace, in which directories carry a trailing `/`. The scan runs on the
//! blocking thread pool and its result is collected by [`Paths::poll`], so the
//! index is served stale rather than waited on.

use super::{
    CompletionBackend, CompletionItem, CompletionRequest, CompletionResult, CompletionStatus,
    ranked_items,
};
use crate::core::Poll;
use futures_util::FutureExt;
use std::io;
use std::path::{Component, Path, PathBuf};
use tokio::task::JoinHandle;

/// How many paths the index holds, and so how many are searchable at all.
/// A backstop, not a policy: past this the tail of the walk is missing and
/// those files can never be completed, so raise it if real workspaces reach it.
const MAX_INDEXED_PATHS: usize = 10_000;

/// How deep the walk goes. Real source trees bottom out around seven.
const MAX_PATH_DEPTH: usize = 10;

/// Directories excluded from scans regardless of prefix.
const SKIPPED_DIRS: &[&str] = &[".git", "target", "node_modules"];

type ScanResults = io::Result<Vec<String>>;

pub struct Paths {
    /// The whole workspace. Directories end in `/`.
    index: Vec<String>,
    status: CompletionStatus,
    root: PathBuf,
    /// The in-flight scan, which is also where its result arrives. At most one
    /// runs at a time, so a delivered result is always a complete one.
    scan: Option<JoinHandle<ScanResults>>,
}

impl Paths {
    /// Completions are relative to `root`. The index starts empty and the
    /// first scan is driven by [`CompletionBackend::refresh`] when the popup
    /// opens, so constructing this touches no filesystem.
    pub fn new(root: PathBuf) -> Self {
        Self {
            index: Vec::new(),
            status: CompletionStatus::Loading,
            root,
            scan: None,
        }
    }

    /// A ready index without a scan. Lives here rather than in a test module
    /// because `index` and `status` are private to this one.
    #[cfg(test)]
    pub(crate) fn with_index(index: Vec<String>) -> Self {
        Self {
            index,
            status: CompletionStatus::Ready,
            ..Self::new(PathBuf::from("."))
        }
    }

    /// A panicked scan reports finished, so a wedged backend recovers on the
    /// next refresh rather than never scanning again.
    fn scanning(&self) -> bool {
        self.scan.as_ref().is_some_and(|scan| !scan.is_finished())
    }
}

impl Default for Paths {
    /// Rooted at the working directory, which is the workspace in practice.
    /// Canonicalising it is the one filesystem call made outside a scan.
    fn default() -> Self {
        Self::new(
            std::env::current_dir()
                .ok()
                .and_then(|path| path.canonicalize().ok())
                .unwrap_or_else(|| PathBuf::from(".")),
        )
    }
}

impl CompletionBackend for Paths {
    fn trigger(&self) -> char {
        '@'
    }

    /// Always answers: the trigger was the only condition, and the controller
    /// has already checked it.
    fn complete(&self, request: &CompletionRequest) -> Option<CompletionResult> {
        Some(CompletionResult {
            range: request.range.clone(),
            status: self.status.clone(),
            items: ranked_items(&request.pattern, &self.index, |path| CompletionItem {
                display: path.to_owned(),
                replacement: path.to_owned(),
            }),
        })
    }

    /// The previous index stays visible until the new one lands. A scan already
    /// under way is left to finish rather than restarted, and without a runtime
    /// to scan on the current index simply stands.
    fn refresh(&mut self) {
        if self.scanning() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };

        if self.index.is_empty() {
            self.status = CompletionStatus::Loading;
        }

        let root = self.root.clone();
        self.scan = Some(runtime.spawn_blocking(move || scan_dir(&root)));
    }

    fn poll(&mut self) -> Poll {
        let Some(mut scan) = self.scan.take() else {
            return Poll::Idle;
        };
        let Some(finished) = (&mut scan).now_or_never() else {
            self.scan = Some(scan);
            return Poll::Idle;
        };

        match finished {
            Ok(Ok(index)) => {
                self.index = index;
                self.status = CompletionStatus::Ready;
            }
            Ok(Err(error)) => {
                self.status = CompletionStatus::Error(match error.kind() {
                    io::ErrorKind::NotFound => "directory not found".into(),
                    _ => error.to_string(),
                });
            }
            // Panicked, or cancelled at shutdown: no new data, so nothing to
            // report. An error status would hide a working index, because the
            // popup renders a status message in place of its items.
            Err(_) => return Poll::Idle,
        }
        Poll::Changed
    }
}

/// Walk `root`, returning workspace-relative paths with `/` on directories.
///
/// An unreadable root is fatal, an unreadable entry inside it is not: one
/// permission-denied folder must not cost the workspace its whole index.
fn scan_dir(root: &Path) -> ScanResults {
    root.metadata()?;

    let mut builder = ignore::WalkBuilder::new(root);
    builder
        // `ignore` handles hidden files and .ignore/.gitignore files. Keep
        // these application-level exclusions in addition to those filters.
        .standard_filters(true)
        .follow_links(false)
        .min_depth(Some(1))
        .max_depth(Some(MAX_PATH_DEPTH))
        .filter_entry(|entry| entry.depth() == 0 || !is_skipped_name(entry.file_name()));

    let mut index = Vec::new();
    for result in builder.build() {
        if index.len() >= MAX_INDEXED_PATHS {
            break;
        }
        let Ok(entry) = result else {
            continue;
        };

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
    fn scan_sorts_dirs_first_and_skips_junk() {
        let root = unique_temp_dir("scan");
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir(root.join("target")).unwrap();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("zeta.txt"), "").unwrap();
        fs::write(root.join("alpha.txt"), "").unwrap();

        assert_eq!(scan_dir(&root).unwrap(), ["src/", "alpha.txt", "zeta.txt"]);

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

        let index = scan_dir(&root).unwrap();

        assert!(index.contains(&"crates/alan/src/views/".to_owned()));
        assert!(index.contains(&"crates/alan/src/views/popup.rs".to_owned()));
        assert!(index.contains(&"crates/agent/src/lib.rs".to_owned()));
        // Junk dirs are excluded everywhere in the tree.
        assert!(!index.iter().any(|path| path.contains("target")));
        // All paths are relative to the scan root.
        assert!(!index.iter().any(|path| path.starts_with("./")));

        let _ = fs::remove_dir_all(&root);
    }

    /// Also passes as root, where the folder is readable and simply gets
    /// indexed: either way the readable files survive.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_directory_does_not_empty_the_index() {
        use std::os::unix::fs::PermissionsExt;

        let root = unique_temp_dir("unreadable");
        fs::write(root.join("keep.rs"), "").unwrap();
        let locked = root.join("locked");
        fs::create_dir(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let index = scan_dir(&root).expect("a locked folder is not a fatal error");
        assert!(index.contains(&"keep.rs".to_owned()));

        let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o755));
        let _ = fs::remove_dir_all(&root);
    }

    /// Poll until the in-flight scan lands, as the UI loop does every tick.
    async fn drain(paths: &mut Paths) -> Poll {
        for _ in 0..10_000 {
            let poll = paths.poll();
            if poll != Poll::Idle {
                return poll;
            }
            tokio::task::yield_now().await;
        }
        panic!("the scan never landed");
    }

    #[tokio::test]
    async fn a_missing_root_shows_a_minimal_message() {
        let root = unique_temp_dir("missing");
        let mut paths = Paths::new(root.join("nope"));

        paths.refresh();
        assert_eq!(drain(&mut paths).await, Poll::Changed);

        assert_eq!(
            paths.status,
            CompletionStatus::Error("directory not found".into())
        );

        let _ = fs::remove_dir_all(root);
    }

    /// At most one scan runs at a time, so every delivered result is complete.
    #[tokio::test]
    async fn refresh_leaves_a_running_scan_alone() {
        let mut paths = Paths::new(PathBuf::from("."));
        // A task that never finishes stands in for a scan in flight.
        paths.scan = Some(tokio::spawn(std::future::pending()));
        let running = paths.scan.as_ref().unwrap().id();

        for _ in 0..5 {
            paths.refresh();
        }

        assert_eq!(
            paths.scan.as_ref().unwrap().id(),
            running,
            "a second scan was spawned alongside the running one"
        );
    }

    /// A panicked scan reports finished, so the guard releases instead of
    /// wedging the backend into never scanning again.
    #[tokio::test]
    async fn a_panicked_scan_does_not_wedge_the_guard() {
        let mut paths = Paths::new(PathBuf::from("."));
        paths.scan = Some(tokio::spawn(async { panic!("scan blew up") }));
        // Let the task run and unwind.
        tokio::task::yield_now().await;

        assert!(!paths.scanning());
        assert_eq!(paths.poll(), Poll::Idle);
    }

    #[tokio::test]
    async fn poll_installs_a_delivered_index_once() {
        let mut paths = Paths::new(PathBuf::from("."));
        paths.scan = Some(tokio::spawn(async { Ok(vec!["fresh.txt".to_owned()]) }));

        assert_eq!(drain(&mut paths).await, Poll::Changed);
        assert_eq!(paths.index, ["fresh.txt"]);
        assert_eq!(paths.poll(), Poll::Idle);
    }

    #[tokio::test]
    async fn refresh_keeps_serving_the_previous_index() {
        let root = unique_temp_dir("previous");
        let mut paths = Paths {
            index: vec!["src/main.rs".into()],
            status: CompletionStatus::Ready,
            ..Paths::new(root.clone())
        };

        paths.refresh();

        assert_eq!(paths.index, ["src/main.rs"]);
        assert_eq!(paths.status, CompletionStatus::Ready);

        let _ = fs::remove_dir_all(&root);
    }

    /// The whole chain a scan result travels: typing opens the popup on an
    /// empty index, the walk lands on the blocking pool, and the popup refills
    /// in place.
    #[tokio::test]
    async fn a_scan_reaches_an_open_popup_through_the_controller() {
        use crate::core::completion::CompletionController;

        let root = unique_temp_dir("end-to-end");
        fs::create_dir(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "").unwrap();

        let mut completion = CompletionController::new(vec![Box::new(Paths::new(root.clone()))]);

        completion.sync("@mai", 4);
        assert!(completion.is_open());
        assert_eq!(completion.item_count(), 0);

        for _ in 0..10_000 {
            if completion.poll() == Poll::Changed {
                break;
            }
            tokio::task::yield_now().await;
        }

        assert_eq!(
            completion.item_count(),
            1,
            "the scan never reached the popup"
        );
        assert_eq!(completion.items(0, 1)[0].display, "src/main.rs");
        // Drained exactly once.
        assert_eq!(completion.poll(), Poll::Idle);

        let _ = fs::remove_dir_all(&root);
    }

    fn request(pattern: &str, range: std::ops::Range<usize>) -> CompletionRequest {
        CompletionRequest {
            pattern: pattern.to_owned(),
            range,
        }
    }

    /// Locating the token is the controller's job, so this backend ranks the
    /// pattern it is handed and overwrites exactly the range it came with.
    #[test]
    fn the_handed_range_is_what_gets_replaced() {
        let paths = Paths::with_index(vec!["src/main.rs".into()]);
        let result = paths.complete(&request("mai", 9..12)).unwrap();

        assert_eq!(result.range, 9..12);
        assert_eq!(result.items[0].replacement, "src/main.rs");
    }
}
