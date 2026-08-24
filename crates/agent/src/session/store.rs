use std::path::{Path, PathBuf};
use thiserror::Error;
use tokio::fs::{self, File, OpenOptions};
use tokio::io::AsyncWriteExt;

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
    #[error("file not found: {0}")]
    NotFound(PathBuf),
    #[error("file is locked by another writer: {0}")]
    Locked(PathBuf),
}

/// Append-only JSONL store over a two-level layout:
/// `<root>/<key>/<name>.jsonl`.
pub(crate) struct JsonlStore {
    root: PathBuf,
}

impl JsonlStore {
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Exclusively create `<root>/<key>/<name>.jsonl`, creating the key
    /// directory if needed, and write `first_line` as its first record.
    ///
    /// Returns [`StoreError::AlreadyExists`] if the file already exists; the
    /// existing file is never touched.
    pub(crate) async fn create_file(
        &self,
        key: &str,
        name: &str,
        first_line: &str,
    ) -> Result<(), StoreError> {
        let dir = self.key_dir(key);
        fs::create_dir_all(&dir)
            .await
            .map_err(|source| StoreError::CreateDir {
                dir: dir.clone(),
                source,
            })?;
        set_permissions(&dir, true).await;

        let path = self.file_path(key, name);
        // `create_new` guarantees an existing file is never overwritten.
        let mut file = File::create_new(&path).await.map_err(|source| {
            if source.kind() == std::io::ErrorKind::AlreadyExists {
                StoreError::AlreadyExists(path.clone())
            } else {
                StoreError::CreateFile {
                    path: path.clone(),
                    source,
                }
            }
        })?;
        set_permissions(&path, false).await;

        Self::write_line(&path, &mut file, first_line).await
    }

    /// Append one newline-terminated record under an exclusive sidecar lock.
    pub(crate) async fn append(&self, key: &str, name: &str, line: &str) -> Result<(), StoreError> {
        let path = self.file_path(key, name);
        if !path.is_file() {
            return Err(StoreError::NotFound(path));
        }

        let _lock = FileLock::acquire(self.lock_path(key, name)).await?;

        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .await
            .map_err(|source| StoreError::OpenFile {
                path: path.clone(),
                source,
            })?;
        Self::write_line(&path, &mut file, line).await
    }

    /// Read the whole file as text.
    pub(crate) async fn read(&self, key: &str, name: &str) -> Result<String, StoreError> {
        let path = self.file_path(key, name);
        if !path.is_file() {
            return Err(StoreError::NotFound(path));
        }
        fs::read_to_string(&path)
            .await
            .map_err(|source| StoreError::ReadFile { path, source })
    }

    /// Paths of every `.jsonl` file stored under `key`, unsorted.
    ///
    /// Missing directories yield an empty list: no files yet.
    pub(crate) async fn files(&self, key: &str) -> Result<Vec<PathBuf>, StoreError> {
        let dir = self.key_dir(key);
        let read_dir = match fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => return Err(StoreError::ReadFile { path: dir, source }),
        };

        let mut paths = Vec::new();
        let mut entries = read_dir;
        loop {
            match entries.next_entry().await {
                Ok(Some(entry)) => {
                    let path = entry.path();
                    if path.extension().and_then(|ext| ext.to_str()) == Some(JSONL_EXTENSION) {
                        paths.push(path);
                    }
                }
                Ok(None) => break,
                Err(source) => {
                    return Err(StoreError::ReadFile {
                        path: dir.clone(),
                        source,
                    });
                }
            }
        }
        Ok(paths)
    }

    /// Directory holding all files for `key`.
    pub(crate) fn key_dir(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }

    /// Full path of one stored file.
    pub(crate) fn file_path(&self, key: &str, name: &str) -> PathBuf {
        self.key_dir(key).join(format!("{name}.{JSONL_EXTENSION}"))
    }

    /// Sidecar lock path used by [`JsonlStore::append`].
    pub(crate) fn lock_path(&self, key: &str, name: &str) -> PathBuf {
        self.key_dir(key)
            .join(format!("{name}.{JSONL_EXTENSION}{LOCK_SUFFIX}"))
    }

    async fn write_line(path: &Path, file: &mut File, line: &str) -> Result<(), StoreError> {
        file.write_all(line.as_bytes())
            .await
            .map_err(|source| StoreError::WriteFile {
                path: path.to_path_buf(),
                source,
            })?;
        file.flush().await.map_err(|source| StoreError::WriteFile {
            path: path.to_path_buf(),
            source,
        })
    }
}

pub(crate) const JSONL_EXTENSION: &str = "jsonl";
const LOCK_SUFFIX: &str = ".lock";

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
                // No newline at all: single truncated line.
                None => ("", Some(content)),
            },
        }
    };

    body.lines()
        .map(|line| (line, true))
        .chain(truncated_tail.map(|tail| (tail, false)))
}

