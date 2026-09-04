use std::borrow::Cow;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::fs::{self, File};

/// Errors from the raw JSONL store.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("failed to create directory {}: {source}", dir.display())]
    CreateDir {
        dir: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to create file {}: {source}", path.display())]
    CreateFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("file already exists: {0}")]
    AlreadyExists(PathBuf),
    #[error("failed to open file {}: {source}", path.display())]
    OpenFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write to file {}: {source}", path.display())]
    WriteFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read file {}: {source}", path.display())]
    ReadFile {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read directory {}: {source}", path.display())]
    ReadDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to set permissions on {}: {source}", path.display())]
    SetPermissions {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("file not found: {0}")]
    NotFound(PathBuf),
    #[error("file is locked by another writer: {0}")]
    Locked(PathBuf),
}

/// Stateless append-only JSONL store.
///
/// Each method operates on a single file identified by an explicit path.
/// The struct carries no state — it exists to group the three core
/// operations and keep them behind a single, testable type.
pub(crate) struct JsonlStore;

impl JsonlStore {
    /// Exclusively create the file at `file_path` and write `first_line`
    /// as its first record.
    ///
    /// The parent directory must already exist. Returns
    /// [`StoreError::AlreadyExists`] if the file already exists; the
    /// existing file is never touched.
    pub(crate) async fn create(file_path: &Path, first_line: &str) -> Result<(), StoreError> {
        // `create_new` guarantees an existing file is never overwritten.
        drop(File::create_new(file_path).await.map_err(|source| {
            if source.kind() == ErrorKind::AlreadyExists {
                StoreError::AlreadyExists(file_path.to_path_buf())
            } else {
                StoreError::CreateFile {
                    path: file_path.to_path_buf(),
                    source,
                }
            }
        })?);
        if let Err(error) = set_permissions(file_path, false).await {
            remove_created_file(file_path).await;
            return Err(error);
        }
        if let Err(error) = Self::append(file_path, first_line).await {
            remove_created_file(file_path).await;
            return Err(error);
        }
        Ok(())
    }

    /// Append one newline-terminated record under an exclusive advisory lock.
    ///
    /// The lock is held on the session file itself. This avoids stale sidecar
    /// lock files and lets the operating system release the lock if a writer
    /// exits unexpectedly.
    pub(crate) async fn append(file_path: &Path, line: &str) -> Result<(), StoreError> {
        let path = file_path.to_path_buf();
        let line = line.to_owned();
        tokio::task::spawn_blocking(move || append_locked(&path, &line))
            .await
            .map_err(|source| StoreError::OpenFile {
                path: file_path.to_path_buf(),
                source: std::io::Error::other(source),
            })?
    }

    /// Read the whole file at `file_path` as text.
    pub(crate) async fn read(file_path: &Path) -> Result<String, StoreError> {
        fs::read_to_string(file_path).await.map_err(|source| {
            if source.kind() == ErrorKind::NotFound {
                StoreError::NotFound(file_path.to_path_buf())
            } else {
                StoreError::ReadFile {
                    path: file_path.to_path_buf(),
                    source,
                }
            }
        })
    }
}

fn line_with_newline(line: &str) -> Cow<'_, str> {
    if line.ends_with('\n') {
        Cow::Borrowed(line)
    } else {
        Cow::Owned(format!("{line}\n"))
    }
}

async fn remove_created_file(path: &Path) {
    let _ = fs::remove_file(path).await;
}

pub(crate) const JSONL_EXTENSION: &str = "jsonl";

/// Set restrictive Unix permissions on `path`.
///
/// Directories get `0o700`, files get `0o600`. On non-Unix platforms this is
/// a no-op.
pub(crate) async fn set_permissions(path: &Path, is_dir: bool) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;

        let mode = if is_dir { 0o700 } else { 0o600 };
        fs::set_permissions(path, Permissions::from_mode(mode))
            .await
            .map_err(|source| StoreError::SetPermissions {
                path: path.to_path_buf(),
                source,
            })
    }

    #[cfg(not(unix))]
    {
        let _ = (path, is_dir);
        Ok(())
    }
}

/// Yield `(line, is_complete)` pairs from raw file contents.
///
/// An incomplete final line (no trailing newline, i.e. a crash-truncated
/// append) is yielded with `is_complete == false`; every other line is
/// complete. Callers decide whether to surface malformed complete lines.
pub(crate) fn split_complete_lines(content: &str) -> impl Iterator<Item = (&str, bool)> {
    let (body, truncated_tail) = if content.is_empty() {
        ("", None)
    } else {
        match content.strip_suffix('\n') {
            Some(body) => (body, None),
            None => match content.rsplit_once('\n') {
                Some((body, tail)) => (body, Some(tail)),
                None => ("", Some(content)),
            },
        }
    };

    body.lines()
        .map(|line| (line, true))
        .chain(truncated_tail.map(|tail| (tail, false)))
}

