use std::path::{Component, Path, PathBuf};

pub(crate) fn fnv1a64(data: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET_BASIS;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

pub(crate) fn pwd_key(pwd: &Path) -> String {
    format!("{:016x}", fnv1a64(pwd.to_string_lossy().as_bytes()))
}

/// Normalize a pwd to an absolute path.
pub(crate) fn normalize_pwd(pwd: PathBuf) -> Result<PathBuf, std::io::Error> {
    if let Ok(canonical) = pwd.canonicalize() {
        return Ok(canonical);
    }

    let absolute = if pwd.is_absolute() {
        pwd
    } else {
        std::env::current_dir()?.join(pwd)
    };
    Ok(normalize_components(absolute))
}

/// Lexically resolve `.` and `..` components without touching the filesystem.
fn normalize_components(path: PathBuf) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a64_matches_known_vectors_and_is_stable() {
        // Standard FNV-1a 64 test vectors.
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x85944171f73967e8);

        // Deterministic across repeated calls within and across runs.
        assert_eq!(fnv1a64(b"/tmp/project"), fnv1a64(b"/tmp/project"));
    }

    #[test]
    fn pwd_key_is_hex_and_collision_free_for_distinct_paths() {
        let a = pwd_key(Path::new("/tmp/a"));
        let b = pwd_key(Path::new("/tmp/b"));

        assert_ne!(a, b);
        assert_eq!(a.len(), 16);
        assert!(a.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(pwd_key(Path::new("/tmp/a")), a, "stable per input");
    }

    #[test]
    fn normalize_resolves_dots_and_parent_dirs_lexically() {
        assert_eq!(
            normalize_components(PathBuf::from("/tmp/./project/../project/src")),
            PathBuf::from("/tmp/project/src")
        );
        assert_eq!(
            normalize_pwd(PathBuf::from("/tmp/./p/../p")).expect("normalize"),
            PathBuf::from("/tmp/p")
        );
    }

    #[test]
    fn normalize_canonicalizes_existing_paths() {
        // An existing temp dir canonicalizes (e.g. /tmp -> /private/tmp on macOS).
        let dir = std::env::temp_dir();
        let normalized = normalize_pwd(dir.clone()).expect("normalize existing");
        assert!(normalized.is_absolute());
        let canonicalized = dir.canonicalize().expect("canonicalize");
        assert_eq!(normalized, canonicalized);
    }

    #[test]
    fn normalize_makes_relative_paths_absolute_without_existing_dir() {
        let relative = PathBuf::from("does/not/exist/../exist");
        let normalized = normalize_pwd(relative.clone()).expect("normalize missing dir");
        assert!(normalized.is_absolute());
        assert!(normalized.ends_with("does/not/exist"));
        assert!(!normalized.to_string_lossy().contains(".."));
    }

    #[test]
    fn different_pwds_hash_to_different_keys() {
        assert_ne!(
            pwd_key(normalize_pwd(PathBuf::from("/w/a")).unwrap().as_path()),
            pwd_key(normalize_pwd(PathBuf::from("/w/b")).unwrap().as_path())
        );
    }
}
