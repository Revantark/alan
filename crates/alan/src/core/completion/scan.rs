//! File-path completion.
//!
//! Typing `@` offers files and folders from one in-memory index of the
//! workspace, in which directories carry a trailing `/`. The scan runs on the
//! blocking thread pool and its result is collected by [`Paths::poll`], so the
//! index is served stale rather than waited on.

use std::io;
use std::path::{Component, Path};

/// How many paths the index holds, and so how many are searchable at all.
/// A backstop, not a policy: past this the tail of the walk is missing and
/// those files can never be completed, so raise it if real workspaces reach it.
const MAX_INDEXED_PATHS: usize = 10_000;

/// How deep the walk goes. Real source trees bottom out around seven.
const MAX_PATH_DEPTH: usize = 10;

/// Directories excluded from scans regardless of prefix.
const SKIPPED_DIRS: &[&str] = &[".git", "target", "node_modules"];

type ScanResults = io::Result<Vec<String>>;

/// Walk `root`, returning workspace-relative paths with `/` on directories.
///
/// An unreadable root is fatal, an unreadable entry inside it is not: one
/// permission-denied folder must not cost the workspace its whole index.
pub fn scan_dir(root: &Path) -> ScanResults {
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