fn append_locked(path: &Path, line: &str) -> Result<(), StoreError> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .open(path)
        .map_err(|source| {
            if source.kind() == ErrorKind::NotFound {
                StoreError::NotFound(path.to_path_buf())
            } else {
                StoreError::OpenFile {
                    path: path.to_path_buf(),
                    source,
                }
            }
        })?;

    fs2::FileExt::try_lock_exclusive(&file).map_err(|source| {
        if source.kind() == ErrorKind::WouldBlock
            || source.raw_os_error() == fs2::lock_contended_error().raw_os_error()
        {
            StoreError::Locked(path.to_path_buf())
        } else {
            StoreError::OpenFile {
                path: path.to_path_buf(),
                source,
            }
        }
    })?;

    let result = write_and_sync(&mut file, path, line);
    let _ = fs2::FileExt::unlock(&file);
    result
}

fn write_and_sync(file: &mut std::fs::File, path: &Path, line: &str) -> Result<(), StoreError> {
    let line = line_with_newline(line);
    file.write_all(line.as_bytes())
        .map_err(|source| StoreError::WriteFile {
            path: path.to_path_buf(),
            source,
        })?;
    file.flush().map_err(|source| StoreError::WriteFile {
        path: path.to_path_buf(),
        source,
    })?;
    file.sync_data().map_err(|source| StoreError::WriteFile {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("alan-store-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp root");
        dir
    }

    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    fn file(root: &Path, key: &str, name: &str) -> PathBuf {
        let dir = root.join(key);
        std::fs::create_dir_all(&dir).expect("create key dir");
        dir.join(format!("{name}.{JSONL_EXTENSION}"))
    }

    #[tokio::test]
    async fn create_writes_first_line_and_refuses_overwrite() {
        let root = temp_root("create");
        let path = file(&root, "key-a", "one");

        JsonlStore::create(&path, "{\"first\":true}\n")
            .await
            .expect("create");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"first\":true}\n"
        );

        let err = JsonlStore::create(&path, "{\"second\":true}\n")
            .await
            .expect_err("second create refused");
        assert!(matches!(err, StoreError::AlreadyExists(_)));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"first\":true}\n"
        );
        cleanup(&root);
    }

    #[tokio::test]
    async fn append_produces_newline_terminated_records_in_order() {
        let root = temp_root("append");
        let path = file(&root, "k", "f");

        JsonlStore::create(&path, "a\n").await.expect("create");
        JsonlStore::append(&path, "b\n").await.expect("append b");
        JsonlStore::append(&path, "c\n").await.expect("append c");
        assert_eq!(JsonlStore::read(&path).await.unwrap(), "a\nb\nc\n");
        cleanup(&root);
    }

    #[test]
    fn split_lines_flags_truncated_tail_only() {
        let complete: Vec<_> = split_complete_lines("a\nb\n").collect();
        assert_eq!(complete, vec![("a", true), ("b", true)]);
        let truncated: Vec<_> = split_complete_lines("a\nb").collect();
        assert_eq!(truncated, vec![("a", true), ("b", false)]);
        let none: Vec<_> = split_complete_lines("").collect();
        assert!(none.is_empty());
        let only_truncated: Vec<_> = split_complete_lines("abc").collect();
        assert_eq!(only_truncated, vec![("abc", false)]);
    }

    #[tokio::test]
    async fn read_missing_and_append_missing_are_not_found() {
        let root = temp_root("missing");
        let path = file(&root, "nope", "f");

        let err = JsonlStore::read(&path).await.expect_err("read missing");
        assert!(matches!(err, StoreError::NotFound(_)));
        let err = JsonlStore::append(&path, "x\n")
            .await
            .expect_err("append missing");
        assert!(matches!(err, StoreError::NotFound(_)));
        cleanup(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_file_permissions_are_restrictive() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("perms");
        let path = file(&root, "k", "f");

        JsonlStore::create(&path, "x\n").await.expect("create");
        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(file_mode & 0o777, 0o600, "file mode");
        cleanup(&root);
    }

    #[tokio::test]
    async fn locked_while_session_file_is_held() {
        let root = temp_root("locked");
        let path = file(&root, "k", "f");

        JsonlStore::create(&path, "x\n").await.expect("create");

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("open session");
        fs2::FileExt::try_lock_exclusive(&file).expect("acquire lock");
        let err = JsonlStore::append(&path, "y\n")
            .await
            .expect_err("append blocked");
        assert!(matches!(err, StoreError::Locked(_)));

        fs2::FileExt::unlock(&file).expect("unlock session");
        drop(file);
        JsonlStore::append(&path, "y\n")
            .await
            .expect("append after unlock");
        assert_eq!(JsonlStore::read(&path).await.unwrap(), "x\ny\n");
        cleanup(&root);
    }
}