/// Exclusive, self-removing sidecar lock guard around appends.
struct FileLock {
    path: PathBuf,
}

impl FileLock {
    async fn acquire(path: PathBuf) -> Result<Self, StoreError> {
        File::create_new(&path).await.map_err(|source| {
            if source.kind() == std::io::ErrorKind::AlreadyExists {
                StoreError::Locked(path.clone())
            } else {
                StoreError::CreateFile {
                    path: path.clone(),
                    source,
                }
            }
        })?;
        Ok(Self { path })
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // Use blocking remove_file in Drop; this is fine because Drop is
        // synchronous and file removal is a fast operation.
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
async fn set_permissions(path: &Path, is_dir: bool) {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;

    let mode = if is_dir { 0o700 } else { 0o600 };
    let _ = fs::set_permissions(path, Permissions::from_mode(mode)).await;
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

    #[tokio::test]
    async fn constructing_store_creates_nothing() {
        let root =
            std::env::temp_dir().join(format!("alan-store-construct-{}", uuid::Uuid::new_v4()));
        let _store = JsonlStore::new(&root);
        assert!(!root.exists(), "store construction must not create files");
    }

    #[tokio::test]
    async fn create_file_writes_first_line_and_refuses_overwrite() {
        let root = temp_root("create");
        let store = JsonlStore::new(&root);

        store
            .create_file("key-a", "one", "{\"first\":true}\n")
            .await
            .expect("create");

        let path = store.file_path("key-a", "one");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"first\":true}\n"
        );

        // Exclusive creation refuses to truncate or touch the existing file.
        let err = store
            .create_file("key-a", "one", "{\"second\":true}\n")
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
    async fn create_file_creates_key_directory() {
        let root = temp_root("mkdir");
        let store = JsonlStore::new(&root);

        store
            .create_file("key-x", "f", "x\n")
            .await
            .expect("create");
        assert!(store.key_dir("key-x").is_dir());
        cleanup(&root);
    }

    #[tokio::test]
    async fn append_produces_newline_terminated_records_in_order() {
        let root = temp_root("append");
        let store = JsonlStore::new(&root);
        store.create_file("k", "f", "a\n").await.expect("create");

        store.append("k", "f", "b\n").await.expect("append b");
        store.append("k", "f", "c\n").await.expect("append c");

        assert_eq!(store.read("k", "f").await.unwrap(), "a\nb\nc\n");
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
        let store = JsonlStore::new(&root);

        let err = store.read("nope", "f").await.expect_err("read missing");
        assert!(matches!(err, StoreError::NotFound(_)));

        let err = store
            .append("nope", "f", "x\n")
            .await
            .expect_err("append missing");
        assert!(matches!(err, StoreError::NotFound(_)));
        cleanup(&root);
    }

    #[tokio::test]
    async fn files_lists_only_jsonl_entries() {
        let root = temp_root("files");
        let store = JsonlStore::new(&root);
        store
            .create_file("k", "one", "1\n")
            .await
            .expect("create one");
        store
            .create_file("k", "two", "2\n")
            .await
            .expect("create two");

        let mut names: Vec<_> = store
            .files("k")
            .await
            .expect("files")
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["one.jsonl", "two.jsonl"]);

        // Lock sidecars are not listed, and unknown keys list empty.
        tokio::fs::write(store.lock_path("k", "one"), "")
            .await
            .unwrap();
        assert_eq!(store.files("k").await.unwrap().len(), 2);
        assert!(store.files("absent").await.unwrap().is_empty());
        cleanup(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_permissions_are_restrictive() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_root("perms");
        let store = JsonlStore::new(&root);
        store.create_file("k", "f", "x\n").await.expect("create");

        let dir_mode = std::fs::metadata(store.key_dir("k"))
            .unwrap()
            .permissions()
            .mode();
        let file_mode = std::fs::metadata(store.file_path("k", "f"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700, "directory mode");
        assert_eq!(file_mode & 0o777, 0o600, "file mode");
        cleanup(&root);
    }

    #[tokio::test]
    async fn locked_while_sidecar_lock_held() {
        let root = temp_root("locked");
        let store = JsonlStore::new(&root);
        store.create_file("k", "f", "x\n").await.expect("create");

        let lock = FileLock::acquire(store.lock_path("k", "f"))
            .await
            .expect("acquire lock");
        let err = store
            .append("k", "f", "y\n")
            .await
            .expect_err("append blocked");
        assert!(matches!(err, StoreError::Locked(_)));

        drop(lock); // Lock removed on drop.
        store
            .append("k", "f", "y\n")
            .await
            .expect("append after unlock");
        assert_eq!(store.read("k", "f").await.unwrap(), "x\ny\n");
        assert!(!store.lock_path("k", "f").exists(), "lock removed on drop");
        cleanup(&root);
    }
}
